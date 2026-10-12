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
use std::collections::HashMap;
use std::net::Ipv4Addr;
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
    /// v1.6.0: the configuration snapshot the running server was started
    /// with, so status() can tell a restart is pending.
    advertised_host: Option<String>,
    tls_manual: bool,
}

/// v1.6.2 W1-L2: one authenticated connection. Lane ids are SERVER-assigned
/// and never reused, and every registration carries its own monotonically
/// increasing generation: a disconnecting socket releases its registration
/// by (lane, generation) pair, so a stale socket's late death can never
/// erase a fresh same-name registration (KB-255 - the old name-keyed
/// forget_device zeroed the tray's paired count exactly this way).
struct ConnectionEntry {
    lane: u64,
    generation: u64,
    /// The hello's device name (client-controlled, display-only).
    device: String,
    tx: tokio::sync::mpsc::UnboundedSender<String>,
}

/// v1.6.2 W1-L2: one lane's session identity, frozen on the coordinator
/// thread at the single serialized instant a Start effect commits (the same
/// handoff-at-commit discipline v1.6.0 used for prefs). The device name and
/// preferences ride the session itself, so the stop pipeline resolves
/// delivery, attribution, and post-process from THIS record minutes later -
/// never from a claimable manager slot another hello can overwrite (KB-254
/// dies by construction: the zero-audio notice names the stopping session's
/// own device).
struct LaneSession {
    device: String,
    prefs: protocol::SessionPrefs,
    /// True from the Start commit until the stop pipeline's session-state
    /// flip (the lane-scoped truth the server's admission reads).
    live: bool,
}

/// v1.6.2 W1-L2: the session whose Stop effect is being executed, handed
/// from the coordinator thread (which knows the lane) to the shared
/// pipeline (whose action signature predates lanes). Written by
/// [`note_stopping_session`] immediately before the Stop effect runs and
/// consumed exactly once by the pipeline's first synchronous step, on the
/// same serialized thread - no window for another session's stop to
/// interleave.
struct StoppingSession {
    lane: u64,
    device: String,
    prefs: protocol::SessionPrefs,
}

pub struct CompanionManager {
    inner: Mutex<CompanionInner>,
    /// Server frames fanned out to every live client connection.
    broadcast: tokio::sync::broadcast::Sender<String>,
    /// Signalled (true) to stop the accept loop and every connection.
    shutdown_tx: watch::Sender<bool>,
    /// v1.6.2 W1-L2: the authenticated connections, in registration order
    /// (a handful at most, so linear scans are the honest data structure).
    /// Replaces the name-keyed device list AND the v1.6.0 lane-sender map.
    connections: Mutex<Vec<ConnectionEntry>>,
    /// The lane-id allocator; strictly monotonic, so a lane id is bound to
    /// exactly one connection for the process's lifetime.
    next_lane_id: std::sync::atomic::AtomicU64,
    /// The registration-generation allocator (KB-255's second key).
    next_generation: std::sync::atomic::AtomicU64,
    /// v1.6.2 W1-L2: per-lane session records (see [`LaneSession`]).
    lane_sessions: Mutex<HashMap<u64, LaneSession>>,
    /// The lane whose Start effect is being executed right now (the
    /// coordinator thread writes it in [`begin_session`] immediately
    /// before the shared action's start runs and reads it back
    /// synchronously on that same thread - the same serialized
    /// handoff-at-commit discipline as the session record itself).
    starting_lane: Mutex<Option<u64>>,
    /// The Stop-effect handoff slot (see [`StoppingSession`]).
    stopping: Mutex<Option<StoppingSession>>,
    /// v1.6.0: the preferences the NEXT companion session will run with.
    /// Armed by the most recent press edge the server saw (display-only:
    /// the tray's companion status block reads this as "what the next
    /// press will do"). The session ITSELF never reads this slot - its
    /// prefs ride the edge into the coordinator, which freezes them per
    /// lane at the start commit.
    pending_prefs: Mutex<protocol::SessionPrefs>,
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
                advertised_host: None,
                tls_manual: false,
            }),
            broadcast,
            shutdown_tx,
            connections: Mutex::new(Vec::new()),
            next_lane_id: std::sync::atomic::AtomicU64::new(1),
            next_generation: std::sync::atomic::AtomicU64::new(1),
            lane_sessions: Mutex::new(HashMap::new()),
            starting_lane: Mutex::new(None),
            stopping: Mutex::new(None),
            pending_prefs: Mutex::new(protocol::SessionPrefs::default()),
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

    /// v1.6.2 W1-L2: register one authenticated connection. Returns its
    /// SERVER-assigned lane id, its KB-255 registration generation, and the
    /// receiver its select loop drains. Registry-only on purpose (the
    /// settings/tray bookkeeping lives in [`Self::note_device_connected`],
    /// which needs a real app handle); the device list (tray count,
    /// settings panel) derives from live registrations, never from a
    /// name-keyed slot a stale socket can erase.
    pub fn register_connection(
        &self,
        device: &str,
    ) -> (
        u64,
        u64,
        tokio::sync::mpsc::UnboundedReceiver<String>,
    ) {
        let lane = self
            .next_lane_id
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let generation = self
            .next_generation
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.connections.lock().unwrap().push(ConnectionEntry {
            lane,
            generation,
            device: device.to_string(),
            tx,
        });
        (lane, generation, rx)
    }

    /// v1.6.2 W1-L2: the connect-edge bookkeeping the server runs right
    /// after a hello registers (the remember_device rule): persist the
    /// newest name as `companion_last_device` and re-sync the tray's
    /// companion block (W1-L8 - the paired count tracks the connection,
    /// not the next unrelated sync point; runs on the server's task thread
    /// so the snapshot compute never lands on the UI thread).
    pub fn note_device_connected(&self, app: &AppHandle, device: &str) {
        let mut settings = crate::settings::get_settings(app);
        if settings.companion_last_device.as_deref() != Some(device) {
            settings.companion_last_device = Some(device.to_string());
            crate::settings::write_settings(app, settings);
        }
        crate::tray::update_tray_menu(app);
    }

    /// v1.6.2 W1-L2 (KB-255): release a connection's registration. Both
    /// keys must match: the lane id (never reused) and the registration
    /// generation. A stale socket whose keepalive death lagged behind a
    /// same-name reconnect carries the OLD pair, which no longer matches
    /// any entry, so the fresh registration survives and the tray's paired
    /// count stays honest. Registry-only (the tray re-sync lives in
    /// [`Self::note_device_disconnected`]).
    pub fn unregister_connection(&self, lane: u64, generation: u64) {
        let mut connections = self.connections.lock().unwrap();
        connections.retain(|entry| !(entry.lane == lane && entry.generation == generation));
    }

    /// The disconnect-edge tray re-sync (the forget_device rule, now
    /// keyed on registrations instead of names).
    pub fn note_device_disconnected(&self, app: &AppHandle) {
        crate::tray::update_tray_menu(app);
    }

    /// The deduplicated device names of the live connections, in
    /// registration order (the settings panel's device list).
    pub fn devices(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for entry in self.connections.lock().unwrap().iter() {
            if !names.iter().any(|n| n == &entry.device) {
                names.push(entry.device.clone());
            }
        }
        names
    }

    /// The device name one connection hello'd with (None when the
    /// connection is gone).
    pub fn connection_device(&self, lane: u64) -> Option<String> {
        self.connections
            .lock()
            .unwrap()
            .iter()
            .find(|entry| entry.lane == lane)
            .map(|entry| entry.device.clone())
    }

    /// v1.6.2 W1-L2: freeze one lane's session identity at the instant the
    /// coordinator commits its Start (see [`begin_session`]). The device
    /// resolves from the connection registry; a lane with no live
    /// connection (test contexts) freezes as the generic Companion name.
    fn freeze_session(&self, lane: u64, prefs: protocol::SessionPrefs) -> LaneSession {
        let device = self
            .connection_device(lane)
            .unwrap_or_else(|| "Companion".to_string());
        LaneSession {
            device,
            prefs,
            live: true,
        }
    }

    /// Whether THIS lane's session is live - the lane-scoped truth the
    /// server's audio admission reads (a spectator phone, a reconnecting
    /// phone, and a keyboard dictation all read false).
    pub fn lane_owns_live_session(&self, lane: u64) -> bool {
        self.lane_sessions
            .lock()
            .unwrap()
            .get(&lane)
            .is_some_and(|session| session.live)
    }

    /// v1.6.2 W1-L2: the lanes with live sessions right now, sorted (the
    /// server-stop and disconnect-all teardowns finalize exactly these;
    /// sorted so callers and tests see a deterministic order despite the
    /// HashMap underneath).
    pub fn live_session_lanes(&self) -> Vec<u64> {
        let mut lanes: Vec<u64> = self
            .lane_sessions
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, session)| session.live)
            .map(|(lane, _)| *lane)
            .collect();
        lanes.sort_unstable();
        lanes
    }

    /// v1.6.0: arm the preferences the next companion session will run
    /// with. Called by the server on every press edge, so the tray's
    /// companion status block shows the latest phone choice as "what the
    /// next press will do". Display-and-default only: the session's own
    /// prefs ride its starting edge into the coordinator.
    pub fn set_pending_session_prefs(&self, prefs: protocol::SessionPrefs) {
        *self.pending_prefs.lock().unwrap() = prefs;
    }

    /// v1.6.0: the preferences the NEXT companion session will run with
    /// (armed by the phones' press edges, defaults before any). The tray's
    /// companion status block reads this as the active output target.
    pub fn pending_session_prefs(&self) -> protocol::SessionPrefs {
        *self.pending_prefs.lock().unwrap()
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

    /// Send one frame down exactly one connection's targeted lane. False
    /// when that lane is gone (the phone dropped): the caller treats it as
    /// the documented no-subscriber no-op.
    pub fn send_frame_to_lane(&self, lane: u64, frame: &protocol::ServerFrame) -> bool {
        let connections = self.connections.lock().unwrap();
        match connections.iter().find(|entry| entry.lane == lane) {
            Some(entry) => entry.tx.send(frame.to_json()).is_ok(),
            None => false,
        }
    }

    pub fn shutdown_signal(&self) -> watch::Receiver<bool> {
        self.shutdown_tx.subscribe()
    }

    pub fn failure_limiter_pin(&self) -> &Mutex<auth::FailureLimiter> {
        &self.failure_limiter
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
            (Some(cert_path), Some(key_path)) => Some(cert::load_manual_pair(
                std::path::Path::new(cert_path),
                std::path::Path::new(key_path),
            )?),
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

        // Pre-warm the remote recorder for the FIRST arriving connection
        // (VAD load off the press path). v1.6.2 W1-L2: pre-warming is
        // per-lane and keeps only the most recent connection's recorder
        // (see prewarm_remote_lane); the server pre-warms each subsequent
        // hello'd lane as it arrives.
        // (No lane exists until the first hello; the hello path warms.)

        log::info!(
            "companion: server running on https://{bind_ip}:{port} (subnet {subnet}, advertised as {})",
            advertised_host.as_deref().unwrap_or("the bind IP")
        );
        Ok(())
    }

    /// Stop the server: finalize every live companion session first (the
    /// forced finalize runs the ordinary Stop pipeline, which closes each
    /// lane's recorder itself), then close the listener and all
    /// connections. Idempotent.
    pub fn stop(&self, app: &AppHandle) {
        if !self.is_running() {
            return;
        }

        // Graceful finalize: every live phone session is finalized so
        // everything captured is transcribed and delivered. Forced rather
        // than synthesized release edges: locked (toggle) sessions ignore
        // release edges by design and would otherwise strand recordings.
        if let Some(coordinator) = app.try_state::<crate::TranscriptionCoordinator>() {
            for lane in self.live_session_lanes() {
                coordinator.finalize_companion_lane(lane);
            }
        }
        self.lane_sessions.lock().unwrap().clear();
        *self.stopping.lock().unwrap() = None;

        self.broadcast_frame(&protocol::ServerFrame::Goodbye {
            reason: "server_stopping",
        });
        let _ = self.shutdown_tx.send(true);

        // Close the lane recorders unless a stop pipeline is using them
        // right now (each pipeline closes its own at its end).
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
        self.connections.lock().unwrap().clear();
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
        let advertised_host =
            effective_advertised_host(settings.companion_advertised_host.as_deref());
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
            devices: self.devices(),
            error: inner.last_error.clone(),
            interfaces: candidates.iter().map(CompanionInterface::from).collect(),
            bind_ip: effective_ip.map(|ip| ip.to_string()),
            advertised_host,
            bind_error,
            tls_manual: inner.tls_manual,
            restart_pending,
        }
    }

    /// Paired-device count for the tray's companion status block (W1-L8):
    /// the deduplicated device names of the live connections (v1.6.2: the
    /// registration-keyed list KB-255 protects). A cheap lock read,
    /// deliberately NOT the full [`Self::status`] snapshot, which
    /// enumerates network interfaces on every call - far too heavy for a
    /// value the tray re-reads on every menu sync.
    pub fn paired_device_count(&self) -> usize {
        self.devices().len()
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
            crate::settings::get_settings(app)
                .companion_bind_ip
                .as_deref(),
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
    // v1.6.0 W1-L7: the pp hotkey upgraded or reverted the live session's
    // post-process mode (coordinator, Mac keyboard only).
    "post_process_session_upgraded",
    "post_process_session_downgraded",
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

/// v1.6.2 W1-L2: one lane's session boundary, reported by the shared
/// TranscribeAction. The State frame goes ONLY to the session's own lane
/// (each connection learns its own session's liveness - a second phone
/// recording concurrently never flips the first phone's button, and a
/// spectator's client never streams audio it would only have dropped), and
/// the recording overlay badge carries the session's own frozen device
/// name. The per-lane record's liveness flips here, which is exactly the
/// truth the server's audio admission samples.
pub fn on_lane_session_changed<R: tauri::Runtime>(app: &tauri::AppHandle<R>, lane: u64, active: bool) {
    let Some(manager) = manager(app) else {
        return;
    };
    let mut sessions = manager.lane_sessions.lock().unwrap();
    match sessions.get_mut(&lane) {
        Some(session) => {
            session.live = active;
        }
        None => return, // no session was ever committed for this lane
    }
    let device = sessions
        .get(&lane)
        .map(|s| s.device.clone())
        .unwrap_or_else(|| "Companion".to_string());
    drop(sessions);
    manager.send_frame_to_lane(
        lane,
        &protocol::ServerFrame::State { recording: active },
    );
    let _ = app.emit_to(
        "recording_overlay",
        "companion-session-changed",
        serde_json::json!({
            "active": active,
            "device": if active { Some(device) } else { None },
        }),
    );
}

/// v1.6.2 W1-L2: freeze one lane's session identity (device name from the
/// connection registry + the prefs the starting press edge carried) as the
/// session being committed. Called ONLY from the coordinator thread's Start
/// effect, at the single serialized instant the start commits and BEFORE
/// start()'s preamble - the v1.6.0 handoff-at-commit discipline, now keyed
/// per lane, so a concurrent press from another phone (queued behind the
/// same coordinator command) can only start its OWN session and nothing
/// can retarget this one's delivery or attribution later.
pub fn begin_session<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    lane: u64,
    prefs: protocol::SessionPrefs,
) {
    if let Some(manager) = manager(app) {
        let session = manager.freeze_session(lane, prefs);
        manager.lane_sessions.lock().unwrap().insert(lane, session);
        *manager.starting_lane.lock().unwrap() = Some(lane);
    }
}

/// v1.6.2 W1-L2: the lane of the companion session whose Start effect is
/// being executed (read back by the shared action's start, synchronously
/// behind begin_session on the coordinator thread). None for
/// keyboard/signal starts.
pub fn starting_session_lane<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
) -> Option<u64> {
    manager(app).and_then(|m| *m.starting_lane.lock().unwrap())
}

/// v1.6.2 W1-L2: drop a lane's session record entirely (a start that never
/// began recording - the rollback path - so a later session on the same
/// lane starts clean).
pub fn discard_session<R: tauri::Runtime>(app: &tauri::AppHandle<R>, lane: u64) {
    if let Some(manager) = manager(app) {
        manager.lane_sessions.lock().unwrap().remove(&lane);
    }
}

/// v1.6.2 W1-L2: hand the stopping session's identity from the coordinator
/// thread (which knows the lane its Stop effect ends) to the shared
/// pipeline, whose TranscribeAction signature predates lanes. Called
/// immediately before the Stop effect runs; consumed exactly once by the
/// pipeline's first synchronous step on the same thread.
pub fn note_stopping_session<R: tauri::Runtime>(app: &tauri::AppHandle<R>, lane: u64) {
    let Some(manager) = manager(app) else {
        return;
    };
    let sessions = manager.lane_sessions.lock().unwrap();
    if let Some(session) = sessions.get(&lane) {
        *manager.stopping.lock().unwrap() = Some(StoppingSession {
            lane,
            device: session.device.clone(),
            prefs: session.prefs,
        });
    }
}

/// v1.6.2 W1-L2: take the stopping session's identity for the pipeline
/// about to run (delivery target, prefs, and attribution device resolve
/// from it - KB-254: the zero-audio notice names THIS session's device,
/// never a claimable slot). None for keyboard sessions and test contexts.
pub fn take_stopping_session<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
) -> Option<(u64, String, protocol::SessionPrefs)> {
    let manager = manager(app)?;
    let stopping = manager.stopping.lock().unwrap().take()?;
    Some((stopping.lane, stopping.device, stopping.prefs))
}

/// v1.6.2 W1-L2 phone-target delivery: send the final transcript of one
/// companion session down ITS OWN frozen lane, never the broadcast channel
/// every paired phone subscribes to. The lane arrives from the stopping
/// session handoff (the connection whose press started the session); a
/// gone lane (the phone dropped) is a no-op - the transcript is already in
/// history, and the disconnect finalize's notice says exactly that.
pub fn deliver_transcript_to_lane<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    lane: u64,
    text: &str,
    post_processed: bool,
    model: &str,
) {
    let Some(manager) = manager(app) else {
        return;
    };
    if !manager.send_frame_to_lane(
        lane,
        &protocol::ServerFrame::TranscriptResult {
            text: text.to_string(),
            post_processed,
            model: model.to_string(),
        },
    ) {
        log::debug!(
            "companion: session lane {lane} is gone (the phone dropped); the transcript stays in history"
        );
    }
}

/// v1.6.2 W1-L2: send one phone its OWN recent list (the last
/// companion-source entries THAT phone dictated, identified by its hello
/// device name). Per-phone by construction: a phone's phone-destined
/// dictations appear in its own list (they are private to its lane) and
/// never in another phone's. Best effort: a query failure logs and skips -
/// history delivery must never break the pipeline that just wrote it.
pub fn send_recent_history_to_lane<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    lane: u64,
    device: &str,
) {
    let Some(manager) = manager(app) else {
        return;
    };
    let Some(hm) = app.try_state::<Arc<crate::managers::history::HistoryManager>>() else {
        return;
    };
    match hm.recent_companion_entries_for_device(
        device,
        protocol::RECENT_HISTORY_LIMIT,
    ) {
        Ok(entries) => {
            let _ = manager.send_frame_to_lane(
                lane,
                &protocol::ServerFrame::RecentHistory {
                    entries: entries
                        .into_iter()
                        .map(|e| protocol::RecentHistoryEntry {
                            text: e.text,
                            timestamp: e.timestamp,
                        })
                        .collect(),
                },
            );
        }
        Err(e) => log::warn!("companion: recent history query failed: {e}"),
    }
}

/// v1.6.2 W1-L2: refresh EVERY live connection's recent list (each with
/// its own device filter). Emitted after every companion history write.
pub fn broadcast_recent_history<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    let Some(manager) = manager(app) else {
        return;
    };
    for (lane, device) in manager
        .connections
        .lock()
        .unwrap()
        .iter()
        .map(|entry| (entry.lane, entry.device.clone()))
        .collect::<Vec<_>>()
    {
        send_recent_history_to_lane(app, lane, &device);
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

    /// v1.6.2 W1-L2 (KB-254 by construction): per-lane session identity.
    /// A session's device name freezes from ITS OWN connection's
    /// registration at the instant the coordinator commits the start, so a
    /// second phone connecting (or reconnecting under any name) before,
    /// during, or after phone A's session can never rename A's session or
    /// blame its notices on the wrong device - the old claimable
    /// session-device slot is gone entirely.
    #[test]
    fn session_identity_freezes_per_lane_and_cannot_be_renamed() {
        let app = tauri::test::mock_app();
        let manager = Arc::new(CompanionManager::new());
        app.manage(Arc::clone(&manager));

        // Phone A connects and its session commits.
        let (lane_a, _gen_a, _rx_a) = manager.register_connection("Pixel 8");
        begin_session(app.handle(), lane_a, protocol::SessionPrefs::default());
        assert!(manager.lane_owns_live_session(lane_a));

        // Phone B spectates mid-session (v1.6.0 KB-197's scenario, now
        // impossible to get wrong: B's hello registers a lane; it cannot
        // touch A's frozen record).
        let (_lane_b, _gen_b, _rx_b) = manager.register_connection("iPhone 15");
        assert_eq!(
            manager.lane_owns_live_session(lane_a),
            true,
            "a mid-session hello must not disturb the live session's record"
        );

        // The stopping-session handoff resolves A's session to A's device
        // (the zero-audio notice names exactly this).
        note_stopping_session(app.handle(), lane_a);
        let (stop_lane, device, _prefs) =
            take_stopping_session(app.handle()).expect("the stopping session resolves");
        assert_eq!(stop_lane, lane_a);
        assert_eq!(device, "Pixel 8");

        // A lane that never froze a session (B, the spectator) hands the
        // pipeline nothing - and consuming the slot once means a second
        // take resolves nothing either.
        assert!(take_stopping_session(app.handle()).is_none());
        note_stopping_session(app.handle(), _lane_b);
        assert!(
            take_stopping_session(app.handle()).is_none(),
            "a spectator lane with no session record resolves no stopping session"
        );
    }

    /// v1.6.2 W1-L2 (KB-110/KB-111 by construction): two connections each
    /// own their session independently. Both sessions are live at once;
    /// ending one flips exactly its lane's liveness (an independent stop),
    /// and the disconnect-finalize decision (the server consults
    /// live_session_lanes / lane liveness) sees only the dropped phone's
    /// session - the other phone keeps streaming into its own recorder.
    #[test]
    fn two_connections_own_independent_live_sessions() {
        let app = tauri::test::mock_app();
        let manager = Arc::new(CompanionManager::new());
        app.manage(Arc::clone(&manager));

        let (lane_a, _gen_a, _rx_a) = manager.register_connection("Pixel 8");
        let (lane_b, _gen_b, _rx_b) = manager.register_connection("iPhone 15");
        begin_session(app.handle(), lane_a, protocol::SessionPrefs::default());
        begin_session(app.handle(), lane_b, protocol::SessionPrefs::default());

        // Both live simultaneously - the one-shared-session era's
        // impossibility.
        assert!(manager.lane_owns_live_session(lane_a));
        assert!(manager.lane_owns_live_session(lane_b));
        assert_eq!(
            manager.live_session_lanes(),
            vec![lane_a, lane_b],
            "both lanes report live sessions for the teardown paths"
        );

        // Phone A stops: only its session ends. B's stays live.
        on_lane_session_changed(app.handle(), lane_a, false);
        assert!(!manager.lane_owns_live_session(lane_a));
        assert_eq!(
            manager.lane_owns_live_session(lane_b),
            true,
            "one phone's stop must not end the other's session (KB-110)"
        );
        assert_eq!(manager.live_session_lanes(), vec![lane_b]);

        // Phone B's socket dies: the disconnect path finalizes exactly
        // lane B, and A (already finished) is untouched by it.
        assert_eq!(manager.live_session_lanes(), vec![lane_b]);
        on_lane_session_changed(app.handle(), lane_b, false);
        assert!(manager.live_session_lanes().is_empty());
    }

    /// v1.6.2 W1-L2 (KB-255): paired-device bookkeeping keys on
    /// lane/generation, never the name. A stale socket's late unregister
    /// (keepalive death lags up to 20 s) carries the old pair and must not
    /// erase a fresh same-name registration; a matching pair removes
    /// exactly its own entry; lane ids are never reused.
    #[test]
    fn registrations_release_by_lane_and_generation_never_by_name() {
        let manager = CompanionManager::new();

        // Phone A connects under a name.
        let (lane_a, gen_a, _rx_a) = manager.register_connection("Pixel 8");
        assert_eq!(manager.devices(), vec!["Pixel 8".to_string()]);
        assert_eq!(manager.paired_device_count(), 1);

        // A quick reconnect: the fresh registration is a NEW lane and
        // generation under the SAME name.
        let (lane_b, gen_b, _rx_b) = manager.register_connection("Pixel 8");
        assert_ne!(lane_a, lane_b, "lane ids are per registration, never per name");
        assert_eq!(
            manager.paired_device_count(),
            1,
            "same-name registrations deduplicate for the tray count"
        );

        // The STALE socket's death arrives late: its (lane, generation)
        // pair no longer matches anything, so the fresh registration
        // survives and the paired count never zeroes (KB-255).
        manager.unregister_connection(lane_a, gen_a);
        assert_eq!(
            manager.devices(),
            vec!["Pixel 8".to_string()],
            "a stale socket's death must not erase a fresh same-name registration"
        );
        assert_eq!(manager.paired_device_count(), 1);

        // The fresh socket's own death removes exactly its entry.
        manager.unregister_connection(lane_b, gen_b);
        assert!(manager.devices().is_empty());
        assert_eq!(manager.paired_device_count(), 0);

        // Distinct names coexist and leave in registration order.
        let (lane_c, _gen_c, _rx_c) = manager.register_connection("iPhone 15");
        let (lane_d, _gen_d, _rx_d) = manager.register_connection("iPad");
        assert_eq!(
            manager.devices(),
            vec!["iPhone 15".to_string(), "iPad".to_string()]
        );
        manager.unregister_connection(lane_c, _gen_c);
        assert_eq!(manager.devices(), vec!["iPad".to_string()]);
        // Lane ids are never reused.
        let (lane_e, _gen_e, _rx_e) = manager.register_connection("iPhone 15");
        assert_ne!(lane_e, lane_c);
        assert_ne!(lane_e, lane_d);
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
                // v1.6.0 W1-L7: the pp hotkey's mid-session mode toggles.
                // The phone can toggle its own session's post-process leg
                // through its pp preference, but these two codes fire only
                // from the Mac keyboard's pp hotkey pressing during a live
                // session; the phone neither triggers them nor renders
                // them (its notice branch vibrates anything unrecognized).
                PostProcessSessionUpgraded,
                PostProcessSessionDowngraded,
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

    /// v1.6.0 (per-session prefs, v1.6.2 per-lane): the session's prefs
    /// ride the STARTING press's edge and freeze into ITS lane's session
    /// record at the single serialized instant the coordinator commits
    /// the start (begin_session) - never read back from the pending slot
    /// another connection can write concurrently, which was the
    /// arm-to-freeze race. The frozen copy survives later pending arms (a
    /// concurrent press from another phone must not retarget the live
    /// session) and the session's own end (the stop pipeline takes the
    /// copy AFTER the false edge). Defaults reproduce the pre-v1.6.0
    /// behavior: paste on the Mac, no post-process.
    #[test]
    fn session_prefs_freeze_per_lane_and_survive_the_stop_pipeline() {
        let app = tauri::test::mock_app();
        let manager = Arc::new(CompanionManager::new());
        app.manage(Arc::clone(&manager));

        let (lane_a, _gen_a, _rx_a) = manager.register_connection("Pixel 8");
        let (lane_b, _gen_b, _rx_b) = manager.register_connection("iPhone 15");
        let phone_target = protocol::SessionPrefs {
            output_target: protocol::OutputTarget::Phone,
            post_process: true,
        };

        // Phone A's press carried phone-target prefs; the coordinator's
        // Start-effect seam freezes exactly those into A's lane record.
        begin_session(app.handle(), lane_a, phone_target);
        // A concurrent press from another phone only arms the pending slot
        // (the tray's next-press display); A's session keeps its own.
        manager.set_pending_session_prefs(protocol::SessionPrefs::default());

        // The session ends; the stop handoff still resolves A's session's
        // copy (the false edge must not clear what the pipeline reads).
        on_lane_session_changed(app.handle(), lane_a, false);
        note_stopping_session(app.handle(), lane_a);
        let (stop_lane, _device, prefs) =
            take_stopping_session(app.handle()).expect("A's stopping session resolves");
        assert_eq!(stop_lane, lane_a);
        assert_eq!(prefs, phone_target);

        // The NEXT session on another lane freezes ITS own prefs, whatever
        // later presses left in the pending slot.
        begin_session(app.handle(), lane_b, protocol::SessionPrefs::default());
        note_stopping_session(app.handle(), lane_b);
        let (_, _, prefs_b) = take_stopping_session(app.handle()).expect("B resolves");
        assert_eq!(prefs_b, Default::default());
    }

    /// v1.6.2 W1-L2 wiring, exercised through real app state: the phone
    /// delivery seam pushes the TranscriptResult wire frame down the
    /// session's OWN lane - never the broadcast channel every paired
    /// phone subscribes to - and the per-lane State frames flip exactly
    /// the recording phone's button. Two registered connections plus a
    /// broadcast subscriber, asserted from a mock app; the stop
    /// pipeline's own invocation of these seams is pinned in actions.rs
    /// (the pipeline needs a full recorder/model rig a unit test cannot
    /// mount).
    #[test]
    fn transcript_delivery_and_state_frames_flow_through_app_state_per_lane() {
        let app = tauri::test::mock_app();
        let manager = Arc::new(CompanionManager::new());
        app.manage(Arc::clone(&manager));

        // Unmanaged helper behavior: no stopping session resolves.
        let bare = tauri::test::mock_app();
        assert!(take_stopping_session(bare.handle()).is_none());
        assert_eq!(starting_session_lane(bare.handle()), None);

        let (pixel_lane, _gen_p, mut pixel_lane_rx) = manager.register_connection("Pixel 8");
        let (_bystander_lane, _gen_b, mut bystander_lane_rx) =
            manager.register_connection("iPhone 15");
        let mut broadcast = manager.subscribe();

        // The session: Pixel 8's press carried phone-target prefs AND its
        // connection's lane id; the Start-effect seam froze both.
        begin_session(
            app.handle(),
            pixel_lane,
            protocol::SessionPrefs {
                output_target: protocol::OutputTarget::Phone,
                post_process: false,
            },
        );
        assert_eq!(starting_session_lane(app.handle()), Some(pixel_lane));
        // Phone-target delivery: the session's own phone receives the exact
        // wire frame the stop pipeline's PhoneSend branch sends.
        deliver_transcript_to_lane(
            app.handle(),
            pixel_lane,
            "hello from the phone lane",
            true,
            "whisper-large-v3-turbo",
        );
        let frame = pixel_lane_rx
            .try_recv()
            .expect("the TranscriptResult reaches the session device's lane");
        assert!(frame.contains(r#""type":"transcript""#), "frame: {frame}");
        assert!(frame.contains("hello from the phone lane"));
        assert!(frame.contains(r#""postProcessed":true"#));
        assert!(frame.contains(r#""model":"whisper-large-v3-turbo""#));
        // The bystander phone's lane stays empty, and so does the broadcast
        // channel: the transcript is targeted, full stop.
        assert!(
            bystander_lane_rx.try_recv().is_err(),
            "a bystander phone must not receive another phone's transcript"
        );
        assert!(
            broadcast.try_recv().is_err(),
            "transcripts must never ride the broadcast channel"
        );

        // v1.6.2 W1-L2: State frames are per-lane. Pixel 8's session going
        // live flips ONLY its own connection's button; the bystander
        // (whose client gates audio streaming on its own recording flag)
        // never streams a chunk it would only have dropped.
        on_lane_session_changed(app.handle(), pixel_lane, true);
        let state = pixel_lane_rx
            .try_recv()
            .expect("the State frame reaches the recording phone's lane");
        assert!(state.contains(r#""type":"state""#));
        assert!(state.contains(r#""recording":true"#));
        assert!(
            bystander_lane_rx.try_recv().is_err(),
            "another phone's session must not flip this phone's button"
        );
        assert!(
            broadcast.try_recv().is_err(),
            "session State frames must never ride the broadcast channel"
        );

        // The stop pipeline flipped the session not-live long before this
        // point; a NEW phone's hello during that window registers a fresh
        // lane - it cannot redirect the frozen session's delivery.
        on_lane_session_changed(app.handle(), pixel_lane, false);
        assert!(
            pixel_lane_rx
                .try_recv()
                .is_ok_and(|frame| frame.contains(r#""recording":false"#)),
            "the lane's own State frame lands on its lane"
        );
        let (late_lane, _gen_l, mut late_lane_rx) = manager.register_connection("iPad");
        let _ = late_lane;
        deliver_transcript_to_lane(app.handle(), pixel_lane, "frozen aim", false, "m");
        assert!(
            pixel_lane_rx
                .try_recv()
                .is_ok_and(|frame| frame.contains("frozen aim")),
            "the frozen lane keeps receiving even after a later lane registration"
        );
        assert!(late_lane_rx.try_recv().is_err());

        // Delivery with no live lane for the session (the phone dropped) is
        // the documented no-op, not a bystander broadcast.
        manager.unregister_connection(pixel_lane, _gen_p);
        deliver_transcript_to_lane(app.handle(), pixel_lane, "after the drop", false, "m");
        assert!(pixel_lane_rx.try_recv().is_err());
        assert!(
            late_lane_rx.try_recv().is_err(),
            "a dropped phone's transcript must not fall back to bystanders"
        );
        assert!(broadcast.try_recv().is_err());
    }

    /// The targeted lanes' registry semantics (wrong-phone fix): lane ids
    /// are SERVER-assigned per connection and never derive from the
    /// client-controlled hello name, so two connections claiming the SAME
    /// device name get distinct lanes - neither can seize or supersede
    /// the other - and a frozen session's lane cannot be redirected by
    /// any later registration. Release removes exactly the named lane; a
    /// session still pointing at a released lane reads the documented
    /// no-op.
    #[test]
    fn device_lanes_are_per_connection_and_never_name_seizable() {
        let manager = CompanionManager::new();

        // Two connections claiming the SAME name: distinct lanes, both
        // live, no supersession.
        let (lane_a, gen_a, mut rx_a) = manager.register_connection("Pixel 8");
        let (lane_b, _gen_b, mut rx_b) = manager.register_connection("Pixel 8");
        assert_ne!(lane_a, lane_b, "lane ids are per connection, not per name");

        assert!(
            manager.send_frame_to_lane(lane_a, &protocol::ServerFrame::State { recording: true })
        );
        assert!(rx_a.try_recv().is_ok());
        assert!(
            rx_b.try_recv().is_err(),
            "a same-name connection cannot seize another connection's lane"
        );

        // A frozen session keeps its own lane even after later
        // registrations; a send to the frozen lane is unaffected.
        let (_lane_c, _gen_c, mut rx_c) = manager.register_connection("iPad");
        assert!(
            manager.send_frame_to_lane(lane_a, &protocol::ServerFrame::State { recording: false })
        );
        assert!(rx_a.try_recv().is_ok());
        assert!(rx_c.try_recv().is_err());

        // Release removes exactly one lane; ids are never reused.
        manager.unregister_connection(lane_a, gen_a);
        assert!(
            !manager.send_frame_to_lane(lane_a, &protocol::ServerFrame::State { recording: false }),
            "a released lane reads as the dropped-phone no-op"
        );
        assert!(
            manager.send_frame_to_lane(lane_b, &protocol::ServerFrame::State { recording: false })
        );
        assert!(rx_b.try_recv().is_ok());
        assert!(rx_c.try_recv().is_err());
        let (lane_d, _gen_d, _rx_d) = manager.register_connection("iPhone 15");
        assert_ne!(lane_d, lane_a, "lane ids are never reused");
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
            restart_needed(
                &Ok(Ipv4Addr::new(100, 101, 7, 24)),
                4177,
                None,
                false,
                &live
            ),
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
        assert_eq!(
            url.as_deref(),
            Some("https://mac.tail1234.ts.net:4177/#t=tok")
        );
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
            advertised_url_qr(
                None,
                Some(tailscale),
                Some("tok"),
                Some("192.168.1.0/24"),
                4177
            ),
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
        let (url, _) = advertised_url_qr(
            None,
            Some(bridge),
            Some("tok"),
            Some("192.168.1.0/24"),
            4177,
        );
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
