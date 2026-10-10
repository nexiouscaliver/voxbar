//! The companion TLS server: one port serving two things. GET / returns
//! the embedded single-file web client (QR-scanned phones, no app-store
//! install); GET /ws upgrades to a WebSocket carrying TEXT control frames
//! and BINARY 16 kHz f32 audio. TLS is mandatory because browsers gate
//! getUserMedia on secure contexts; the cert is the self-signed one from
//! `cert.rs`, whose fingerprint Settings shows for the phone's first-visit
//! interstitial.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::WebSocketStream;

use super::auth;
use super::cert::CompanionCert;
use super::client_page;
use super::ip;
use super::protocol::{self, ClientFrame, ServerFrame};
use super::CompanionManager;
use tauri::Manager;

/// Keepalive: ping every 10 s, drop the peer after 20 s of silence.
const PING_INTERVAL: Duration = Duration::from_secs(10);
const PONG_DEADLINE: Duration = Duration::from_secs(20);
/// A fresh socket must complete its hello within this window.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

pub struct ServerConfig {
    pub ip: std::net::Ipv4Addr,
    pub port: u16,
    pub token: String,
    pub subnet: String,
}

/// Run the accept loop until the shutdown signal fires. `ready` receives
/// the bind outcome so the enable path can surface a bind error
/// synchronously.
pub async fn run(
    app: tauri::AppHandle,
    cfg: ServerConfig,
    cert: CompanionCert,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    ready: std::sync::mpsc::Sender<Result<(), String>>,
) {
    let bind_addr = SocketAddr::new(IpAddr::V4(cfg.ip), cfg.port);
    let listener = match TcpListener::bind(bind_addr).await {
        Ok(listener) => listener,
        Err(e) => {
            let _ = ready.send(Err(format!("bind {bind_addr} failed: {e}")));
            return;
        }
    };

    let server_config = match rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .and_then(|b| {
        b.with_no_client_auth()
            .with_single_cert(vec![cert.cert], cert.key)
    }) {
        Ok(config) => config,
        Err(e) => {
            let _ = ready.send(Err(format!("TLS config failed: {e}")));
            return;
        }
    };
    let tls = TlsAcceptor::from(Arc::new(server_config));

    if ready.send(Ok(())).is_err() {
        return; // the enable path went away; nothing to serve
    }
    log::info!("companion: listening on {bind_addr}");

    loop {
        let (socket, peer) = tokio::select! {
            _ = shutdown.changed() => break,
            accepted = listener.accept() => match accepted {
                Ok((socket, peer)) => (socket, peer),
                Err(e) => {
                    log::debug!("companion: accept error: {e}");
                    continue;
                }
            },
        };

        // Trust boundary, before any byte of control or audio: private
        // ranges only, and the token's subnet binding (re-pair notice on
        // mismatch via the refused hello).
        let peer_ip = peer.ip();
        if !ip::is_private_lan(&peer_ip) {
            log::warn!("companion: refusing non-private peer {peer_ip}");
            continue;
        }

        let app = app.clone();
        let tls = tls.clone();
        let cfg_token = cfg.token.clone();
        let cfg_subnet = cfg.subnet.clone();
        tauri::async_runtime::spawn(async move {
            let Ok(tls_stream) = tls.accept(socket).await else {
                return; // handshake failure; nothing to say to a non-TLS peer
            };
            handle_connection(app, tls_stream, peer_ip, cfg_token, cfg_subnet).await;
        });
    }

    log::info!("companion: accept loop exited");
}

/// Replay-a-prefix stream: the HTTP head was read before the WebSocket
/// upgrade, so tungstenite re-reads it from here before hitting the socket.
struct PrefixedStream<S> {
    prefix: Vec<u8>,
    pos: usize,
    inner: S,
}

impl<S: AsyncRead + Unpin> AsyncRead for PrefixedStream<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.pos < self.prefix.len() {
            let remaining = &self.prefix[self.pos..];
            let n = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..n]);
            self.pos += n;
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PrefixedStream<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Read the HTTP request head (up to the blank line), bounded to 8 KiB.
async fn read_http_head<S: AsyncRead + Unpin>(
    stream: &mut S,
) -> std::io::Result<Option<(Vec<u8>, String)>> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).position(|w| w == b"\r\n\r\n").is_some() {
            break;
        }
        if buf.len() > 8 * 1024 {
            return Ok(None);
        }
    }
    let head = String::from_utf8_lossy(&buf).into_owned();
    Ok(Some((buf, head)))
}

async fn handle_connection<S: AsyncRead + AsyncWrite + Unpin>(
    app: tauri::AppHandle,
    mut stream: S,
    peer: IpAddr,
    token: String,
    subnet: String,
) {
    let Some((raw_head, head)) = read_http_head(&mut stream).await.unwrap_or(None) else {
        return;
    };
    let request_line = head.lines().next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    let upgrade = head
        .lines()
        .any(|l| l.to_ascii_lowercase().starts_with("upgrade: websocket"));

    if path == "/ws" && upgrade {
        let replay = PrefixedStream {
            prefix: raw_head,
            pos: 0,
            inner: stream,
        };
        match tokio_tungstenite::accept_hdr_async(replay, |_: &Request, response: Response| {
            Ok(response)
        })
        .await
        {
            Ok(ws) => {
                serve_websocket(app, ws, peer, token, subnet).await;
            }
            Err(e) => log::debug!("companion: websocket upgrade failed: {e}"),
        }
        return;
    }

    // Everything else gets the client page (browsers ask for "/" after the
    // QR scan; the hash fragment never reaches the server).
    if method == "GET" {
        let body = client_page::render_client_page();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.flush().await;
    }
}

async fn serve_websocket<S: AsyncRead + AsyncWrite + Unpin>(
    app: tauri::AppHandle,
    ws: WebSocketStream<S>,
    peer: IpAddr,
    token: String,
    subnet: String,
) {
    let manager = app
        .try_state::<Arc<CompanionManager>>()
        .map(|m| m.inner().clone());
    let Some(manager) = manager else {
        return;
    };

    let (mut sink, mut source) = ws.split();
    let mut broadcast_rx = manager.subscribe();
    let mut shutdown_rx = manager.shutdown_signal();

    // ---- hello handshake ----
    let device_name: String;
    let mut tracker = protocol::SessionTracker::new();
    let mut rate_cap = protocol::InputRateCap::new(Instant::now());
    let mut last_rx = Instant::now();

    let hello = tokio::select! {
        _ = tokio::time::sleep(HELLO_TIMEOUT) => None,
        frame = source.next() => match frame {
            Some(Ok(Message::Text(text))) => protocol::parse_client_frame(&text),
            _ => None,
        },
    };

    match hello {
        Some(ClientFrame::Hello {
            token: provided,
            device,
            ..
        }) => {
            let blocked = {
                let mut limiter = manager.failure_limiter_pin().lock().unwrap();
                limiter.is_blocked(peer, Instant::now())
            };
            if blocked {
                let _ = sink
                    .send(Message::text(
                        ServerFrame::Error {
                            code: "rate_limited",
                        }
                        .to_json(),
                    ))
                    .await;
                return;
            }

            let subnet_ok = auth::same_subnet(peer, &subnet);
            let token_ok = subnet_ok && auth::token_matches(&token, &provided);
            if !token_ok {
                manager
                    .failure_limiter_pin()
                    .lock()
                    .unwrap()
                    .register_failure(peer, Instant::now());
                let code = if subnet_ok {
                    "bad_token"
                } else {
                    "wrong_subnet"
                };
                let _ = sink
                    .send(Message::text(ServerFrame::Error { code }.to_json()))
                    .await;
                log::warn!("companion: refused hello from {peer} ({code})");
                return;
            }
            manager.failure_limiter_pin().lock().unwrap().clear(peer);

            device_name = device;
            manager.remember_device(&app, &device_name);
            *manager.session_device.lock().unwrap() = Some(device_name.clone());

            let settings = crate::settings::get_settings(&app);
            let activation = match settings.shortcut_activation {
                crate::settings::ShortcutActivation::PushToTalk => "push_to_talk",
                crate::settings::ShortcutActivation::Toggle => "toggle",
                crate::settings::ShortcutActivation::HoldOrToggle => "hold_or_toggle",
            };
            let welcome = ServerFrame::Welcome {
                activation,
                hold_threshold_ms: settings.hold_threshold_ms,
                recording: manager.session_live(),
            };
            if sink.send(Message::text(welcome.to_json())).await.is_err() {
                return;
            }
        }
        _ => {
            // No valid hello in the window: drop without revealing why.
            log::debug!("companion: no hello from {peer}; dropping");
            return;
        }
    }

    log::info!("companion: {device_name} paired from {peer}");

    // ---- session loop ----
    let mut ping_ticker = tokio::time::interval(PING_INTERVAL);
    ping_ticker.tick().await; // the first tick fires immediately
    let mut server_stopping = false;

    loop {
        enum Step {
            Frame(Option<Result<Message, tokio_tungstenite::tungstenite::Error>>),
            Broadcast(String),
            Tick,
            Stop,
        }
        let step = tokio::select! {
            _ = shutdown_rx.changed() => Step::Stop,
            frame = source.next() => Step::Frame(frame),
            out = broadcast_rx.recv() => match out {
                Ok(text) => Step::Broadcast(text),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => Step::Stop,
            },
            _ = ping_ticker.tick() => Step::Tick,
        };

        match step {
            Step::Stop => {
                server_stopping = true;
                let _ = sink
                    .send(Message::text(
                        ServerFrame::Goodbye {
                            reason: "server_stopping",
                        }
                        .to_json(),
                    ))
                    .await;
                break;
            }
            Step::Broadcast(text) => {
                if sink.send(Message::text(text)).await.is_err() {
                    break;
                }
            }
            Step::Tick => {
                if last_rx.elapsed() > PONG_DEADLINE {
                    log::info!("companion: {device_name} silent past the keepalive deadline");
                    break;
                }
                if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
                // Hard session cap: auto-finalize exactly once.
                if tracker.cap_exceeded(Instant::now()) {
                    log::info!("companion: session cap reached for {device_name}; finalizing");
                    if let Some(coordinator) = app.try_state::<crate::TranscriptionCoordinator>() {
                        // Forced finalize: the cap must end the session even
                        // when it is locked (toggle mode), where a synthesized
                        // release edge is ignored and the session would run on
                        // with its audio blocked by the tracker.
                        coordinator.finalize_companion_session();
                    }
                    // KB-016: the cap notice used to reach the phone only -
                    // the Mac had no NoticeCode variant and no emit, so a
                    // force-finalized dictation was silent everywhere the
                    // user might be looking. Same channel as the disconnect
                    // notice below; detail names the device.
                    // AUD-01: this emit is the SINGLE source of the cap
                    // notice. The notice forwarder (companion/mod.rs)
                    // re-broadcasts overlay notices to the paired phones, so
                    // a direct broadcast_frame here as well would deliver the
                    // same Notice frame twice. companion_session_capped stays
                    // phone-bound in the forwarder's filter, so phones keep
                    // getting exactly one cap notice.
                    crate::managers::transcription::emit_overlay_notice(
                        &app,
                        crate::managers::transcription::NoticeCode::CompanionSessionCapped,
                        Some(device_name.clone()),
                    );
                }
            }
            Step::Frame(None) => break,
            Step::Frame(Some(Err(_))) => break,
            Step::Frame(Some(Ok(message))) => {
                last_rx = Instant::now();
                match message {
                    Message::Text(text) => match protocol::parse_client_frame(&text) {
                        Some(ClientFrame::Ptt { pressed }) => {
                            if let Some(coordinator) =
                                app.try_state::<crate::TranscriptionCoordinator>()
                            {
                                coordinator.send_companion_edge(&app, pressed);
                            }
                            if pressed {
                                tracker.press(Instant::now());
                                // Audio is only meaningful during a session;
                                // reset the rate window with it.
                                rate_cap = protocol::InputRateCap::new(Instant::now());
                            } else {
                                tracker.release();
                            }
                        }
                        Some(ClientFrame::Hello { .. }) => {
                            // A second hello on a paired socket is protocol
                            // garbage; drop the connection.
                            break;
                        }
                        None => break,
                    },
                    Message::Binary(bytes) => {
                        let samples = protocol::decode_f32_le(&bytes);
                        if samples.is_empty() {
                            continue;
                        }
                        if !rate_cap.admit(samples.len(), Instant::now()) {
                            log::warn!(
                                "companion: {device_name} exceeded the input rate cap; dropping"
                            );
                            let _ = sink
                                .send(Message::text(
                                    ServerFrame::Goodbye { reason: "rate_cap" }.to_json(),
                                ))
                                .await;
                            // Do not finalize: the session continues from a
                            // healthy reconnect or a manual stop.
                            break;
                        }
                        if tracker.recording() {
                            if let Some(rm) = app
                                .try_state::<Arc<crate::managers::audio::AudioRecordingManager>>()
                            {
                                if let Some(source) = rm.remote_source() {
                                    source.push_chunk(&samples);
                                }
                            }
                        }
                    }
                    Message::Ping(payload) => {
                        if sink.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Message::Pong(_) => { /* last_rx already refreshed */ }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
    }

    // ---- disconnect: graceful finalize ----
    let _ = sink.close().await;
    manager.forget_device(&device_name);

    // Acknowledge the recorder's stop-pause handshake so Stop never waits
    // the 2 s timeout on a vanished phone.
    if let Some(rm) = app.try_state::<Arc<crate::managers::audio::AudioRecordingManager>>() {
        if let Some(source) = rm.remote_source() {
            source.ack_pause();
        }
    }

    // The server stopping mid-session already synthesized the release edge
    // (CompanionManager::stop); only a genuine phone drop runs the
    // disconnect-finalize here.
    if !server_stopping {
        let companion_recording_live = app
            .try_state::<Arc<crate::managers::audio::AudioRecordingManager>>()
            .map(|rm| {
                rm.is_recording()
                    && rm.active_source() == crate::managers::audio::CaptureSource::Remote
            })
            .unwrap_or(false);

        if companion_recording_live {
            log::info!(
                "companion: {device_name} dropped mid-dictation; finalizing what was captured"
            );
            crate::managers::transcription::emit_overlay_notice(
                &app,
                crate::managers::transcription::NoticeCode::CompanionDisconnected,
                Some(device_name.clone()),
            );
            if let Some(coordinator) = app.try_state::<crate::TranscriptionCoordinator>() {
                // Forced finalize: the ordinary Stop effect runs, everything
                // captured (ring backlog included) transcribes and pastes -
                // and it works against locked (toggle) sessions too, which a
                // synthesized release edge would silently drop, stranding the
                // recording the notice above just claimed was finalized.
                coordinator.finalize_companion_session();
            }
        }
    }
    log::info!("companion: {device_name} disconnected");
}
