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

/// Fleet pass 2: the detached connect/disconnect workers have no failure
/// surface of their own - a panic inside one (a store that cannot mount,
/// a serialization failure) printed to raw stderr only, which a packaged
/// .app launched from Finder never surfaces into the rotating log the
/// debug panel and diagnostics read. Every spawned connect/disconnect
/// worker body therefore runs under catch_unwind with the panic logged
/// through the app's logger (the work itself stays best-effort: the log
/// line is the fix, not a retry).
fn spawn_worker(name: &'static str, body: impl FnOnce() + Send + 'static) {
    std::thread::spawn(move || {
        if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
            log::error!("companion: {name} worker panicked: {panic:?}");
        }
    });
}

/// Fleet pass 2 (KB-270 recovery gap): the disconnect finalize every
/// connection owes - registration release by both KB-255 keys, the
/// coordinator's lane retirement, the tray re-sync, the lane recorder
/// teardown, and the mid-dictation finalize. Extracted from the session
/// loop's tail so the ONE post-registration early exit (the Welcome send
/// failing because the phone died between hello and welcome) runs the
/// same teardown instead of leaking the connections entry (a phantom
/// phone in the tray's paired count) and the lane machine, post-process
/// mirror entry, and cancel generation forever - the exact growth the
/// KB-270 lane promised could not happen. `server_stopping` is false on
/// the welcome-failure path (no loop ran), and a connection that never
/// received its Welcome owns no session, so the mid-dictation branch
/// reads false there by construction.
#[allow(clippy::too_many_arguments)] // arity mirrors the serve_websocket locals the teardown consumes
async fn finalize_disconnected_connection<S: AsyncRead + AsyncWrite + Unpin>(
    sink: &mut futures_util::stream::SplitSink<WebSocketStream<S>, Message>,
    app: &tauri::AppHandle,
    manager: &Arc<CompanionManager>,
    device_name: &str,
    device_lane: u64,
    device_generation: u64,
    server_stopping: bool,
    admitted_dropped_chunks: u64,
    scrubbed_non_finite_samples: u64,
    out_of_session_audio_chunks: u64,
) {
    let _ = sink.close().await;
    // KB-255: release the registration by BOTH keys (lane + generation).
    // A stale socket whose keepalive death lagged behind a same-name
    // reconnect carries the old pair, matches nothing, and the fresh
    // registration survives with the tray's paired count intact.
    manager.unregister_connection(device_lane, device_generation);
    // KB-270 (v1.6.4): the lane's coordinator machine and its bookkeeping
    // are now dead weight once their pipeline drains; the coordinator
    // retires them at the drain (and immediately when the machine is
    // already idle), so a flapping phone cannot grow the maps forever.
    if let Some(coordinator) = app.try_state::<crate::TranscriptionCoordinator>() {
        coordinator.notify_lane_gone(device_lane);
    }
    // v1.6.4 (accept-wedge hardening): the disconnect-edge tray re-sync
    // computes the tray snapshot (settings + model list, a disk icon load
    // on cache miss) on its calling thread by design (tray.rs); this is
    // exactly the edge a dead client lands on, so run it on a detached
    // worker instead of this async task - the same rule the connect edge
    // (note_device_connected) already follows.
    {
        let app = app.clone();
        let disconnected_manager = Arc::clone(manager);
        spawn_worker("note_device_disconnected", move || {
            disconnected_manager.note_device_disconnected(&app);
        });
    }

    // KB-263 (v1.6.4): NO pause acknowledgement happens on the disconnect
    // path, deliberately. The old eager ack ran before any Stop had
    // raised the request, so it was inert (an ack only counts once
    // `pause_requested` is live, and the Stop that follows wipes any
    // earlier one - pinned by the recorder test
    // kb263_an_ack_before_any_stop_raises_the_pause_request_is_wiped), and
    // no deferred ack can work either: every lane stop shape removes the
    // map entry and closes the SOURCE before stopping the recorder (pinned
    // by lane_stops_run_outside_the_lanes_lock_and_close_the_source_first),
    // so by the time any Stop raises the pause request there is no ack
    // route left. The source close IS the acknowledgement: it marks the
    // producer gone, which the consumer reads as an immediate ack and
    // skips the 2 s wait (pinned by
    // remote_stop_after_source_close_skips_the_pause_wait_entirely). A
    // future stop shape that waits with the producer alive must arrange
    // its own ack at the moment it raises the request. Also retire the
    // lane's idle recorder (the RAM rule: no idle lane recorder outlives
    // its connection once a newer pre-warm or this teardown claims the
    // slot).
    if let Some(rm) = app.try_state::<Arc<crate::managers::audio::AudioRecordingManager>>() {
        rm.close_remote_lane(device_lane);
    }

    // The server stopping mid-session already synthesized the release edge
    // (CompanionManager::stop); only a genuine phone drop runs the
    // disconnect-finalize here.
    if !server_stopping {
        // v1.6.2 W1-L2 (KB-111): the decision is scoped to THIS lane's
        // session. Another phone still recording reads false here and its
        // session is untouched - either phone dropping used to finalize
        // the ONE shared session while the other kept streaming.
        let lane_recording_live = app
            .try_state::<Arc<crate::managers::audio::AudioRecordingManager>>()
            .map(|rm| rm.lane_is_recording(device_lane))
            .unwrap_or(false);

        if lane_recording_live {
            log::info!(
                "companion: {device_name} dropped mid-dictation; finalizing what was captured"
            );
            crate::managers::transcription::emit_overlay_notice(
                app,
                crate::managers::transcription::NoticeCode::CompanionDisconnected,
                Some(device_name.to_string()),
            );
            if let Some(coordinator) = app.try_state::<crate::TranscriptionCoordinator>() {
                // Forced finalize: the ordinary Stop effect runs, everything
                // captured (ring backlog included) transcribes and pastes -
                // and it works against locked (toggle) sessions too, which a
                // synthesized release edge would silently drop, stranding the
                // recording the notice above just claimed was finalized.
                coordinator.finalize_companion_lane(device_lane);
            }
        }
    }
    log::info!("companion: {device_name} disconnected");
    if admitted_dropped_chunks > 0 {
        // KB-272: admitted audio that never reached the ring. This is the
        // recorder-lifecycle failure mode the v1.6.3 forensic pass had to
        // rule out with no signal at all; it must never hide again.
        log::warn!(
            "companion: {device_name} had {admitted_dropped_chunks} admitted audio chunk(s) \
             dropped: no live lane recorder; they never reached the ring"
        );
    }
    if scrubbed_non_finite_samples > 0 {
        log::warn!(
            "companion: {device_name} sent {scrubbed_non_finite_samples} non-finite audio \
             sample(s); they were scrubbed to silence"
        );
    }
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
    // This connection's identity: a SERVER-assigned lane id (never the
    // client-controlled device name) plus its KB-255 registration
    // generation. Claimed once the hello authenticates; the lane rides
    // every Ptt edge so the session freezes exactly this connection as its
    // owner, and the (lane, generation) pair keys the registration so a
    // stale socket's late death cannot erase a fresh same-name
    // registration.
    let device_lane: u64;
    let device_generation: u64;
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
    // KB-272 (v1.6.3): chunks admission ACCEPTED whose push into the lane's
    // ring still failed (no live lane recorder). Before this counter the
    // only call site ignored push_remote_chunk's bool, so this exact class
    // of silent drop had to be ruled out by hand in the v1.6.3 forensic
    // pass; now it is counted for the disconnect log line and warned about
    // at its first occurrence.
    let mut admitted_dropped_chunks: u64 = 0;
    // v1.6.3 defense in depth: non-finite samples the decode boundary
    // scrubbed to silence (a hostile or buggy client), surfaced the same
    // way so a poisoned stream is diagnosable from the log alone.
    let mut scrubbed_non_finite_samples: u64 = 0;
    let mut non_finite_audio_warned = false;

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
            device_id,
            ..
        }) => {
            let blocked = {
                let mut limiter = manager
                    .failure_limiter_pin()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
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
                    .unwrap_or_else(|e| e.into_inner())
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
            manager
                .failure_limiter_pin()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear(peer);

            // v1.6.2 fleet pass 1: the connection's identity is the human
            // name disambiguated by the phone's persistent device id (the
            // name alone is a platform token every same-platform phone
            // shares, which collapsed two iPhones into one Recent list -
            // private phone-destined dictations included). Everything
            // name-keyed downstream (the registration, the frozen session,
            // history's capture_device, the per-phone recents filter) uses
            // this composite, so same-platform phones never share it.
            // KB-261 (v1.6.4, fleet pass 1): the identity is EXACT - the
            // raw name#tag composite, nothing else. The v1.6.4
            // platform-name adoption (a fresh-id hello folding into an
            // absent prior's identity) is removed: the platform token is
            // client-controlled and shared by every same-platform phone,
            // so adoption shipped one phone's transcript history to
            // whichever same-platform token holder paired while its owner
            // was away. The resolution is in-memory only (no settings IO
            // on this async task). KB-291 (v1.6.6): the
            // persist_known_identities worker this block used to spawn for
            // a freshly known identity is deleted with the write-only
            // known-identity machinery it served.
            let (registration, rx) =
                manager.register_connection_with_identity(&device, device_id.as_deref());
            device_name = registration.device;
            device_lane = registration.lane;
            device_generation = registration.generation;
            device_lane_rx = rx;
            // v1.6.4 (accept-wedge hardening): the connect-edge bookkeeping
            // (settings read + write, tray re-sync) is synchronous disk and
            // main-thread work; run it on a detached worker instead of this
            // async task, so a storm of reconnecting clients cannot pin
            // every async runtime worker and stall the accept loop.
            {
                let app = app.clone();
                let connected_manager = Arc::clone(&manager);
                let connected_device = device_name.clone();
                spawn_worker("note_device_connected", move || {
                    connected_manager.note_device_connected(&app, &connected_device);
                });
            }

            // Fleet pass 2 (accept-wedge hardening, closed seam): the
            // Welcome frame needs a settings read, and get_settings is a
            // read-modify-write on the store (migration and binding-merge
            // write-backs) that takes the plugin store's per-store mutex -
            // the exact synchronous store IO the hardening moved off this
            // task, sitting right after the workers it was extracted into.
            // The read now runs on the blocking pool and the async task
            // awaits the handle; a failed read falls back to the shipped
            // defaults so a panicking worker cannot take the connection
            // down with it.
            let welcome_app = app.clone();
            let (activation, hold_threshold_ms) = tauri::async_runtime::spawn_blocking(move || {
                let settings = crate::settings::get_settings(&welcome_app);
                let activation = match settings.shortcut_activation {
                    crate::settings::ShortcutActivation::PushToTalk => "push_to_talk",
                    crate::settings::ShortcutActivation::Toggle => "toggle",
                    crate::settings::ShortcutActivation::HoldOrToggle => "hold_or_toggle",
                };
                (activation, settings.hold_threshold_ms)
            })
            .await
            .unwrap_or_else(|panic| {
                log::error!(
                    "companion: welcome settings read panicked ({panic:?}); \
                         using the shipped defaults"
                );
                let defaults = crate::settings::get_default_settings();
                let activation = match defaults.shortcut_activation {
                    crate::settings::ShortcutActivation::PushToTalk => "push_to_talk",
                    crate::settings::ShortcutActivation::Toggle => "toggle",
                    crate::settings::ShortcutActivation::HoldOrToggle => "hold_or_toggle",
                };
                (activation, defaults.hold_threshold_ms)
            });
            let welcome = ServerFrame::Welcome {
                activation,
                hold_threshold_ms,
                // Per-lane truth (W1-L2): the fresh connection owns no
                // session yet - a reconnecting phone's orphaned session was
                // force-finalized at its disconnect - so this is false
                // exactly when THIS phone has nothing live.
                recording: manager.lane_owns_live_session(device_lane),
            };
            if sink.send(Message::text(welcome.to_json())).await.is_err() {
                // Fleet pass 2: the phone died between hello and the
                // welcome write. This used to be a bare silent return that
                // skipped the ENTIRE disconnect finalize below - leaking
                // the connections entry (a phantom phone in the tray's
                // paired count until server stop) and the KB-270 lane
                // machine, post-process mirror entry, and cancel
                // generation forever. Run the same finalize every other
                // disconnect owes.
                log::info!(
                    "companion: {device_name} dropped before its Welcome; finalizing the lane"
                );
                finalize_disconnected_connection(
                    &mut sink,
                    &app,
                    &manager,
                    &device_name,
                    device_lane,
                    device_generation,
                    false,
                    0,
                    0,
                    0,
                )
                .await;
                return;
            }
            // v1.6.2 W1-L2: the phone's OWN recent list rides the welcome
            // (filtered to this phone's sessions), and its lane's recorder
            // is the one pre-warmed (the RAM rule: pre-warm keeps only the
            // most recent connection's recorder).
            // v1.6.4 (accept-wedge hardening): the history query is a
            // synchronous SQLite read; run it on a detached worker instead
            // of this async task, so a reconnecting-client storm cannot pin
            // the async runtime's workers (a wedged accept loop until
            // restart is exactly the v1.6.3 proof-run failure).
            {
                let app = app.clone();
                let history_device = device_name.clone();
                let history_lane = device_lane;
                spawn_worker("send_recent_history_to_lane", move || {
                    crate::companion::send_recent_history_to_lane(
                        &app,
                        history_lane,
                        &history_device,
                    );
                });
            }
            if let Some(rm) = app.try_state::<Arc<crate::managers::audio::AudioRecordingManager>>()
            {
                let rm = Arc::clone(&rm);
                std::thread::spawn(move || {
                    if let Err(e) = rm.prewarm_remote_lane(device_lane) {
                        log::debug!("companion: lane {device_lane} recorder pre-warm failed: {e}");
                    }
                });
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
                        // on with its audio still admitted. v1.6.2 W1-L2:
                        // per-lane - THIS connection's session ends, every
                        // other phone's keeps recording.
                        coordinator.finalize_companion_lane(device_lane);
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
                                // KB-285 (v1.6.6): the edge's activation
                                // mode and hold threshold live in the
                                // settings store, and get_settings is a
                                // read-modify-write over the plugin store's
                                // mutex (migration and binding-merge
                                // write-backs) - the exact blocking class
                                // the hello-path seams moved off this task,
                                // and this edge fires on every phone press.
                                // Read through the blocking pool and await
                                // the handle (the Welcome read's seam); a
                                // panicking read falls back to the shipped
                                // defaults. The read stays per-edge, never
                                // a connect-time snapshot, so a
                                // mid-connection activation change is
                                // honored by the next press.
                                let edge_app = app.clone();
                                let (mode, hold_threshold) =
                                    tauri::async_runtime::spawn_blocking(move || {
                                        let settings = crate::settings::get_settings(&edge_app);
                                        (
                                            settings.shortcut_activation,
                                            Duration::from_millis(settings.hold_threshold_ms),
                                        )
                                    })
                                    .await
                                    .unwrap_or_else(|panic| {
                                        log::error!(
                                            "companion: ptt-edge settings read panicked \
                                                 ({panic:?}); using the shipped defaults"
                                        );
                                        let defaults = crate::settings::get_default_settings();
                                        (
                                            defaults.shortcut_activation,
                                            Duration::from_millis(defaults.hold_threshold_ms),
                                        )
                                    });
                                coordinator.send_companion_edge(
                                    pressed,
                                    prefs,
                                    device_lane,
                                    mode,
                                    hold_threshold,
                                );
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
                        // v1.6.3 defense in depth: the decode boundary
                        // scrubs non-finite samples to silence (a single
                        // NaN poisons Silero into reading the whole stream
                        // unvoiced) and reports how many it scrubbed.
                        let (samples, non_finite) = protocol::decode_f32_le(&bytes);
                        if non_finite > 0 {
                            scrubbed_non_finite_samples += non_finite as u64;
                            if !non_finite_audio_warned {
                                non_finite_audio_warned = true;
                                log::warn!(
                                    "companion: {device_name} sent non-finite audio samples; \
                                     scrubbing them to silence"
                                );
                            }
                        }
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
                            // v1.6.2 W1-L2: the chunk lands in THIS
                            // connection's own ring; no other phone's
                            // session can receive it and no shared recorder
                            // exists to cross-contaminate.
                            // KB-272 (v1.6.3): a false return means the
                            // chunk was admitted but the lane's recorder is
                            // gone (never pre-warmed, or its session already
                            // closed it) - count it, never swallow it.
                            let delivered = app
                                .try_state::<Arc<crate::managers::audio::AudioRecordingManager>>()
                                .map(|rm| rm.push_remote_chunk(device_lane, &samples))
                                .unwrap_or(false);
                            if !delivered {
                                admitted_dropped_chunks += 1;
                                if admitted_dropped_chunks == 1 {
                                    log::warn!(
                                        "companion: {device_name} audio admitted but the lane \
                                         recorder is closed; dropping it"
                                    );
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
    // Fleet pass 2: the whole block lives in
    // finalize_disconnected_connection now, so the ONE post-registration
    // early exit (the Welcome send failing) runs the same teardown the
    // session loop's tail does - no path off the hello may skip it.
    finalize_disconnected_connection(
        &mut sink,
        &app,
        &manager,
        &device_name,
        device_lane,
        device_generation,
        server_stopping,
        admitted_dropped_chunks,
        scrubbed_non_finite_samples,
        out_of_session_audio_chunks,
    )
    .await;
}

/// v1.6.2 W1-L0/W1-L2: the Mac's session truth for one connection's lane.
/// The session record was frozen from this connection's own starting press
/// at the instant the coordinator committed the start (begin_session), so
/// this is true exactly while the session THIS connection owns is live: a
/// spectator phone, a reconnecting phone that has not pressed, and a
/// keyboard dictation (whose sessions never touch the companion session
/// state) all read false, and their audio stays out-of-session. With
/// per-lane sessions, another phone's live session reads false here too -
/// each connection sees only its own.
fn lane_owns_live_session(manager: &CompanionManager, lane: u64) -> bool {
    manager.lane_owns_live_session(lane)
}

#[cfg(test)]
mod tests {
    use super::lane_owns_live_session;
    use crate::companion::protocol::SessionAdmission;
    use crate::companion::CompanionManager;
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
        /// KB-272: admitted chunks whose push into the lane's ring failed
        /// (no live lane recorder) - the counter the disconnect log line
        /// surfaces.
        admitted_dropped_chunks: u64,
    }

    impl Rig {
        fn new(lane: u64) -> Self {
            Rig {
                admission: SessionAdmission::new(),
                lane,
                admitted_samples: 0,
                out_of_session_chunks: 0,
                admitted_dropped_chunks: 0,
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
        /// out-of-session. The healthy path (a live lane recorder).
        fn chunk(&mut self, manager: &CompanionManager, samples: usize, now: Instant) {
            self.chunk_delivery(manager, samples, now, true);
        }

        /// The same chunk with an explicit push outcome, mirroring the
        /// KB-272 seam: `delivered` is push_remote_chunk's bool at the
        /// call site (false when the lane's recorder is gone).
        fn chunk_delivery(
            &mut self,
            manager: &CompanionManager,
            samples: usize,
            now: Instant,
            delivered: bool,
        ) {
            self.admission
                .observe(now, lane_owns_live_session(manager, self.lane));
            if self.admission.admits() {
                if delivered {
                    self.admitted_samples += samples;
                } else {
                    self.admitted_dropped_chunks += 1;
                }
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
    /// it: the per-lane session record freezes from the starting press's
    /// own edge (begin_session, live from the instant of the commit) and
    /// the shared action's start lands before the State frame tells the
    /// phone to stream. on_lane_session_changed is Wry-typed, so the
    /// mock-app tests drive the same record its true branch writes (the
    /// mod.rs test pattern).
    fn commit_session_start<R: tauri::Runtime>(app: &tauri::AppHandle<R>, lane: u64) {
        crate::companion::begin_session(app, lane, Default::default());
    }

    /// The stop pipeline's landing, as the connection observes it: the
    /// session record flips not-live (the State frame the phone stops
    /// streaming on).
    fn commit_session_end<R: tauri::Runtime>(app: &tauri::AppHandle<R>, lane: u64) {
        crate::companion::on_lane_session_changed(app, lane, false);
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
        let (lane, _, _) = manager.register_connection("Pixel 8");
        let mut rig = Rig::new(lane);
        let t0 = Instant::now();

        // The tap's press edge is forwarded; its start has not committed.
        rig.ptt(&manager, true, t0);
        // The coordinator commits the start (lane frozen, state live,
        // State broadcast out; the phone begins streaming on it).
        commit_session_start(app.handle(), lane);
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
        commit_session_end(app.handle(), lane);
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
        let (lane, _, _) = manager.register_connection("Pixel 8");
        let mut rig = Rig::new(lane);
        let t0 = Instant::now();

        // Press and hold: the session starts and speech streams through.
        rig.ptt(&manager, true, t0);
        commit_session_start(app.handle(), lane);
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
        commit_session_end(app.handle(), lane);
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
        let (mine, _, _) = manager.register_connection("iPhone 15");
        let (other, _, _) = manager.register_connection("iPad");
        let mut rig = Rig::new(mine);
        let t0 = Instant::now();

        // No session has ever been live.
        rig.chunk(&manager, 320, t0);
        assert_eq!(rig.admitted_samples, 0);
        assert_eq!(rig.out_of_session_chunks, 1);

        // Another phone's session is live: spectator audio stays out.
        commit_session_start(app.handle(), other);
        rig.chunk(&manager, 320, t0 + Duration::from_millis(50));
        assert_eq!(
            rig.admitted_samples, 0,
            "a spectator must not feed another phone's dictation"
        );
        assert_eq!(rig.out_of_session_chunks, 2);

        // That session ends (and a keyboard dictation in between changes
        // nothing: companion session state stays false throughout, which
        // is exactly what this lane reads).
        commit_session_end(app.handle(), other);
        rig.chunk(&manager, 320, t0 + Duration::from_millis(100));
        assert_eq!(rig.admitted_samples, 0);
        assert_eq!(rig.out_of_session_chunks, 3);

        // Our own session opens admission...
        commit_session_start(app.handle(), mine);
        rig.chunk(&manager, 320, t0 + Duration::from_millis(150));
        assert_eq!(rig.admitted_samples, 320);

        // ...and the stale lane of our ended session never re-opens it.
        commit_session_end(app.handle(), mine);
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
        let (lane, _, _) = manager.register_connection("Pixel 8");
        let mut rig = Rig::new(lane);
        let t0 = Instant::now();

        // The tap that locks on, then fifteen minutes of live session.
        rig.ptt(&manager, true, t0);
        commit_session_start(app.handle(), lane);
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

    /// KB-272 pin (v1.6.3): an admitted chunk whose push into the lane's
    /// ring fails is COUNTED, never silently swallowed. Before this, the
    /// only call site ignored push_remote_chunk's bool, so a recorder-
    /// lifecycle drop of admitted audio produced no signal at all - the
    /// exact failure class the v1.6.3 forensic pass had to rule out by
    /// hand. Asserted at the connection loop's own seam: the dropped
    /// chunks are separated from delivered samples AND from out-of-session
    /// chunks, so the disconnect log line can name them.
    #[test]
    fn admitted_chunks_the_lane_recorder_refuses_are_counted_not_swallowed() {
        let app = tauri::test::mock_app();
        let manager = Arc::new(CompanionManager::new());
        app.manage(Arc::clone(&manager));
        let (lane, _, _) = manager.register_connection("Pixel 8");
        let mut rig = Rig::new(lane);
        let t0 = Instant::now();

        // A healthy hold session: press, start commits, speech flows.
        rig.ptt(&manager, true, t0);
        commit_session_start(app.handle(), lane);
        rig.chunk(&manager, 640, t0 + Duration::from_millis(50));
        assert_eq!(rig.admitted_samples, 640);
        assert_eq!(rig.admitted_dropped_chunks, 0);

        // The lane's recorder dies mid-session (never pre-warmed back, or
        // its session already closed it): push_remote_chunk returns false
        // for every further chunk. Admission still says live (the Mac's
        // session state has not flipped), so these chunks are ADMITTED
        // yet undeliverable - the KB-272 class. They must land in their
        // own counter, not in the delivered samples, not in the
        // out-of-session count.
        for i in 0..5 {
            rig.chunk_delivery(
                &manager,
                640,
                t0 + Duration::from_millis(100 + 40 * i),
                false,
            );
        }
        assert_eq!(
            rig.admitted_samples, 640,
            "undeliverable chunks never count as delivered samples"
        );
        assert_eq!(rig.admitted_dropped_chunks, 5);
        assert_eq!(
            rig.out_of_session_chunks, 0,
            "an admitted-but-dropped chunk is NOT an out-of-session chunk"
        );

        // The session ends; post-session audio keeps its own bucket.
        commit_session_end(app.handle(), lane);
        rig.chunk(&manager, 640, t0 + Duration::from_secs(2));
        assert_eq!(rig.admitted_dropped_chunks, 5);
        assert_eq!(rig.out_of_session_chunks, 1);
    }

    /// KB-272 source pin (the interleave needs real recorders, so the call
    /// site is pinned against this committed source, the audio.rs
    /// convention): the binary handler must CONSUME push_remote_chunk's
    /// return into the admitted-dropped counter and warn at the first
    /// drop, and the disconnect path must surface the counter - the exact
    /// regression (an ignored bool) may never come back unnoticed.
    #[test]
    fn binary_handler_consumes_push_remote_chunks_return_and_disconnect_surfaces_it() {
        // Production source only: include_str! would otherwise also read
        // this test module, whose own literals would self-match.
        let source = include_str!("server.rs");
        let production = &source[..source.find("#[cfg(test)]").expect("the test module")];
        // The call site: admitted branch pushes and handles the bool.
        let push_at = production
            .find(".map(|rm| rm.push_remote_chunk(device_lane, &samples))")
            .expect("the push_remote_chunk call site");
        let admitted_arm = &production[push_at..push_at + 600];
        assert!(
            admitted_arm.contains(".unwrap_or(false)"),
            "the push outcome must be a bool, not ()"
        );
        assert!(
            admitted_arm.contains("if !delivered {"),
            "the false arm must be explicit"
        );
        assert!(
            admitted_arm.contains("admitted_dropped_chunks += 1"),
            "a refused admitted chunk must be counted"
        );
        // No silent `rm.push_remote_chunk(` discard remains in production.
        assert!(
            !production.contains("rm.push_remote_chunk(device_lane, &samples);"),
            "the bare statement form (return value dropped) may not remain"
        );
        // The disconnect path surfaces the counter.
        let disconnect_at = production
            .find("if admitted_dropped_chunks > 0 {")
            .expect("the disconnect surfacing");
        let disconnect_arm = &production[disconnect_at..disconnect_at + 500];
        assert!(
            disconnect_arm.contains("admitted_dropped_chunks} admitted audio chunk(s)"),
            "the disconnect log line names the count"
        );
    }

    /// KB-263 source pin (the interleave needs real sockets): the
    /// disconnect path performs NO pause acknowledgement of any shape.
    /// Fleet pass 2: the disconnect finalize is extracted into
    /// finalize_disconnected_connection (so the Welcome-failure exit runs
    /// it too), and this pin slices THAT fn - the whole disconnect path
    /// now lives there. The pre-fix eager `ack_remote_pause` ran before
    /// any Stop raised the request and was inert (an ack only counts once
    /// `pause_requested` is live, and the Stop that follows wipes any
    /// earlier one - pinned by the recorder test
    /// kb263_an_ack_before_any_stop_raises_the_pause_request_is_wiped);
    /// a deferred "ack once requested" watcher is equally dead, because
    /// every lane stop shape removes the map entry and closes the source
    /// BEFORE the recorder raises the request (pinned in audio.rs by
    /// lane_stops_run_outside_the_lanes_lock_and_close_the_source_first),
    /// so no ack route exists at the moment the request goes live. The
    /// contract this pins instead: the source close IS the acknowledgement
    /// (producer_gone makes the consumer skip the 2 s wait - pinned at
    /// recorder level by
    /// remote_stop_after_source_close_skips_the_pause_wait_entirely), so
    /// the disconnect path must not grow any ack machinery again.
    #[test]
    fn kb263_disconnect_performs_no_pause_ack_the_source_close_is_the_ack() {
        let source = include_str!("server.rs");
        let production = &source[..source.find("#[cfg(test)]").expect("the test module")];
        let disconnect_section_at = production
            .find("async fn finalize_disconnected_connection")
            .expect("the disconnect finalize fn");
        let disconnect_section = &production[disconnect_section_at..];

        // No pause-ack machinery of any shape on the disconnect path.
        assert!(
            !disconnect_section.contains("ack_remote_pause"),
            "the disconnect path must not acknowledge the stop-pause: every lane stop shape closes the source first, and the producer-gone close is the immediate acknowledgement"
        );
        assert!(
            !disconnect_section.contains("ack_pause"),
            "no direct source-level ack either"
        );
        // The disconnect still retires the idle lane recorder (the RAM
        // rule that always lived beside the old ack).
        assert!(
            disconnect_section.contains("rm.close_remote_lane(device_lane);"),
            "the idle lane recorder is still retired at disconnect"
        );

        // The covering contract is pinned where it lives: the audio-side
        // ordering (source closed before every recorder stop) must exist
        // in audio.rs, and the recorder must skip the pause wait for a
        // gone producer. Cross-file guards so this pin cannot drift from
        // the mechanism it relies on.
        let audio = include_str!("../managers/audio.rs");
        let ordering_pin_at = audio
            .find("fn lane_stops_run_outside_the_lanes_lock_and_close_the_source_first()")
            .expect("the audio-side close-first ordering pin");
        assert!(
            audio[..ordering_pin_at].contains("entry.source.close();"),
            "the production lane stop closes the source before the recorder stop"
        );
        let recorder = include_str!("../audio_toolkit/audio/recorder/tests.rs");
        assert!(
            recorder.contains("fn remote_stop_after_source_close_skips_the_pause_wait_entirely()"),
            "the producer-gone fast-return pin must stay in the recorder suite"
        );
        assert!(
            recorder
                .contains("fn kb263_an_ack_before_any_stop_raises_the_pause_request_is_wiped()"),
            "the early-ack-is-wiped pin must stay in the recorder suite"
        );
    }

    /// KB-270 source pin: the disconnect path must tell the coordinator the
    /// lane is gone right after releasing its registration, so the
    /// coordinator can retire the lane's machine (immediately when idle,
    /// at the drain when a pipeline is in flight) instead of growing its
    /// lanes map forever on a flapping phone.
    #[test]
    fn kb270_disconnect_notifies_the_coordinator_the_lane_is_gone() {
        let source = include_str!("server.rs");
        let production = &source[..source.find("#[cfg(test)]").expect("the test module")];
        let unregister_at = production
            .find("manager.unregister_connection(device_lane, device_generation);")
            .expect("the registration release");
        let notify_at = production
            .find("coordinator.notify_lane_gone(device_lane);")
            .expect("the lane-gone notification");
        assert!(
            unregister_at < notify_at,
            "the lane-gone notification follows the registration release"
        );
    }

    /// v1.6.4 accept-wedge hardening, source pin (a live reproduction
    /// needs the real TLS server on a Wry app, which the unit harness
    /// cannot mount). SCOPE (fleet pass 2, after the blanket claim was
    /// found false; seam count narrowed by KB-291 in v1.6.6, which
    /// deleted the persist_known_identities worker): this pin checks
    /// exactly the seams it enumerates - (1) the SQLite recent-history
    /// query at hello, (2) the connect-edge settings+tray bookkeeping,
    /// (3) the disconnect-edge tray re-sync, each riding the
    /// panic-capturing spawn_worker; (4) the Welcome frame's settings
    /// read riding spawn_blocking (fleet pass 2: it sat inline on the
    /// async task through the original hardening, and get_settings is a
    /// read-modify-write over the plugin store's mutex, not a pure
    /// read); and (5) the Ptt-edge settings read riding spawn_blocking
    /// (KB-285 in v1.6.6: send_companion_edge read the store inline on
    /// this task for every phone press/release edge - the same
    /// migration/salvage write-back wedge). It does NOT certify the
    /// absence of other blocking calls; the async task itself must stay
    /// free of direct get_settings/write_settings only in the seams
    /// named here.
    #[test]
    fn connection_blocking_seams_run_off_the_async_connection_task() {
        let source = include_str!("server.rs");
        let production = &source[..source.find("#[cfg(test)]").expect("the test module")];

        // The three detached connect/disconnect seams ride spawn_worker by
        // name (also the fleet pass 2 panic-capture pin: a worker panic
        // must surface in the app log, not raw stderr).
        for (name, call) in [
            (
                "note_device_connected",
                "manager.note_device_connected(&app, &connected_device);",
            ),
            (
                "send_recent_history_to_lane",
                "crate::companion::send_recent_history_to_lane(",
            ),
            (
                "note_device_disconnected",
                "disconnected_manager.note_device_disconnected(&app);",
            ),
        ] {
            let call_at = production
                .find(call)
                .unwrap_or_else(|| panic!("the seam call must exist: {call}"));
            let head = production[..call_at]
                .rfind("spawn_worker(\"")
                .expect("the seam must be introduced by the panic-capturing spawn_worker helper");
            assert!(
                production[head..].starts_with(&format!("spawn_worker(\"{name}\", move || {{")),
                "the {name} worker must ride spawn_worker with its name (panic-capture + seam routing)"
            );
        }
        // KB-291: the deleted persist worker may not return either (the
        // needle is concatenated so the history comments that name the
        // deleted worker cannot satisfy it).
        let persist_spawn = ["spawn_worker(\"persist_", "known_identities\""].concat();
        assert!(
            !production.contains(&persist_spawn),
            "the known-identity persist worker is deleted machinery (KB-291)"
        );
        // And the helper itself catches panics into the app log.
        let helper_at = production
            .find("fn spawn_worker(")
            .expect("the panic-capturing worker helper");
        let helper_end = helper_at + production[helper_at..].find("\n}\n").expect("fn end");
        let helper = &production[helper_at..helper_end];
        assert!(
            helper.contains("catch_unwind") && helper.contains("log::error!"),
            "a worker panic must be caught and logged, never stderr-only"
        );

        // (4) The Welcome settings read rides spawn_blocking, and the old
        // inline form is gone.
        let welcome_read_at = production
            .find("let settings = crate::settings::get_settings(&welcome_app);")
            .expect("the welcome settings read");
        let blocking_at = production
            .find("tauri::async_runtime::spawn_blocking(move || {")
            .expect("the blocking-pool dispatch");
        assert!(
            blocking_at < welcome_read_at,
            "the Welcome settings read must run on the blocking pool, never the async task"
        );
        assert!(
            !production.contains("let settings = crate::settings::get_settings(&app);"),
            "the direct (blocking) settings read may not remain on the async task"
        );

        // (5) KB-285: the Ptt-edge settings read rides spawn_blocking too.
        // The read must sit inside its own blocking-pool dispatch (the
        // nearest preceding one, distinct from the Welcome seam's), and
        // the old inline edge call (the coordinator reading the store
        // itself) may not return.
        let edge_read_at = production
            .find("crate::settings::get_settings(&edge_app)")
            .expect("the ptt-edge settings read");
        let edge_dispatch_before = production[..edge_read_at]
            .rfind("tauri::async_runtime::spawn_blocking(move || {")
            .expect("the ptt-edge read must be introduced by a blocking-pool dispatch");
        assert!(
            edge_dispatch_before > blocking_at,
            "the Ptt-edge settings read must have its own blocking-pool dispatch after the Welcome seam's (never the async task)"
        );
        assert!(
            !production
                .contains("coordinator.send_companion_edge(&app, pressed, prefs, device_lane);"),
            "the direct edge call that read the store on the async task may not return (KB-285)"
        );
        // Cross-file: the coordinator's edge forwarder must not read the
        // store itself (reintroducing the read there silently moves the
        // blocking IO back onto the connection task).
        let coordinator_source = include_str!("../transcription_coordinator.rs");
        let edge_at = coordinator_source
            .find("pub fn send_companion_edge(")
            .expect("the edge forwarder");
        let edge_end = edge_at
            + coordinator_source[edge_at..]
                .find("\n    }\n")
                .expect("fn end");
        let edge_body = &coordinator_source[edge_at..edge_end];
        assert!(
            !edge_body.contains("get_settings"),
            "send_companion_edge must not read the settings store; its caller resolves the values off the async task (KB-285)"
        );

        let manager_source = include_str!("mod.rs");
        let register_at = manager_source
            .find("pub fn register_connection_with_identity(")
            .expect("the identity registration");
        let register_end = register_at
            + manager_source[register_at..]
                .find("\n    }\n")
                .expect("fn end");
        let register_body = &manager_source[register_at..register_end];
        assert!(
            !register_body.contains("get_settings") && !register_body.contains("write_settings"),
            "the registration must stay in-memory only (no settings IO on the async task)"
        );
        assert!(
            register_body.contains("let mut connections"),
            "the registration holds the connections registry guard"
        );
        let push_at = register_body
            .find("connections.push(ConnectionEntry {")
            .expect("the registration insert");
        let drop_at = register_body
            .find("drop(connections);")
            .expect("the guard releases after the insert");
        assert!(
            push_at < drop_at,
            "the registration insert happens inside the guard's critical section (the guard releases only after the insert)"
        );

        // None of the blocking seams remain as direct calls on the async
        // task.
        assert!(
            !production.contains("send_recent_history_to_lane(&app, device_lane, &device_name);"),
            "the direct (blocking) history call may not remain on the async task"
        );
        assert!(
            !production.contains("manager.note_device_connected(&app, &device_name);"),
            "the direct (blocking) connect-edge call may not remain on the async task"
        );
        assert!(
            !production
                .lines()
                .any(|line| line.trim() == "manager.note_device_disconnected(&app);"),
            "the direct (blocking) disconnect-edge call may not remain on the async task (the worker's own call goes through the disconnected_manager binding)"
        );
    }

    /// Fleet pass 2 (KB-270 recovery gap), source pin: the ONE
    /// post-registration early exit - the Welcome send failing because
    /// the phone died between hello and welcome - must run the disconnect
    /// finalize (registration release, coordinator lane retirement, tray
    /// re-sync, recorder teardown) before returning, instead of the old
    /// silent bare return that leaked the connections entry and the lane
    /// bookkeeping forever. The exit must also log: every other exit on
    /// the path does.
    #[test]
    fn welcome_send_failure_finalizes_the_lane_instead_of_leaking_it() {
        let source = include_str!("server.rs");
        let production = &source[..source.find("#[cfg(test)]").expect("the test module")];
        let fail_arm_at = production
            .find("if sink.send(Message::text(welcome.to_json())).await.is_err() {")
            .expect("the welcome-failure arm");
        let fail_arm = &production[fail_arm_at..fail_arm_at + 1200];
        let log_at = fail_arm
            .find("dropped before its Welcome")
            .expect("the welcome-failure exit must log");
        let finalize_at = fail_arm
            .find("finalize_disconnected_connection(")
            .expect("the welcome-failure exit must run the disconnect finalize");
        let ret_at = fail_arm
            .find("return;")
            .expect("the welcome-failure exit returns after the finalize");
        assert!(
            log_at < finalize_at && finalize_at < ret_at,
            "the arm must log, finalize, and only then return"
        );

        // And the session loop's tail routes through the SAME finalize fn
        // (one teardown, two entry points), so the tail cannot drift from
        // the welcome-failure path.
        let tail_at = production
            .find("---- disconnect: graceful finalize ----")
            .expect("the session-loop tail");
        assert!(
            production[tail_at..].contains("finalize_disconnected_connection("),
            "the session-loop tail must route through the same finalize fn"
        );
    }
}
