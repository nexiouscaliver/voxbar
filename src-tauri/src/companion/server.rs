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
        // Err type is tokio-tungstenite's fixed ErrorResponse for
        // accept_hdr_async callbacks; not shrinkable from this crate.
        #[allow(clippy::result_large_err)]
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
    // This connection's targeted frame lane: a SERVER-assigned id (never
    // the client-controlled device name), claimed once the hello
    // authenticates and passed on every Ptt edge so the session freezes
    // exactly this connection as its delivery target.
    let device_lane: u64;
    let mut device_lane_rx: tokio::sync::mpsc::UnboundedReceiver<String>;
    let mut admission = protocol::SessionAdmission::new();
    let mut rate_cap = protocol::InputRateCap::new(Instant::now());
    let mut last_rx = Instant::now();
    // v1.6.2 W1-L0: audio frames that arrived while the Mac's session
    // state said no live session for this connection's lane (dropped by
    // design; counted for the disconnect log line so a state-desynced
    // phone is diagnosable from the log alone). Healthy tap sessions stay
    // at zero: their speech is all inside the session their press locked
    // on, release edges notwithstanding).
    let mut out_of_session_audio_chunks: u64 = 0;

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
            // KB-197: claim the session slot only while no session is live
            // (see claim_session_device) - a second phone connecting
            // mid-dictation is a spectator, not the session's device.
            manager.claim_session_device(&device_name);
            // v1.6.0: claim this connection's targeted frame lane.
            // Phone-target transcripts ride it (never the broadcast every
            // paired phone sees), so a bystander phone cannot render or
            // clipboard-write a dictation it did not make. The lane is
            // keyed by a server-assigned id, not the hello name: a paired
            // phone repeating another phone's name cannot seize the lane,
            // and same-named connections coexist.
            let (lane, rx) = manager.register_device_lane();
            device_lane = lane;
            device_lane_rx = rx;

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
            // v1.6.0: the phone's recent companion history rides along with
            // the welcome, so the client can render its recent list right
            // away (and a reconnecting phone catches up on sessions it
            // missed). Broadcast: every paired phone shares the same Mac
            // history, and this connection's subscription predates the
            // handshake.
            crate::companion::broadcast_recent_history(&app);
        }
        _ => {
            // No valid hello in the window: drop without revealing why.
            log::debug!("companion: no hello from {peer}; dropping");
            return;
        }
    }

    log::info!("companion: {device_name} paired from {peer}");

    // ---- session loop ----
    // v1.6.0 per-session preferences, sticky for this connection: every
    // Ptt edge folds its optional fields into these, and the press edge
    // that starts a session snapshots them into the manager so the shared
    // pipeline honors them for that session (a mid-session press from the
    // same or another phone only arms the NEXT session's prefs).
    let mut prefs = protocol::SessionPrefs::default();
    let mut ping_ticker = tokio::time::interval(PING_INTERVAL);
    ping_ticker.tick().await; // the first tick fires immediately
    let mut server_stopping = false;

    loop {
        enum Step {
            Frame(Option<Result<Message, tokio_tungstenite::tungstenite::Error>>),
            Broadcast(String),
            Targeted(String),
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
            out = device_lane_rx.recv() => match out {
                Some(text) => Step::Targeted(text),
                // Unreachable while this connection holds its lane (the
                // map keeps the sender alive); kept as a safe wind-down if
                // the lane is ever released elsewhere.
                None => break,
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
            Step::Targeted(text) => {
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
                // Hard session cap: auto-finalize exactly once per cap
                // interval. The truth observation keeps the clock honest for
                // a silent-but-live session (chunks refresh it too, but the
                // cap must never depend on audio arriving).
                admission.observe(
                    Instant::now(),
                    lane_owns_live_session(&manager, device_lane),
                );
                if admission.cap_exceeded(Instant::now()) {
                    log::info!("companion: session cap reached for {device_name}; finalizing");
                    if let Some(coordinator) = app.try_state::<crate::TranscriptionCoordinator>() {
                        // Forced finalize: the cap must end the session even
                        // when it is locked (toggle mode or a tap-locked
                        // hold-or-toggle session), where a synthesized
                        // release edge is ignored and the session would run
                        // on with its audio still admitted.
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
                        Some(ClientFrame::Ptt {
                            pressed,
                            prefs: update,
                        }) => {
                            prefs.apply(update);
                            if pressed {
                                // Arm the pending slot the tray's companion
                                // status block reads as "what the next press
                                // will do". The session ITSELF is armed by
                                // the prefs riding the edge below: the
                                // coordinator applies them to the live slot
                                // at the single serialized instant it commits
                                // the start, so a concurrent press from
                                // another phone (whose command queues behind
                                // this one) cannot overwrite them in any
                                // arm-to-freeze window.
                                manager.set_pending_session_prefs(prefs);
                            }
                            // v1.6.2 W1-L0: refresh the admission truth
                            // BEFORE forwarding the edge. This frame is what
                            // carries the lane into the coordinator, so the
                            // truth read here necessarily predates any start
                            // this edge may commit: the observation closes
                            // any stale live window from an earlier session
                            // of this same phone instead of mistaking the
                            // about-to-commit start for its continuation.
                            // The release edge deliberately does NOT close
                            // admission: a hold-or-toggle tap locks the
                            // session ON at the finger-lift, and the Mac's
                            // session state (not the finger) decides when
                            // audio stops flowing.
                            admission.observe(
                                Instant::now(),
                                lane_owns_live_session(&manager, device_lane),
                            );
                            if let Some(coordinator) =
                                app.try_state::<crate::TranscriptionCoordinator>()
                            {
                                coordinator.send_companion_edge(&app, pressed, prefs, device_lane);
                            }
                            if pressed {
                                // Audio is only meaningful during a session;
                                // reset the rate window with it.
                                rate_cap = protocol::InputRateCap::new(Instant::now());
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
                        // v1.6.2 W1-L0: sample the session truth at the
                        // chunk itself. This is the same state the State
                        // frames broadcast (and the disconnect-finalize
                        // path consults), read live: no broadcast lag, no
                        // edge-desync window between a tap's release and
                        // the locked session it left behind.
                        admission.observe(
                            Instant::now(),
                            lane_owns_live_session(&manager, device_lane),
                        );
                        if admission.admits() {
                            if let Some(rm) = app
                                .try_state::<Arc<crate::managers::audio::AudioRecordingManager>>()
                            {
                                if let Some(source) = rm.remote_source() {
                                    source.push_chunk(&samples);
                                }
                            }
                        } else {
                            out_of_session_audio_chunks += 1;
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
    manager.forget_device(&app, &device_name);
    // Release the targeted lane this connection claimed. Ids are never
    // reused, so a session still pointing at it reads the miss as the
    // documented dropped-phone no-op.
    manager.unregister_device_lane(device_lane);

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
    if out_of_session_audio_chunks > 0 {
        // Not an error by itself (spectator audio is refused the same way),
        // but a live symptom of state desync: speech kept arriving while the
        // Mac's session state said no live session for this connection's
        // lane.
        log::info!(
            "companion: {device_name} sent {out_of_session_audio_chunks} audio chunk(s) \
             outside a recording session; they were dropped"
        );
    }
}

/// v1.6.2 W1-L0: the Mac's session truth for one connection's lane. The
/// session lane was frozen from this connection's own starting press at
/// the instant the coordinator committed the start (begin_session_lane),
/// so this is true exactly while the session THIS connection owns is
/// live: a spectator phone, a reconnecting phone that has not pressed,
/// and a keyboard dictation (whose sessions never touch the companion
/// session state) all read false, and their audio stays out-of-session.
fn lane_owns_live_session(manager: &CompanionManager, lane: u64) -> bool {
    // Child-module access to the frozen lane slot; the stop pipeline's
    // phone-target delivery (deliver_transcript_to_phone) reads the same
    // slot the same way.
    manager.session_live() && *manager.session_lane.lock().unwrap() == Some(lane)
}

#[cfg(test)]
mod tests {
    use super::lane_owns_live_session;
    use crate::companion::protocol::SessionAdmission;
    use crate::companion::CompanionManager;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tauri::Manager;

    /// One connection's admission rig: the same two seams the connection
    /// loop drives (the lane-scoped session truth and the admission it
    /// feeds) plus the counters the loop maintains. Frames are simulated
    /// by calling the same methods the loop's handlers call, in wire
    /// order; time is an explicit parameter because the cap contract runs
    /// to fifteen minutes.
    struct Rig {
        admission: SessionAdmission,
        lane: u64,
        admitted_samples: usize,
        out_of_session_chunks: u64,
    }

    impl Rig {
        fn new(lane: u64) -> Self {
            Rig {
                admission: SessionAdmission::new(),
                lane,
                admitted_samples: 0,
                out_of_session_chunks: 0,
            }
        }

        /// A Ptt frame, exactly as the loop's text handler processes it:
        /// observe the truth BEFORE the edge is forwarded (the frame is
        /// what carries the lane into the coordinator). The edge itself
        /// goes to the coordinator and mutates nothing here.
        fn ptt(&mut self, manager: &CompanionManager, pressed: bool, now: Instant) {
            self.admission
                .observe(now, lane_owns_live_session(manager, self.lane));
            let _ = pressed;
        }

        /// A binary audio chunk, exactly as the loop's binary handler
        /// processes it: observe the truth, then admit (push into the
        /// remote source, counted as samples here) or count it
        /// out-of-session.
        fn chunk(&mut self, manager: &CompanionManager, samples: usize, now: Instant) {
            self.admission
                .observe(now, lane_owns_live_session(manager, self.lane));
            if self.admission.admits() {
                self.admitted_samples += samples;
            } else {
                self.out_of_session_chunks += 1;
            }
        }

        /// The keepalive tick, exactly as the loop's Tick arm processes
        /// the cap: observe the truth (the clock must never depend on
        /// audio arriving), then check the cap. True means the loop would
        /// run the forced finalize plus the cap notice.
        fn tick(&mut self, manager: &CompanionManager, now: Instant) -> bool {
            self.admission
                .observe(now, lane_owns_live_session(manager, self.lane));
            self.admission.cap_exceeded(now)
        }
    }

    /// The coordinator's Start-effect commit, as the connection observes
    /// it: the lane freezes from the starting press's own edge
    /// (begin_session_lane) and the session state flips live before the
    /// State broadcast tells the phone to stream. on_session_changed is
    /// Wry-typed, so the mock-app tests drive the same two slots its true
    /// branch writes (the mod.rs test pattern).
    fn commit_session_start<R: tauri::Runtime>(
        app: &tauri::AppHandle<R>,
        manager: &CompanionManager,
        lane: u64,
    ) {
        crate::companion::begin_session_lane(app, lane);
        manager.session_live.store(true, Ordering::Release);
    }

    /// The stop pipeline's landing, as the connection observes it: the
    /// session state flips false (the State broadcast the phone stops
    /// streaming on).
    fn commit_session_end(manager: &CompanionManager) {
        manager.session_live.store(false, Ordering::Release);
    }

    /// v1.6.2 W1-L0 pin: reproduces the operator's exact sequence from
    /// the live log (06:19:03 through 06:19:10). A quick tap locks a
    /// hold-or-toggle session on (~62 ms press-hold), the finger-lift
    /// release edge arrives while the session is still live, and every
    /// chunk after it must be captured. The pre-v1.6.2 edge tracker ended
    /// its admission window at that release edge, so all speech after
    /// the first ~100 ms was silently dropped as out-of-session audio
    /// (VAD saw one 40-50 ms leading-silence chunk, finish collected
    /// zero samples, the zero-audio notice fired on every tap). Asserted:
    /// the FULL audio is captured, not just the pre-release sliver, and
    /// the out-of-session counter stays at zero for the healthy tap
    /// session (task 4).
    #[test]
    fn tap_locked_session_captures_the_full_audio_after_the_release_edge() {
        let app = tauri::test::mock_app();
        let manager = Arc::new(CompanionManager::new());
        app.manage(Arc::clone(&manager));
        manager.claim_session_device("Pixel 8");
        let (lane, _lane_rx) = manager.register_device_lane();
        let mut rig = Rig::new(lane);
        let t0 = Instant::now();

        // The tap's press edge is forwarded; its start has not committed.
        rig.ptt(&manager, true, t0);
        // The coordinator commits the start (lane frozen, state live,
        // State broadcast out; the phone begins streaming on it).
        commit_session_start(app.handle(), &manager, lane);
        // The finger-lift release edge lands 62 ms later: below the hold
        // threshold, so the session LOCKED ON. The coordinator ignores
        // the release for stopping purposes; the server no longer lets
        // it close admission either.
        rig.ptt(&manager, false, t0 + Duration::from_millis(62));

        // The whole dictation streams after the release edge.
        let chunk_samples = 320; // 20 ms at 16 kHz
        let speech_chunks = 250usize; // ~5 s of speech, all post-release
        for i in 0..speech_chunks {
            rig.chunk(
                &manager,
                chunk_samples,
                t0 + Duration::from_millis(100 + 20 * (i as u64)),
            );
        }
        let spoken = chunk_samples * speech_chunks;
        assert_eq!(
            rig.admitted_samples, spoken,
            "the FULL post-release audio must be captured, not a pre-release sliver"
        );
        assert_eq!(
            rig.out_of_session_chunks, 0,
            "a healthy tap session sends zero out-of-session chunks"
        );

        // The second tap stops it: the press edge (a press, never a
        // release, is the stop edge for a locked session) observes the
        // session still live, then the stop pipeline lands.
        rig.ptt(&manager, true, t0 + Duration::from_secs(6));
        commit_session_end(&manager);
        // The phone stops streaming once the State broadcast reaches it;
        // nothing further is admitted after the state flip.
        rig.chunk(
            &manager,
            chunk_samples,
            t0 + Duration::from_secs(6) + Duration::from_millis(50),
        );
        assert_eq!(
            rig.admitted_samples, spoken,
            "audio after the session state ends is not admitted"
        );
        assert_eq!(rig.out_of_session_chunks, 1);
    }

    /// v1.6.2 W1-L0 pin: push-to-talk holds keep working end to end.
    /// Audio during the hold is admitted, a chunk racing the release
    /// still lands (the recorder consumes until the stop pipeline flips
    /// the state), and admission closes exactly when the session ends,
    /// never at the release edge itself.
    #[test]
    fn push_to_talk_hold_admits_until_the_session_actually_ends() {
        let app = tauri::test::mock_app();
        let manager = Arc::new(CompanionManager::new());
        app.manage(Arc::clone(&manager));
        manager.claim_session_device("Pixel 8");
        let (lane, _lane_rx) = manager.register_device_lane();
        let mut rig = Rig::new(lane);
        let t0 = Instant::now();

        // Press and hold: the session starts and speech streams through.
        rig.ptt(&manager, true, t0);
        commit_session_start(app.handle(), &manager, lane);
        for i in 0..100 {
            rig.chunk(&manager, 320, t0 + Duration::from_millis(20 * i));
        }

        // The release edge (a 2 s hold, above every threshold). The
        // coordinator classifies it as a hold and stops the session; a
        // chunk that raced the stop pipeline still lands, because the
        // recorder is consuming until the state actually flips.
        rig.ptt(&manager, false, t0 + Duration::from_secs(2));
        rig.chunk(
            &manager,
            320,
            t0 + Duration::from_secs(2) + Duration::from_millis(5),
        );
        assert_eq!(rig.admitted_samples, 101 * 320);

        // The stop pipeline lands: later chunks are out-of-session.
        commit_session_end(&manager);
        rig.chunk(
            &manager,
            320,
            t0 + Duration::from_secs(2) + Duration::from_millis(80),
        );
        assert_eq!(rig.admitted_samples, 101 * 320);
        assert_eq!(rig.out_of_session_chunks, 1);
    }

    /// v1.6.2 W1-L0 pin, exhaustive over the truth inputs a connection
    /// can face: audio is admitted only while THIS connection's lane
    /// owns the live session. No session at all, another phone's live
    /// session, and this phone's own ended session (stale lane) all
    /// refuse and count, so a spectator phone can never feed another
    /// phone's dictation and a press that never started a session never
    /// contaminates the pre-warmed ring.
    #[test]
    fn audio_is_refused_whenever_this_lanes_session_is_not_live() {
        let app = tauri::test::mock_app();
        let manager = Arc::new(CompanionManager::new());
        app.manage(Arc::clone(&manager));
        manager.claim_session_device("iPhone 15");
        let (mine, _mine_rx) = manager.register_device_lane();
        let (other, _other_rx) = manager.register_device_lane();
        let mut rig = Rig::new(mine);
        let t0 = Instant::now();

        // No session has ever been live.
        rig.chunk(&manager, 320, t0);
        assert_eq!(rig.admitted_samples, 0);
        assert_eq!(rig.out_of_session_chunks, 1);

        // Another phone's session is live: spectator audio stays out.
        commit_session_start(app.handle(), &manager, other);
        rig.chunk(&manager, 320, t0 + Duration::from_millis(50));
        assert_eq!(
            rig.admitted_samples, 0,
            "a spectator must not feed another phone's dictation"
        );
        assert_eq!(rig.out_of_session_chunks, 2);

        // That session ends (and a keyboard dictation in between changes
        // nothing: companion session state stays false throughout, which
        // is exactly what this lane reads).
        commit_session_end(&manager);
        rig.chunk(&manager, 320, t0 + Duration::from_millis(100));
        assert_eq!(rig.admitted_samples, 0);
        assert_eq!(rig.out_of_session_chunks, 3);

        // Our own session opens admission...
        commit_session_start(app.handle(), &manager, mine);
        rig.chunk(&manager, 320, t0 + Duration::from_millis(150));
        assert_eq!(rig.admitted_samples, 320);

        // ...and the stale lane of our ended session never re-opens it.
        commit_session_end(&manager);
        rig.chunk(&manager, 320, t0 + Duration::from_millis(200));
        assert_eq!(rig.admitted_samples, 320);
        assert_eq!(rig.out_of_session_chunks, 4);
    }

    /// v1.6.2 W1-L0 pin: a tap-locked session still force-finalizes at
    /// the 15-minute cap. The pre-v1.6.2 tracker cleared its clock at the
    /// tap's release edge together with admission, so locked tap sessions
    /// had no cap at all; now the Tick's truth observation keeps the
    /// clock armed for as long as the session stays live (silent or
    /// not), and the forced finalize (which works against locked
    /// sessions) fires exactly once per cap interval.
    #[test]
    fn tap_locked_session_still_hits_the_cap_from_the_tick_path() {
        let app = tauri::test::mock_app();
        let manager = Arc::new(CompanionManager::new());
        app.manage(Arc::clone(&manager));
        manager.claim_session_device("Pixel 8");
        let (lane, _lane_rx) = manager.register_device_lane();
        let mut rig = Rig::new(lane);
        let t0 = Instant::now();

        // The tap that locks on, then fifteen minutes of live session.
        rig.ptt(&manager, true, t0);
        commit_session_start(app.handle(), &manager, lane);
        rig.ptt(&manager, false, t0 + Duration::from_millis(62));

        let cap = Duration::from_secs(15 * 60);
        // The release edge's observation is the first LIVE one, so that
        // is where the cap clock arms; fifteen minutes of live session
        // follow, with the phone streaming and the keepalive ticking.
        let armed = t0 + Duration::from_millis(62);
        let mut now = armed;
        while now < armed + cap {
            rig.chunk(&manager, 320, now);
            if (now - armed).as_secs() % 10 == 0 {
                assert!(
                    !rig.tick(&manager, now),
                    "the cap must not fire before fifteen minutes (at {})",
                    (now - armed).as_secs()
                );
            }
            now += Duration::from_secs(1);
        }

        // At the threshold the tick fires the forced finalize exactly
        // once; the next tick does not refire while the stop lands.
        assert!(rig.tick(&manager, armed + cap));
        assert!(!rig.tick(&manager, armed + cap + Duration::from_secs(10)));
    }
}
