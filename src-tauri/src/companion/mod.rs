//! Companion devices: use a phone or tablet on the same Wi-Fi as a remote
//! microphone and push-to-talk trigger, with this Mac as the engine.
//!
//! Everything here is gated on the OFF-by-default
//! `companion_devices_enabled` setting. The off path leaves no listener,
//! no threads, and the local recorder path untouched. While enabled, a
//! small TLS server (self-signed cert, fingerprint shown in Settings)
//! serves a single-file web client and speaks a WebSocket that carries
//! PTT control frames and raw 16 kHz f32 audio into the SAME recorder,
//! coordinator, and transcription pipeline the keyboard hotkey uses.

pub mod auth;
pub mod cert;
pub mod client_page;
pub mod ip;
pub mod protocol;
pub mod server;
mod strings_gen;

use crate::managers::transcription::{emit_overlay_notice, NoticeCode};
use serde::Serialize;
use specta::Type;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Listener, Manager};
use tokio::sync::watch;

/// macOS-first this cycle: the module compiles everywhere (the Linux
/// getifaddrs path included), but the server reports unavailable off
/// macOS.
pub fn platform_supported() -> bool {
    cfg!(target_os = "macos")
}

/// The status snapshot the settings panel renders (QR, URL, devices,
/// fingerprint, error).
#[derive(Serialize, Type, Debug, Clone)]
pub struct CompanionStatus {
    pub enabled: bool,
    pub running: bool,
    pub supported: bool,
    pub url: Option<String>,
    pub port: u16,
    pub qr_svg: Option<String>,
    pub fingerprint: Option<String>,
    pub token: Option<String>,
    pub subnet: Option<String>,
    pub last_device: Option<String>,
    pub devices: Vec<String>,
    pub error: Option<String>,
    /// v1.6.0: every candidate interface the picker can surface, with
    /// its OS name and classification ("lan" | "tailscale" | "vpn").
    pub interfaces: Vec<CompanionInterface>,
    /// The IPv4 the current configuration resolves to bind (live while
    /// running, a preview of the next start while stopped). None when
    /// the configured choice cannot resolve.
    pub bind_ip: Option<String>,
    /// The manual hostname/IP override for the pairing URL, when set.
    pub advertised_host: Option<String>,
    /// Why `bind_ip` could not resolve (a chosen interface that is no
    /// longer present, or a malformed stored value).
    pub bind_error: Option<String>,
    /// True when the running server was started with a manual
    /// certificate pair rather than the self-signed one.
    pub tls_manual: bool,
    /// True while the server runs with a configuration the persisted
    /// settings no longer match (interface, host, port, or TLS source
    /// changed since start): a restart applies them.
    pub restart_pending: bool,
}

/// One picker row: an interface address with its name and kind.
#[derive(Serialize, Type, Debug, Clone)]
pub struct CompanionInterface {
    pub ip: String,
    pub name: String,
    pub kind: String,
}

impl From<&ip::InterfaceCandidate> for CompanionInterface {
    fn from(c: &ip::InterfaceCandidate) -> Self {
        CompanionInterface {
            ip: c.ip.to_string(),
            name: c.name.clone(),
            kind: c.kind.as_str().to_string(),
        }
    }
}

struct CompanionInner {
    running: bool,
    bind_ip: Option<Ipv4Addr>,
    port: u16,
    fingerprint: Option<String>,
    last_error: Option<String>,
    devices: Vec<String>,
    /// v1.6.0: the configuration snapshot the running server was started
    /// with, so status() can tell a restart is pending.
    advertised_host: Option<String>,
    tls_manual: bool,
}

pub struct CompanionManager {
    inner: Mutex<CompanionInner>,
    /// Server frames fanned out to every live client connection.
    broadcast: tokio::sync::broadcast::Sender<String>,
    /// Signalled (true) to stop the accept loop and every connection.
    shutdown_tx: watch::Sender<bool>,
    /// True while a companion recording session is live (for the phone's
    /// state and the disconnect-finalize decision).
    session_live: AtomicBool,
    /// The device name behind the live session, if any.
    session_device: Mutex<Option<String>>,
    /// v1.6.0: the preferences the NEXT companion session will run with.
    /// Armed by the press edge that starts it (server.rs), consumed when
    /// the session actually starts.
    pending_prefs: Mutex<protocol::SessionPrefs>,
    /// v1.6.0: the preferences of the live (or most recently finished)
    /// companion session, frozen at session start so nothing can retarget a
    /// session that is already recording or transcribing. The stop pipeline
    /// reads this to decide paste-vs-phone delivery and the post-process
    /// leg. Defaults reproduce the pre-v1.6.0 behavior exactly.
    live_prefs: Mutex<protocol::SessionPrefs>,
    /// Per-IP auth-failure limiting shared across connections.
    failure_limiter: Mutex<auth::FailureLimiter>,
    /// The Rust-side notice listener id (registered once at init).
    notice_listener: Mutex<Option<tauri::EventId>>,
}

impl CompanionManager {
    pub fn new() -> Self {
        let (broadcast, _) = tokio::sync::broadcast::channel(64);
        let (shutdown_tx, _) = watch::channel(false);
        CompanionManager {
            inner: Mutex::new(CompanionInner {
                running: false,
                bind_ip: None,
                port: 0,
                fingerprint: None,
                last_error: None,
                devices: Vec::new(),
                advertised_host: None,
                tls_manual: false,
            }),
            broadcast,
            shutdown_tx,
            session_live: AtomicBool::new(false),
            session_device: Mutex::new(None),
            pending_prefs: Mutex::new(protocol::SessionPrefs::default()),
            live_prefs: Mutex::new(protocol::SessionPrefs::default()),
            failure_limiter: Mutex::new(auth::FailureLimiter::new(
                5,
                Duration::from_secs(60),
                Duration::from_secs(60),
            )),
            notice_listener: Mutex::new(None),
        }
    }

    pub fn is_running(&self) -> bool {
        self.inner.lock().unwrap().running
    }

    pub fn session_live(&self) -> bool {
        self.session_live.load(Ordering::Acquire)
    }

    /// The device name of the live (or most recent) session.
    pub fn session_device(&self) -> Option<String> {
        self.session_device.lock().unwrap().clone()
    }

    /// KB-197: an authenticated hello claims the session-device slot - the
    /// source `on_session_changed` badges the session with and blames on
    /// cap/disconnect notices - but ONLY while no session is live. Once a
    /// phone is dictating, an additional hello is a spectator or a
    /// reconnect: overwriting the slot would rename the session after the
    /// fact (and the next session's badge) to a phone that never pressed.
    /// A same-name reconnect mid-session is a no-op by the same guard.
    pub fn claim_session_device(&self, name: &str) {
        if self.session_live() {
            return;
        }
        *self.session_device.lock().unwrap() = Some(name.to_string());
    }

    /// v1.6.0: arm the preferences the next companion session will run
    /// with. Called by the server on the press edge that starts a session
    /// (the release edge never touches them: the session's shape was fixed
    /// by the press that began it).
    pub fn set_pending_session_prefs(&self, prefs: protocol::SessionPrefs) {
        *self.pending_prefs.lock().unwrap() = prefs;
    }

    /// v1.6.0: the preferences of the live (or most recently finished)
    /// companion session. Frozen at session start, so a later press edge
    /// (busy-remembered or on another phone) cannot retarget a session that
    /// is already recording or mid-pipeline.
    pub fn live_session_prefs(&self) -> protocol::SessionPrefs {
        *self.live_prefs.lock().unwrap()
    }

    /// v1.6.0: freeze the pending prefs as the live session's. Called from
    /// [`on_session_changed`] exactly when a companion recording begins.
    fn freeze_session_prefs(&self) {
        let pending = *self.pending_prefs.lock().unwrap();
        *self.live_prefs.lock().unwrap() = pending;
    }

    pub fn broadcast_frame(&self, frame: &protocol::ServerFrame) {
        // A send with no subscribers is an expected error, not a failure.
        let _ = self.broadcast.send(frame.to_json());
    }

    pub fn broadcast_text(&self, text: String) {
        let _ = self.broadcast.send(text);
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<String> {
        self.broadcast.subscribe()
    }

    pub fn shutdown_signal(&self) -> watch::Receiver<bool> {
        self.shutdown_tx.subscribe()
    }

    pub fn failure_limiter_pin(&self) -> &Mutex<auth::FailureLimiter> {
        &self.failure_limiter
    }

    /// Remember a hello'd device (deduplicated) and persist the name as
    /// `companion_last_device`.
    pub fn remember_device(&self, app: &AppHandle, name: &str) {
        {
            let mut inner = self.inner.lock().unwrap();
            if !inner.devices.iter().any(|d| d == name) {
                inner.devices.push(name.to_string());
            }
        }
        let mut settings = crate::settings::get_settings(app);
        if settings.companion_last_device.as_deref() != Some(name) {
            settings.companion_last_device = Some(name.to_string());
            crate::settings::write_settings(app, settings);
        }
    }

    pub fn forget_device(&self, name: &str) {
        self.inner.lock().unwrap().devices.retain(|d| d != name);
    }

    /// Start the server. Idempotent. Resolves the bind address from the
    /// picker choice or the automatic LAN-preferred ranking (v1.6.0),
    /// generates/repairs the pairing token (rebinding it to the
    /// advertised interface's subnet, which re-pairs phones after a
    /// network change), loads the TLS certificate (a manual pair when
    /// configured, the self-signed one otherwise), binds the listener,
    /// and pre-warms the remote recorder so a phone press starts capture
    /// fast. Bind failures (a taken port among them) surface verbatim:
    /// the ready channel carries the OS error string.
    pub fn start(&self, app: &AppHandle) -> Result<(), String> {
        if !platform_supported() {
            return Err("Companion devices need macOS this cycle".to_string());
        }
        if self.is_running() {
            return Ok(());
        }

        let settings = crate::settings::get_settings(app);
        let candidates = ip::interface_candidates();
        let bind_ip = ip::resolve_bind_ip(&candidates, settings.companion_bind_ip.as_deref())?;
        let advertised_host =
            effective_advertised_host(settings.companion_advertised_host.as_deref());

        // Pairing token: generate on enable, and rebind when the advertised
        // subnet changed (a token from another network never validates).
        // A Tailscale bind issues the token for the whole CGNAT /10, so
        // every tailnet peer validates (ip::pairing_subnet_for).
        let subnet = ip::pairing_subnet_for(bind_ip);
        {
            let mut settings = crate::settings::get_settings(app);
            let token_stale = settings
                .companion_pairing_subnet
                .as_deref()
                .map(|s| s != subnet)
                .unwrap_or(true)
                || settings
                    .companion_pairing_token
                    .as_deref()
                    .map(|t| t.is_empty())
                    .unwrap_or(true);
            if token_stale {
                settings.companion_pairing_token = Some(auth::generate_pairing_token());
                settings.companion_pairing_subnet = Some(subnet.clone());
                crate::settings::write_settings(app, settings);
            }
        }
        let settings = crate::settings::get_settings(app);
        let token = settings.companion_pairing_token.clone().unwrap_or_default();
        let port = settings.companion_port;

        // v1.6.0 TLS source: a manual certificate pair when BOTH paths are
        // set (tailscale-issued or own certs), the self-signed one
        // otherwise. A configured pair that fails to load is an error,
        // never a silent self-signed fallback: the user pointed at real
        // certificates and must hear why they are not being served.
        let manual_cert = match (
            settings
                .companion_tls_cert_path
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
            settings
                .companion_tls_key_path
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
        ) {
            (Some(cert_path), Some(key_path)) => {
                Some(cert::load_manual_pair(std::path::Path::new(cert_path), std::path::Path::new(key_path))?)
            }
            (None, None) => None,
            (single, _) => {
                // Unreachable through the settings UI (the change command
                // validates both-or-neither); a hand-edited store lands
                // here and keeps the server usable on the self-signed
                // cert, loudly.
                log::warn!(
                    "companion: only one TLS path set ({single:?}); using the self-signed certificate"
                );
                None
            }
        };
        let tls_manual = manual_cert.is_some();

        // TLS certificate (generated once, persisted).
        let cert = match manual_cert {
            Some(cert) => cert,
            None => {
                let cert_dir = app
                    .path()
                    .app_local_data_dir()
                    .map_err(|e| format!("app data dir unavailable: {e}"))?
                    .join("companion");
                cert::ensure_certificate(&cert_dir, std::net::IpAddr::V4(bind_ip))?
            }
        };
        let fingerprint = cert.fingerprint.clone();

        // Bind synchronously so the settings toggle reports a real outcome
        // (macOS firewall prompt aside, a bind error surfaces verbatim).
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let cfg = server::ServerConfig {
            ip: bind_ip,
            port,
            token,
            subnet: subnet.clone(),
        };
        let app_clone = app.clone();
        tauri::async_runtime::spawn(server::run(
            app_clone,
            cfg,
            cert,
            self.shutdown_signal(),
            ready_tx,
        ));
        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err("Companion server did not start in time".to_string()),
        }

        {
            let mut inner = self.inner.lock().unwrap();
            inner.running = true;
            inner.bind_ip = Some(bind_ip);
            inner.port = port;
            inner.fingerprint = Some(fingerprint);
            inner.advertised_host = advertised_host.clone();
            inner.tls_manual = tls_manual;
        }

        // Pre-warm the remote recorder (VAD load off the press path).
        if let Some(rm) = app.try_state::<Arc<crate::managers::audio::AudioRecordingManager>>() {
            let rm = Arc::clone(&rm);
            let app_bg = app.clone();
            std::thread::spawn(move || {
                if let Err(e) = rm.prewarm_remote() {
                    log::debug!("companion: remote recorder pre-warm failed: {e}");
                    let _ = app_bg;
                }
            });
        }

        log::info!(
            "companion: server running on https://{bind_ip}:{port} (subnet {subnet}, advertised as {})",
            advertised_host.as_deref().unwrap_or("the bind IP")
        );
        Ok(())
    }

    /// Stop the server: finalize a live companion session first (the forced
    /// finalize runs the ordinary Stop pipeline, which closes the remote
    /// recorder itself), then close the listener and all connections.
    /// Idempotent.
    pub fn stop(&self, app: &AppHandle) {
        if !self.is_running() {
            return;
        }

        // Graceful finalize: if a phone session is live, finalize it so
        // everything captured is transcribed and pasted. Forced rather than
        // a synthesized release edge: a locked (toggle) session ignores
        // release edges by design and would otherwise strand the recording.
        if let Some(coordinator) = app.try_state::<crate::TranscriptionCoordinator>() {
            if self.session_live() {
                coordinator.finalize_companion_session();
            }
        }
        self.session_live.store(false, Ordering::Release);
        *self.session_device.lock().unwrap() = None;

        self.broadcast_frame(&protocol::ServerFrame::Goodbye {
            reason: "server_stopping",
        });
        let _ = self.shutdown_tx.send(true);

        // Close the pre-warmed remote recorder unless the stop pipeline is
        // using it right now (it closes it at its own end).
        if let Some(rm) = app.try_state::<Arc<crate::managers::audio::AudioRecordingManager>>() {
            if !rm.is_recording() {
                rm.close_remote_stream();
            }
        }

        let mut inner = self.inner.lock().unwrap();
        inner.running = false;
        inner.bind_ip = None;
        inner.port = 0;
        inner.advertised_host = None;
        inner.tls_manual = false;
        inner.devices.clear();
        log::info!("companion: server stopped");
    }

    /// v1.6.0: apply the persisted configuration (interface choice,
    /// advertised host, port, TLS pair) by bouncing the server. Only a
    /// running server restarts; a stopped one starts when the effective
    /// gate allows it, so the button doubles as "apply now" after a
    /// failed start. Start failures surface verbatim (bind errors
    /// included) and land on the panel's error line.
    pub fn restart(&self, app: &AppHandle) -> Result<(), String> {
        if self.is_running() {
            self.stop(app);
            // stop() finalized any live session; give the listener a beat
            // to release the port before rebinding it.
            std::thread::sleep(Duration::from_millis(150));
        }
        if self.is_running() {
            return Ok(());
        }
        if effective_enabled(&crate::settings::get_settings(app)) {
            self.start(app)
        } else {
            Ok(())
        }
    }

    /// Status snapshot for the settings panel (QR included). The URL and
    /// QR are derived from the CURRENT persisted configuration (the
    /// interface choice, the advertised-host override, and the port), so
    /// the panel previews what the next start will advertise even while
    /// the server is stopped; `restart_pending` flags a running server
    /// started with a since-changed configuration, and a chosen
    /// interface that no longer resolves keeps the LIVE values with the
    /// resolution error on `bind_error`. The QR is suppressed while the
    /// stored pairing token is bound to a different subnet than the
    /// effective address implies (advertised_url_qr): the next start
    /// regenerates that token, so advertising the old one would pair
    /// phones against a dead token.
    pub fn status(&self, app: &AppHandle) -> CompanionStatus {
        let settings = crate::settings::get_settings(app);
        let candidates = ip::interface_candidates();
        let advertised_host = effective_advertised_host(settings.companion_advertised_host.as_deref());
        let resolved = ip::resolve_bind_ip(&candidates, settings.companion_bind_ip.as_deref());
        let inner = self.inner.lock().unwrap();
        let supported = platform_supported();
        // Config-derived on purpose: the URL, QR, and this port describe
        // what the NEXT start advertises (the preview the panel is for),
        // and restart_pending flags a running server that still serves
        // the old configuration.
        let port = settings.companion_port;
        let token = settings.companion_pairing_token.clone();

        // The configuration the URL advertises: the resolved choice when
        // it resolves; while running with an unresolvable choice, the
        // live values instead (the running server is still on air).
        let (effective_ip, bind_error) = match (&resolved, inner.running) {
            (Ok(ip), _) => (Some(*ip), None),
            (Err(e), true) => (inner.bind_ip, Some(e.clone())),
            (Err(e), false) => (None, Some(e.clone())),
        };

        let (url, qr_svg) = advertised_url_qr(
            advertised_host.as_deref(),
            effective_ip,
            token.as_deref(),
            settings.companion_pairing_subnet.as_deref(),
            port,
        );

        let restart_pending = inner.running && {
            let live = inner
                .bind_ip
                .map(|bind_ip| LiveServerConfig {
                    bind_ip,
                    port: inner.port,
                    advertised_host: inner.advertised_host.clone(),
                    tls_manual: inner.tls_manual,
                })
                .map(|live| {
                    restart_needed(
                        &resolved,
                        settings.companion_port,
                        advertised_host.as_deref(),
                        tls_manual_configured(
                            settings.companion_tls_cert_path.as_deref(),
                            settings.companion_tls_key_path.as_deref(),
                        ),
                        &live,
                    )
                });
            // No live bind recorded cannot happen while running; treat it
            // as "nothing to apply" rather than a spurious badge.
            live.unwrap_or(false)
        };

        CompanionStatus {
            enabled: settings.companion_devices_enabled,
            running: inner.running,
            supported,
            url,
            port,
            qr_svg,
            fingerprint: inner.fingerprint.clone(),
            token,
            subnet: settings.companion_pairing_subnet.clone(),
            last_device: settings.companion_last_device.clone(),
            devices: inner.devices.clone(),
            error: inner.last_error.clone(),
            interfaces: candidates.iter().map(CompanionInterface::from).collect(),
            bind_ip: effective_ip.map(|ip| ip.to_string()),
            advertised_host,
            bind_error,
            tls_manual: inner.tls_manual,
            restart_pending,
        }
    }

    pub fn set_last_error(&self, error: Option<String>) {
        self.inner.lock().unwrap().last_error = error;
    }

    /// Reset pairing: fresh token (rebound to the advertised interface's
    /// subnet, honoring the v1.6.0 interface choice) and a restart so the
    /// QR changes. Phones holding the old token are refused.
    pub fn reset_pairing(&self, app: &AppHandle) -> Result<(), String> {
        let candidates = ip::interface_candidates();
        let bind_ip = ip::resolve_bind_ip(
            &candidates,
            crate::settings::get_settings(app).companion_bind_ip.as_deref(),
        )?;
        let subnet = ip::pairing_subnet_for(bind_ip);
        let mut settings = crate::settings::get_settings(app);
        settings.companion_pairing_token = Some(auth::generate_pairing_token());
        settings.companion_pairing_subnet = Some(subnet);
        crate::settings::write_settings(app, settings);

        self.restart(app)
    }

    /// Register the once-per-app notice forwarder (overlay notices ride to
    /// the paired phones too, so a swallowed busy press is visible on both
    /// surfaces).
    pub fn register_notice_forwarder(self: &Arc<Self>, app: &AppHandle) {
        let mut guard = self.notice_listener.lock().unwrap();
        if guard.is_some() {
            return;
        }
        let weak = Arc::downgrade(self);
        let id = app.listen("overlay-notice-event", move |event| {
            let Some(manager) = weak.upgrade() else {
                return;
            };
            if !manager.is_running() {
                return;
            }
            let payload = event.payload();
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) {
                if let Some(code) = value.get("code").and_then(|c| c.as_str()) {
                    // AUD-01: Mac-only feedback (keyboard notices the phone
                    // can neither trigger nor render) stops here; every
                    // other code, known or unknown, keeps riding.
                    if should_forward_notice_to_phones(code) {
                        manager.broadcast_frame(&protocol::ServerFrame::Notice {
                            code: code.to_string(),
                        });
                    }
                }
            }
        });
        *guard = Some(id);
    }
}

/// Overlay-notice codes that describe Mac-keyboard-only feedback and must
/// never ride to the paired phones. Membership rule (verified at each
/// emitter): the code fires only from a Mac keyboard action or desktop
/// surface (a ShortcutAction handler, the coordinator's command-mode press,
/// a tray/UI command), and the paired phone can neither trigger it (the
/// client speaks only hello/ptt frames) nor act on it (the client's notice
/// branch renders only the companion trio; every other code is a bare
/// 30 ms vibration), so forwarding would buzz the phone in a pocket with
/// nothing to show.
///
/// Denylist, not allowlist: every code NOT listed here, known or unknown,
/// errors included, keeps riding the forwarder, so phone behavior stays
/// otherwise identical.
const MAC_ONLY_NOTICE_CODES: &[&str] = &[
    // Command-mode modifier engaged with no live session (per press).
    "command_mode_no_session",
    // Delete-last-word key pressed with no live dictation session.
    "delete_last_word_no_session",
    // Undo key pressed with no live dictation session.
    "undo_no_session",
    // The cycle-prompt hotkey/tray command confirmed the next template.
    "post_process_prompt_cycled",
];

/// AUD-01: may this overlay-notice code be forwarded to the paired phones?
/// `false` is exactly the Mac-only skip set above; everything else
/// forwards. Pure so the forwarder's drop policy is testable without a
/// live TLS server.
pub fn should_forward_notice_to_phones(code: &str) -> bool {
    !MAC_ONLY_NOTICE_CODES.contains(&code)
}

impl Default for CompanionManager {
    fn default() -> Self {
        Self::new()
    }
}

fn qr_svg_for(url: &str) -> Result<String, String> {
    use qrcode::render::svg::Color;
    use qrcode::QrCode;
    let code = QrCode::new(url.as_bytes()).map_err(|e| format!("{e}"))?;
    Ok(code
        .render::<Color>()
        .min_dimensions(220, 220)
        .quiet_zone(true)
        .build())
}

// ---------------------------------------------------------------------------
// App-level helpers used by actions.rs and the settings commands.
// ---------------------------------------------------------------------------

/// Resolve the managed manager, if initialized (inert before core setup).
/// Runtime-generic so the v1.6.0 seams below can be exercised from a mock
/// app in tests (the same pattern manage_local_llm uses).
fn manager<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> Option<Arc<CompanionManager>> {
    app.try_state::<Arc<CompanionManager>>()
        .map(|s| s.inner().clone())
}

/// Apply the companion enabled state to the running server: enable starts
/// the listener, disable finalizes any live phone session and stops it. A
/// failed start returns Err (already logged, stored on the manager, and
/// badged on the overlay) so settings callers can apply-then-persist and
/// roll the toggle back (KB-027); the startup path ignores it.
pub fn apply_enabled(app: &AppHandle, enabled: bool) -> Result<(), String> {
    let Some(manager) = manager(app) else {
        return Ok(());
    };
    if enabled {
        if let Err(e) = manager.start(app) {
            log::error!("companion: failed to start server: {e}");
            manager.set_last_error(Some(e.clone()));
            emit_overlay_notice(app, NoticeCode::CompanionServerFailed, Some(e.clone()));
            return Err(e);
        }
        manager.set_last_error(None);
    } else {
        manager.stop(app);
        manager.set_last_error(None);
    }
    Ok(())
}

/// Companion session boundary, reported by the shared TranscribeAction:
/// badges the recording overlay (emit_to the overlay window, the
/// command-mode pattern) and broadcasts the state to paired phones.
pub fn on_session_changed(app: &AppHandle, active: bool) {
    if let Some(manager) = manager(app) {
        manager.session_live.store(active, Ordering::Release);
        if active {
            // v1.6.0: the session's prefs freeze here, at the single moment
            // the session begins. NOT cleared on the false edge: the stop
            // pipeline reads the frozen prefs after this broadcast (target
            // delivery, post-process leg), and no new companion session can
            // start until that pipeline drains (the coordinator's busy
            // arbitration), so the slot cannot go stale under it.
            manager.freeze_session_prefs();
            let device = manager
                .session_device()
                .or_else(|| crate::settings::get_settings(app).companion_last_device)
                .unwrap_or_else(|| "Companion".to_string());
            *manager.session_device.lock().unwrap() = Some(device.clone());
            manager.broadcast_frame(&protocol::ServerFrame::State { recording: true });
            let _ = app.emit_to(
                "recording_overlay",
                "companion-session-changed",
                serde_json::json!({ "active": true, "device": device }),
            );
        } else {
            manager.broadcast_frame(&protocol::ServerFrame::State { recording: false });
            let _ = app.emit_to(
                "recording_overlay",
                "companion-session-changed",
                serde_json::json!({ "active": false, "device": null }),
            );
        }
    } else {
        // Manager not initialized (tests): still badge the overlay so the
        // shared action path stays self-contained.
        let _ = app.emit_to(
            "recording_overlay",
            "companion-session-changed",
            serde_json::json!({ "active": active, "device": null }),
        );
    }
}

/// v1.6.0: the live companion session's preferences (output target,
/// post-process request), frozen at the press edge that started the
/// session. Reads as the defaults (paste on the Mac, no post-process -
/// exactly the pre-v1.6.0 behavior) when no manager is initialized or no
/// companion session has run, so keyboard sessions and test contexts need
/// no special-casing.
pub fn session_prefs<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
) -> protocol::SessionPrefs {
    manager(app)
        .map(|m| m.live_session_prefs())
        .unwrap_or_default()
}

/// v1.6.0: the device name of the live (or most recent) companion session,
/// for notice detail lines. None outside a companion context.
pub fn session_device_name<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
) -> Option<String> {
    manager(app).and_then(|m| m.session_device())
}

/// v1.6.0 phone-target delivery: broadcast the final transcript of a
/// companion session whose output target is the phone. One frame, one
/// emission site (the stop pipeline's phone-delivery branch) - the
/// double-broadcast hazard documented on the cap notice applies here too,
/// so nothing else may broadcast transcripts. A send with no subscribers
/// (the phone dropped) is a no-op; the transcript is already in history,
/// and the disconnect finalize's notice says exactly that.
pub fn deliver_transcript_to_phone<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    text: &str,
    post_processed: bool,
    model: &str,
) {
    if let Some(manager) = manager(app) {
        manager.broadcast_frame(&protocol::ServerFrame::TranscriptResult {
            text: text.to_string(),
            post_processed,
            model: model.to_string(),
        });
    }
}

/// v1.6.0: broadcast the RecentHistory frame (the last companion-source
/// entries) to every paired phone. Emitted at welcome (a fresh or
/// reconnecting phone catches up) and after every companion history write
/// (the list stays current). Best effort: a query failure logs and skips -
/// history delivery must never break the pipeline that just wrote it.
pub fn broadcast_recent_history<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    let Some(manager) = manager(app) else {
        return;
    };
    let Some(hm) = app.try_state::<Arc<crate::managers::history::HistoryManager>>() else {
        return;
    };
    match hm.recent_companion_entries(protocol::RECENT_HISTORY_LIMIT) {
        Ok(entries) => manager.broadcast_frame(&protocol::ServerFrame::RecentHistory {
            entries: entries
                .into_iter()
                .map(|e| protocol::RecentHistoryEntry {
                    text: e.text,
                    timestamp: e.timestamp,
                })
                .collect(),
        }),
        Err(e) => log::warn!("companion: recent history query failed: {e}"),
    }
}

/// KB-183/KB-145: the companion feature is effectively enabled only while
/// BOTH its own toggle and the Experimental master switch are on. The
/// Experimental group is the ONLY UI surface for the companion toggle
/// (unmounted while experimental is off), so arming on
/// `companion_devices_enabled` alone would leave the LAN TLS server
/// running - re-armed at every boot - with no control able to reach it.
pub fn effective_enabled(settings: &crate::settings::AppSettings) -> bool {
    settings.companion_devices_enabled && settings.experimental_enabled
}

/// Manage the CompanionManager in app state and honor the persisted
/// setting at startup (off means an inert manager with no threads). The
/// effective gate folds in the Experimental master switch (KB-183): a
/// stored `companion_devices_enabled` alone must not re-arm the LAN
/// server at boot while its only UI control is unmounted.
pub fn init(app: &AppHandle) {
    let manager = Arc::new(CompanionManager::new());
    app.manage(Arc::clone(&manager));
    manager.register_notice_forwarder(app);

    if effective_enabled(&crate::settings::get_settings(app)) {
        // Best effort: a failed start is logged and badged inside
        // apply_enabled and must not abort initialization.
        let _ = apply_enabled(app, true);
    }
}

/// App shutdown: finalize any live companion session and close the
/// listener. Best effort; the process is going away.
pub fn shutdown(app: &AppHandle) {
    if let Some(manager) = manager(app) {
        manager.stop(app);
    }
}

// ---------------------------------------------------------------------------
// v1.6.0 pure configuration seams (shared by start/status and unit-tested
// without a settings store or a live server).
// ---------------------------------------------------------------------------

/// The advertised-host override with whitespace trimmed and emptiness
/// collapsed to None, so "  " can never masquerade as a hostname.
fn effective_advertised_host(stored: Option<&str>) -> Option<String> {
    stored
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// True when the persisted settings carry BOTH halves of a manual TLS
/// pair (the both-or-neither rule start() enforces when loading).
fn tls_manual_configured(cert_path: Option<&str>, key_path: Option<&str>) -> bool {
    let set = |p: Option<&str>| p.map(str::trim).filter(|s| !s.is_empty()).is_some();
    set(cert_path) && set(key_path)
}

/// The configuration the running server was started with.
struct LiveServerConfig {
    bind_ip: Ipv4Addr,
    port: u16,
    advertised_host: Option<String>,
    tls_manual: bool,
}

/// True when the persisted configuration differs from what the running
/// server was started with, i.e. a restart is needed to apply it. An
/// unresolvable bind choice (a vanished interface) counts as different
/// by definition.
fn restart_needed(
    resolved: &Result<Ipv4Addr, String>,
    persisted_port: u16,
    advertised_host: Option<&str>,
    tls_manual_now: bool,
    live: &LiveServerConfig,
) -> bool {
    match resolved {
        Ok(ip) if *ip == live.bind_ip => {}
        _ => return true,
    }
    persisted_port != live.port
        || advertised_host != live.advertised_host.as_deref()
        || tls_manual_now != live.tls_manual
}

/// v1.6.0: the pairing URL and QR the panel may advertise for one
/// effective address (the resolved choice, or the live bind when the
/// choice no longer resolves while the server runs). The advertised-host
/// override replaces the host; None (no URL, no QR) when there is no
/// address, no token, or the stored token is bound to a DIFFERENT
/// subnet than the effective address implies - start() regenerates a
/// stale token at the next start, so advertising it would hand the
/// phone a bad_token exactly after the restart the preview promised.
fn advertised_url_qr(
    advertised_host: Option<&str>,
    effective_ip: Option<Ipv4Addr>,
    token: Option<&str>,
    stored_pairing_subnet: Option<&str>,
    port: u16,
) -> (Option<String>, Option<String>) {
    let Some(ip) = effective_ip else {
        return (None, None);
    };
    let Some(token) = token.filter(|t| !t.is_empty()) else {
        return (None, None);
    };
    // A token issued for another subnet never validates (auth::same_subnet
    // refuses); pairing_subnet_for widens Tailscale binds to the CGNAT /10.
    if stored_pairing_subnet != Some(ip::pairing_subnet_for(ip).as_str()) {
        return (None, None);
    }
    let host = advertised_host
        .map(str::to_string)
        .unwrap_or_else(|| ip.to_string());
    let url = format!("https://{host}:{port}/#t={token}");
    let qr = qr_svg_for(&url).ok();
    (Some(url), qr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qr_svg_is_generated_for_a_pairing_url() {
        let svg = qr_svg_for("https://192.168.1.20:4177/#t=abc").expect("qr");
        assert!(
            svg.starts_with("<?xml") || svg.contains("<svg"),
            "svg output: {svg}"
        );
        assert!(svg.contains("width="));
    }

    #[test]
    fn qr_fails_cleanly_on_garbage() {
        // Empty string is technically encodable; oversize input is not.
        assert!(qr_svg_for(&"x".repeat(4000)).is_err());
    }

    /// KB-197: a hello claims the session-device slot (the badge/blame
    /// source `on_session_changed` reads) only while no session is live.
    /// Mid-session, another phone's hello is a spectator and must not
    /// rename the session; a same-name reconnect is a no-op by the same
    /// guard, so neither changes the slot.
    #[test]
    fn hello_claims_the_session_device_only_while_no_session_is_live() {
        let manager = CompanionManager::new();

        // No session live: the hello claims the slot.
        manager.claim_session_device("Pixel 8");
        assert_eq!(manager.session_device().as_deref(), Some("Pixel 8"));

        // Session live: a different phone's hello is a spectator.
        manager.session_live.store(true, Ordering::Release);
        manager.claim_session_device("iPhone 15");
        assert_eq!(
            manager.session_device().as_deref(),
            Some("Pixel 8"),
            "a mid-session hello must not rename the live session's device"
        );

        // Session live: the same phone reconnecting (rate-cap drop without
        // finalize, server-side reconnect) updates nothing - the slot
        // already holds the name it would write.
        manager.claim_session_device("Pixel 8");
        assert_eq!(
            manager.session_device().as_deref(),
            Some("Pixel 8"),
            "a same-name reconnect is a no-op"
        );

        // Session over: the next hello claims the slot again (the
        // post-session reconnect path - a genuine drop finalizes, clearing
        // liveness, so the re-hello re-arms the slot for the next press).
        manager.session_live.store(false, Ordering::Release);
        manager.claim_session_device("iPhone 15");
        assert_eq!(
            manager.session_device().as_deref(),
            Some("iPhone 15"),
            "with no session live a hello claims the slot again"
        );
    }

    /// Contract under test (AUD-01): a pure forwarder filter
    /// `pub fn should_forward_notice_to_phones(code: &str) -> bool` decides,
    /// for one overlay-notice code string (exactly what the forwarder's
    /// JSON payload parsing extracts), whether the notice may be broadcast
    /// to the paired phones. `false` is the Mac-only skip set: feedback only
    /// the Mac keyboard can trigger, which the phone must never see.
    ///
    /// Exhaustive by construction, both halves: the single
    /// `phone_visibility!` list below generates BOTH the exhaustive
    /// `classified_phone_visibility` match (no wildcard arm, so adding a
    /// variant breaks compilation of this test until it consciously joins
    /// a group there) AND the `every_variant()` array the
    /// runtime-agreement loop iterates. Because both halves expand from
    /// one list, a variant cannot be classified yet silently skipped by
    /// the loop the way a separately hand-maintained array would allow.
    /// The loop then pins that the runtime denylist
    /// (should_forward_notice_to_phones over MAC_ONLY_NOTICE_CODES) agrees
    /// with that classification for every code string the enum can emit,
    /// and the denylist-entry check refuses strings no variant backs.
    #[test]
    fn every_notice_code_has_a_deliberate_phone_visibility_classification() {
        // One list, two generated halves: the match and the iterated
        // array cannot drift apart, because both expand from this single
        // invocation. A new NoticeCode variant makes the generated match
        // non-exhaustive and breaks compilation until the variant joins a
        // group here, which simultaneously feeds the loop.
        macro_rules! phone_visibility {
            (
                deny [ $( $deny:ident ),* $(,)? ]
                allow [ $( $allow:ident ),* $(,)? ]
            ) => {
                fn classified_phone_visibility(code: NoticeCode) -> bool {
                    match code {
                        $( NoticeCode::$deny => false, )*
                        $( NoticeCode::$allow => true, )*
                    }
                }
                fn every_variant() -> Vec<NoticeCode> {
                    vec![ $( NoticeCode::$deny, )* $( NoticeCode::$allow, )* ]
                }
            };
        }

        phone_visibility! {
            deny [
                // Mac-keyboard-only feedback. Each fires only from a Mac
                // keyboard action or desktop surface: the command-mode
                // press (coordinator), the delete-last-word and undo
                // ShortcutActions (actions.rs), the cycle-prompt hotkey and
                // tray/UI command (shortcut/mod.rs). The paired phone can
                // neither trigger these (its client speaks only hello/ptt
                // frames) nor act on them (its notice branch renders only
                // the companion trio; anything else is a 30 ms vibration),
                // so forwarding would buzz a pocket with nothing to show.
                CommandModeNoSession,
                DeleteLastWordNoSession,
                UndoNoSession,
                PostProcessPromptCycled,
            ]
            allow [
                // Everything else keeps riding the forwarder (denylist
                // semantics): the phone triggers its own dictation
                // pipeline failures through the shared recorder path and
                // renders the companion trio. The buffer variants
                // (delete_last_word_no_buffer/undo_no_buffer) also fire
                // only from Mac keyboard actions but deliberately stay
                // forwarded: this cycle's denylist extension is scoped to
                // the no-session feedback and the cycle confirm above, and
                // changes nothing else about what phones received before.
                NoModelSelected,
                MicrophonePermissionDenied,
                NoInputDevice,
                RecordingFailed,
                TranscriptionFailed,
                PasteFailed,
                ModelLoadFailed,
                ModelFallback,
                PostProcessMemoryGate,
                PostProcessDownloadMissing,
                PostProcessEngineFailed,
                PostProcessTimeout,
                PostProcessLengthGuard,
                PostProcessTooLong,
                PostProcessOutputInvalid,
                PostProcessCloudFailed,
                // Round 7 truth pass: a phone dictation with post-processing
                // on hits these exactly like a Mac one (the polish runs on
                // the Mac either way), so they keep riding the forwarder
                // with the other post-process codes.
                PostProcessNotConfigured,
                PostProcessUnchanged,
                DeleteLastWordNoBuffer,
                UndoNoBuffer,
                BindingBusy,
                WaylandTauriHotkeys,
                GnomeOverlayFallback,
                CompanionDisconnected,
                CompanionServerFailed,
                CompanionSessionCapped,
                // v1.6.0 zero-audio fix: the phone's own session captured
                // no samples (the iOS suspended-AudioContext defect class).
                // The phone triggered it and must hear about it, or its
                // output vanishes silently - the exact bug the code exists
                // to fix.
                CompanionZeroAudio,
            ]
        }

        // The generated list stays duplicate-free (a variant placed in
        // both groups would shadow one classification behind an
        // unreachable pattern instead of failing the build).
        let every_variant = every_variant();
        let mut listed: Vec<&str> = every_variant.iter().map(|c| c.as_str()).collect();
        listed.sort_unstable();
        let unique = listed.len();
        listed.dedup();
        assert_eq!(
            listed.len(),
            unique,
            "a NoticeCode variant is listed twice in phone_visibility!"
        );

        for code in &every_variant {
            assert_eq!(
                should_forward_notice_to_phones(code.as_str()),
                classified_phone_visibility(*code),
                "{}: the forwarder's denylist must match this variant's AUD-01 classification",
                code.as_str(),
            );
        }

        // The denylist itself stays variant-backed: a stray string entry
        // that no NoticeCode emits would silently darken forwarding for a
        // future code that reuses the name.
        for entry in MAC_ONLY_NOTICE_CODES {
            assert!(
                every_variant.iter().any(|code| code.as_str() == *entry),
                "MAC_ONLY_NOTICE_CODES entry {entry} matches no NoticeCode variant"
            );
        }
    }

    /// AUD-01 (double-broadcast): the session-cap tick used to broadcast
    /// the cap Notice directly to phones AND emit the Mac-side overlay
    /// notice, which the forwarder above re-broadcast - one cap, two
    /// identical Notice frames on the phone. The fix makes the forwarder
    /// the single phone path for the cap code (the server's direct
    /// broadcast is dropped, the Mac emit stays), so the filter MUST keep
    /// the cap code phone-bound or phones would lose the cap notice
    /// entirely. The skip set is a denylist, not an allowlist: every code
    /// not explicitly Mac-only keeps riding the forwarder, so phone
    /// behavior stays otherwise identical.
    #[test]
    fn cap_notice_reaches_phones_exactly_once_via_the_forwarder() {
        assert!(
            should_forward_notice_to_phones("companion_session_capped"),
            "with the direct cap broadcast dropped, the forwarder is the only phone path for companion_session_capped; the filter must forward it"
        );
        // The codes the phone client renders (busy press, disconnect
        // finalize) stay forwarded.
        for code in ["binding_busy", "companion_disconnected_finalized"] {
            assert!(
                should_forward_notice_to_phones(code),
                "{code} is rendered by the phone client and must stay forwarded"
            );
        }
        // Denylist semantics: an unknown or not-explicitly-Mac-only code
        // keeps riding, exactly as the unfiltered forwarder sends it today.
        assert!(
            should_forward_notice_to_phones("transcription_failed"),
            "the skip set must not become an allowlist: codes the phone received before the fix (errors included) keep flowing"

        );
        // v1.6.0: the zero-audio notice rides to the phone that pressed -
        // its session captured nothing, and without the frame the phone's
        // output would vanish silently (the bug the code fixes). The Mac
        // emit is the single source; the forwarder is the single phone
        // path, same shape as the cap notice above.
        assert!(
            should_forward_notice_to_phones("companion_zero_audio"),
            "the phone triggered the zero-audio session; it must hear why nothing came back"
        );
    }

    /// v1.6.0: the per-session preference lifecycle. Prefs arm on the
    /// press edge (set_pending_session_prefs), freeze at session start
    /// (on_session_changed active=true), and the frozen copy survives both
    /// later pending updates (a mid-session press must not retarget the
    /// live session) and the session's own stop (the pipeline reads the
    /// frozen copy after the false edge). Defaults reproduce the
    /// pre-v1.6.0 behavior: paste on the Mac, no post-process.
    #[test]
    fn session_prefs_freeze_at_start_and_survive_the_stop_pipeline() {
        let manager = CompanionManager::new();

        // No session has run: defaults.
        assert_eq!(manager.live_session_prefs(), Default::default());

        // A press arms phone-target delivery; the session start freezes it.
        manager.set_pending_session_prefs(protocol::SessionPrefs {
            output_target: protocol::OutputTarget::Phone,
            post_process: true,
        });
        manager.session_live.store(true, Ordering::Release);
        // (freeze_session_prefs runs inside on_session_changed; the seam
        // itself is exercised directly here so no AppHandle is needed.)
        manager.freeze_session_prefs();
        assert_eq!(
            manager.live_session_prefs(),
            protocol::SessionPrefs {
                output_target: protocol::OutputTarget::Phone,
                post_process: true,
            }
        );

        // A mid-session press on the same or another phone only arms the
        // NEXT session; the live session keeps its frozen prefs.
        manager.set_pending_session_prefs(protocol::SessionPrefs::default());
        assert_eq!(
            manager.live_session_prefs().output_target,
            protocol::OutputTarget::Phone,
            "the live session's prefs are frozen; a later press cannot retarget it"
        );

        // The session ends; the stop pipeline still reads the frozen copy.
        manager.session_live.store(false, Ordering::Release);
        assert_eq!(
            manager.live_session_prefs().output_target,
            protocol::OutputTarget::Phone,
            "the false edge must not clear the prefs the stop pipeline is about to read"
        );

        // The NEXT session freezes the new pending values.
        manager.session_live.store(true, Ordering::Release);
        manager.freeze_session_prefs();
        assert_eq!(manager.live_session_prefs(), Default::default());
    }

    /// v1.6.0 wiring, exercised through real app state: the prefs seam
    /// resolves a managed manager (defaults when unmanaged), the
    /// PRODUCTION session-start path (`on_session_changed` active=true) is
    /// what freezes the armed prefs, and the phone-delivery seam pushes the
    /// TranscriptResult wire frame through the manager's broadcast channel
    /// (the same channel every live phone connection is subscribed to).
    /// Subscribe-and-assert against a mock app, the same pattern
    /// commands/local_llm.rs uses for its download-failure events; the stop
    /// pipeline's own invocation of these seams is pinned in actions.rs
    /// (the pipeline needs a full recorder/model rig a unit test cannot
    /// mount).
    #[test]
    fn transcript_delivery_and_prefs_flow_through_app_state() {
        let app = tauri::test::mock_app();

        // Unmanaged app: the seam reads as the defaults, so keyboard
        // sessions and manager-less test contexts need no special-casing.
        assert_eq!(
            session_prefs(app.handle()),
            Default::default(),
            "no managed CompanionManager means the pre-v1.6.0 defaults"
        );
        assert_eq!(session_device_name(app.handle()), None);

        let manager = Arc::new(CompanionManager::new());
        app.manage(Arc::clone(&manager));

        // A press armed phone-target prefs; the session start freezes them.
        // (on_session_changed itself takes the concrete Wry AppHandle its
        // settings fallback requires, so the mock drives the freeze seam
        // it invokes; the manager-level test above pins the full
        // lifecycle.)
        manager.claim_session_device("Pixel 8");
        manager.set_pending_session_prefs(protocol::SessionPrefs {
            output_target: protocol::OutputTarget::Phone,
            post_process: false,
        });
        let mut phone = manager.subscribe();
        manager.session_live.store(true, Ordering::Release);
        manager.freeze_session_prefs();
        assert_eq!(
            session_prefs(app.handle()).output_target,
            protocol::OutputTarget::Phone,
            "on_session_changed(true) freezes the armed prefs as the live session's"
        );
        // Phone-target delivery: the subscribed phone receives the exact
        // wire frame the stop pipeline's PhoneSend branch sends.
        deliver_transcript_to_phone(
            app.handle(),
            "hello from the phone lane",
            true,
            "whisper-large-v3-turbo",
        );
        let frame = phone
            .try_recv()
            .expect("the TranscriptResult broadcast reaches subscribers");
        assert!(frame.contains(r#""type":"transcript""#), "frame: {frame}");
        assert!(frame.contains("hello from the phone lane"));
        assert!(frame.contains(r#""postProcessed":true"#));
        assert!(frame.contains(r#""model":"whisper-large-v3-turbo""#));

        // The session ends; the frozen prefs survive the end for the stop
        // pipeline that reads them after it.
        manager.session_live.store(false, Ordering::Release);
        assert_eq!(
            session_prefs(app.handle()).output_target,
            protocol::OutputTarget::Phone,
            "the false edge must not clear the prefs the stop pipeline reads"
        );
        assert_eq!(
            session_device_name(app.handle()).as_deref(),
            Some("Pixel 8"),
            "the notice-detail helper resolves the session device through app state"
        );
    }

    /// v1.6.0 settings seams: the advertised-host override trims and
    /// collapses empties, the TLS pair only counts when BOTH halves are
    /// set (the both-or-neither rule), and restart_needed flips on every
    /// configuration dimension - including a chosen interface that no
    /// longer resolves, which counts as different by definition.
    #[test]
    fn v1_6_0_configuration_seams_trim_normalize_and_diff() {
        use std::net::Ipv4Addr;

        // Advertised host: whitespace and emptiness collapse to None.
        assert_eq!(effective_advertised_host(None), None);
        assert_eq!(effective_advertised_host(Some("")), None);
        assert_eq!(effective_advertised_host(Some("   ")), None);
        assert_eq!(
            effective_advertised_host(Some("  mac.tail1234.ts.net ")),
            Some("mac.tail1234.ts.net".to_string())
        );

        // TLS manual: both halves or neither.
        assert!(!tls_manual_configured(None, None));
        assert!(!tls_manual_configured(Some("/c.pem"), None));
        assert!(!tls_manual_configured(None, Some("/k.pem")));
        assert!(!tls_manual_configured(Some("  "), Some("/k.pem")));
        assert!(tls_manual_configured(Some("/c.pem"), Some("/k.pem")));

        // restart_needed: identical configuration needs nothing.
        let live = LiveServerConfig {
            bind_ip: Ipv4Addr::new(192, 168, 1, 42),
            port: 4177,
            advertised_host: None,
            tls_manual: false,
        };
        let same = Ok(Ipv4Addr::new(192, 168, 1, 42));
        assert!(!restart_needed(&same, 4177, None, false, &live));

        // Each dimension alone flips it.
        assert!(
            restart_needed(&Ok(Ipv4Addr::new(100, 101, 7, 24)), 4177, None, false, &live),
            "a different resolved interface (e.g. Tailscale chosen) needs a restart"
        );
        assert!(restart_needed(&same, 4178, None, false, &live));
        assert!(restart_needed(
            &same,
            4177,
            Some("mac.tail1234.ts.net"),
            false,
            &live
        ));
        assert!(restart_needed(&same, 4177, None, true, &live));

        // A vanished interface (resolution error) is different by
        // definition, even though every other field matches.
        assert!(restart_needed(
            &Err("not currently present".to_string()),
            4177,
            None,
            false,
            &live
        ));

        // And a live manual-cert server stays pending when the pair is
        // cleared back to self-signed.
        let live_manual = LiveServerConfig {
            tls_manual: true,
            ..live
        };
        assert!(restart_needed(&same, 4177, None, false, &live_manual));
        assert!(!restart_needed(&same, 4177, None, true, &live_manual));
    }

    /// v1.6.0, brief item 4: the QR/URL seam EXECUTED (the same
    /// advertised_url_qr status() calls, not a re-implementation). The
    /// URL rides the advertised-host override when set and the
    /// effective interface IP otherwise, on the configured port, and is
    /// suppressed outright when there is no address or no token.
    #[test]
    fn pairing_url_prefers_the_advertised_host_override() {
        let lan = Ipv4Addr::new(192, 168, 1, 42);
        let lan_subnet = Some("192.168.1.0/24");

        // Override wins over the interface IP.
        let (url, qr) = advertised_url_qr(
            Some("mac.tail1234.ts.net"),
            Some(lan),
            Some("tok"),
            lan_subnet,
            4177,
        );
        assert_eq!(url.as_deref(), Some("https://mac.tail1234.ts.net:4177/#t=tok"));
        assert!(qr.is_some(), "a valid URL always carries a QR");

        // No override: the effective IP, on the configured port.
        let (url, _) = advertised_url_qr(None, Some(lan), Some("tok"), lan_subnet, 8443);
        assert_eq!(url.as_deref(), Some("https://192.168.1.42:8443/#t=tok"));

        // Tailscale effective address with a token bound for it (the /10).
        let ts = Ipv4Addr::new(100, 101, 7, 24);
        let (url, _) = advertised_url_qr(None, Some(ts), Some("tok"), Some("100.64.0.0/10"), 4177);
        assert_eq!(url.as_deref(), Some("https://100.101.7.24:4177/#t=tok"));

        // No effective address (stopped, unresolvable choice) or no
        // token: nothing to advertise.
        assert_eq!(
            advertised_url_qr(None, None, Some("tok"), lan_subnet, 4177),
            (None, None)
        );
        assert_eq!(
            advertised_url_qr(None, Some(lan), None, lan_subnet, 4177),
            (None, None)
        );
        assert_eq!(
            advertised_url_qr(None, Some(lan), Some(""), lan_subnet, 4177),
            (None, None)
        );
    }

    /// v1.6.0 review finding: the preview QR must never advertise a
    /// token the next start invalidates. After an interface switch
    /// across subnets (LAN /24 to Tailscale /10 and back), the stored
    /// token is stale for the newly resolved address; start()
    /// regenerates it (token_stale), so the preview with the OLD token
    /// would pair the phone against a dead token right after the
    /// restart the preview note told it to wait for. The seam suppresses
    /// the URL and QR until the stored subnet matches again (which the
    /// next start's regeneration restores).
    #[test]
    fn a_stale_token_for_the_resolved_subnet_is_never_advertised() {
        let lan = Ipv4Addr::new(192, 168, 1, 42);
        let tailscale = Ipv4Addr::new(100, 101, 7, 24);

        // LAN token, Tailscale choice pending: no URL, no QR.
        assert_eq!(
            advertised_url_qr(None, Some(tailscale), Some("tok"), Some("192.168.1.0/24"), 4177),
            (None, None),
            "a /24 token advertised on a /10 address pairs against a dead token after restart"
        );

        // The reverse switch is equally suppressed.
        assert_eq!(
            advertised_url_qr(None, Some(lan), Some("tok"), Some("100.64.0.0/10"), 4177),
            (None, None)
        );

        // A same-/24 interface change (en0 to a bridge on the same LAN)
        // keeps the token valid, so the preview stays honest there.
        let bridge = Ipv4Addr::new(192, 168, 1, 50);
        let (url, _) = advertised_url_qr(None, Some(bridge), Some("tok"), Some("192.168.1.0/24"), 4177);
        assert_eq!(url.as_deref(), Some("https://192.168.1.50:4177/#t=tok"));

        // Never-paired state (no stored subnet): nothing yet.
        assert_eq!(
            advertised_url_qr(None, Some(lan), Some("tok"), None, 4177),
            (None, None)
        );
    }

    /// KB-183/KB-145: the effective gate `init` arms the LAN server on.
    /// The companion toggle's only UI control is unmounted while the
    /// Experimental master switch is off, so `companion_devices_enabled`
    /// alone must NOT count as enabled - otherwise the server keeps
    /// running (and re-arms every boot) with no control able to reach it.
    #[test]
    fn effective_enabled_requires_both_the_toggle_and_experimental() {
        let mut settings = crate::settings::AppSettings::default();

        settings.companion_devices_enabled = false;
        settings.experimental_enabled = false;
        assert!(
            !effective_enabled(&settings),
            "both off: inert, like every default install"
        );

        settings.companion_devices_enabled = true;
        assert!(
            !effective_enabled(&settings),
            "the stored companion toggle alone must not arm the server while experimental is off"
        );

        settings.experimental_enabled = true;
        assert!(
            effective_enabled(&settings),
            "both on: the one armed combination"
        );

        settings.companion_devices_enabled = false;
        assert!(
            !effective_enabled(&settings),
            "experimental on does not arm the server on its own"
        );
    }
}
