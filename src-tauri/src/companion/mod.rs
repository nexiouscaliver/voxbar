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
}

struct CompanionInner {
    running: bool,
    bind_ip: Option<Ipv4Addr>,
    port: u16,
    fingerprint: Option<String>,
    last_error: Option<String>,
    devices: Vec<String>,
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
            }),
            broadcast,
            shutdown_tx,
            session_live: AtomicBool::new(false),
            session_device: Mutex::new(None),
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

    /// Start the server. Idempotent. Generates/repairs the pairing token
    /// (rebinding it to the current LAN /24, which re-pairs phones after a
    /// subnet change), loads the TLS certificate, binds the listener, and
    /// pre-warms the remote recorder so a phone press starts capture fast.
    pub fn start(&self, app: &AppHandle) -> Result<(), String> {
        if !platform_supported() {
            return Err("Companion devices need macOS this cycle".to_string());
        }
        if self.is_running() {
            return Ok(());
        }

        let lan_ip = ip::advertised_lan_ipv4()
            .ok_or_else(|| "No private Wi-Fi interface found to advertise".to_string())?;

        // Pairing token: generate on enable, and rebind when the advertised
        // subnet changed (a token from another network never validates).
        let subnet = auth::subnet_of(lan_ip);
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

        // TLS certificate (generated once, persisted).
        let cert_dir = app
            .path()
            .app_local_data_dir()
            .map_err(|e| format!("app data dir unavailable: {e}"))?
            .join("companion");
        let cert = cert::ensure_certificate(&cert_dir, std::net::IpAddr::V4(lan_ip))?;
        let fingerprint = cert.fingerprint.clone();

        // Bind synchronously so the settings toggle reports a real outcome
        // (macOS firewall prompt aside, a bind error surfaces verbatim).
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let cfg = server::ServerConfig {
            ip: lan_ip,
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
            inner.bind_ip = Some(lan_ip);
            inner.port = port;
            inner.fingerprint = Some(fingerprint);
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

        log::info!("companion: server running on https://{lan_ip}:{port} (subnet {subnet})");
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
        inner.devices.clear();
        log::info!("companion: server stopped");
    }

    /// Status snapshot for the settings panel (QR included).
    pub fn status(&self, app: &AppHandle) -> CompanionStatus {
        let settings = crate::settings::get_settings(app);
        let inner = self.inner.lock().unwrap();
        let supported = platform_supported();
        let bind_ip = inner.bind_ip;
        let port = if inner.running {
            inner.port
        } else {
            settings.companion_port
        };
        let token = settings.companion_pairing_token.clone();

        let (url, qr_svg) = match (inner.running, bind_ip, token.as_deref()) {
            (true, Some(ip), Some(token)) if !token.is_empty() => {
                let url = format!("https://{ip}:{port}/#t={token}");
                let qr = qr_svg_for(&url).ok();
                (Some(url), qr)
            }
            _ => (None, None),
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
        }
    }

    pub fn set_last_error(&self, error: Option<String>) {
        self.inner.lock().unwrap().last_error = error;
    }

    /// Reset pairing: fresh token (rebound to the current subnet) and a
    /// restart so the QR changes. Phones holding the old token are refused.
    pub fn reset_pairing(&self, app: &AppHandle) -> Result<(), String> {
        let subnet =
            auth::subnet_of(ip::advertised_lan_ipv4().ok_or("No private Wi-Fi interface found")?);
        let mut settings = crate::settings::get_settings(app);
        settings.companion_pairing_token = Some(auth::generate_pairing_token());
        settings.companion_pairing_subnet = Some(subnet);
        crate::settings::write_settings(app, settings);

        if self.is_running() {
            self.stop(app);
            // stop() finalized any live session; give the listener a beat
            // to release the port, then bring the server back with the new
            // token.
            std::thread::sleep(Duration::from_millis(150));
            if let Err(e) = self.start(app) {
                return Err(e);
            }
        }
        Ok(())
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
                    manager.broadcast_frame(&protocol::ServerFrame::Notice {
                        code: code.to_string(),
                    });
                }
            }
        });
        *guard = Some(id);
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
fn manager(app: &AppHandle) -> Option<Arc<CompanionManager>> {
    app.try_state::<Arc<CompanionManager>>()
        .map(|s| s.inner().clone())
}

/// Apply a `companion_devices_enabled` change: start/stop the server with
/// side effects. Called from the settings command and at startup.
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

/// Manage the CompanionManager in app state and honor the persisted
/// setting at startup (off means an inert manager with no threads).
pub fn init(app: &AppHandle) {
    let manager = Arc::new(CompanionManager::new());
    app.manage(Arc::clone(&manager));
    manager.register_notice_forwarder(app);

    if crate::settings::get_settings(app).companion_devices_enabled {
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
}
