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
    /// The hello's IDENTITY name: the client's display name tagged with
    /// its persistent device id (see `protocol::device_identity_name`),
    /// unique per phone where the UA-derived display name alone is a
    /// platform token every same-platform phone shares. Keyed into the
    /// session freeze and history's capture_device, so per-phone data (the
    /// Recent list) can never collapse two same-platform phones.
    device: String,
    tx: tokio::sync::mpsc::UnboundedSender<String>,
}

/// KB-261 (v1.6.4): one authenticated connection's registration outcome.
/// `device` is the connection's exact raw identity (see
/// [`protocol::device_identity_name`]); the lane and generation are the
/// KB-255 registration keys; `persist_identities` is Some when this
/// identity was not already known and the CALLER owns persisting the
/// bounded set off the connection task (see [`persist_known_identities`]).
pub struct ConnectionRegistration {
    pub device: String,
    pub lane: u64,
    pub generation: u64,
    pub persist_identities: Option<Vec<String>>,
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
    // Locking note (v1.6.4, the crashed-client accept-wedge hardening):
    // every std Mutex this manager owns is acquired poison-tolerantly
    // (`lock().unwrap_or_else(|e| e.into_inner())`). A panicking task that
    // held one of these locks used to poison it, and every later
    // connection task then panicked at its own `.lock().unwrap()` - the
    // server answered no new phone until restart (the v1.6.3 proof-run
    // wedge shape). These are bookkeeping maps whose invariants survive a
    // mid-critical-section panic (insert/remove/flag flips), so recovery
    // beats poisoning the whole companion surface.
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
    /// KB-261 (v1.6.4): the phone identities this Mac has paired, in-memory
    /// and authoritative (seeded from the persisted
    /// `companion_known_identities` at server start, bounded to
    /// KNOWN_IDENTITIES_LIMIT). The durable record of WHICH phones paired:
    /// an exact re-pair owes no persistence, and a restart reseeds from it.
    /// Fleet pass 1: it is no longer an adoption matcher - identities are
    /// exact (see [`Self::register_connection_with_identity`]).
    known_identities: Mutex<Vec<String>>,
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
            known_identities: Mutex::new(Vec::new()),
            failure_limiter: Mutex::new(auth::FailureLimiter::new(
                5,
                Duration::from_secs(60),
                Duration::from_secs(60),
            )),
            notice_listener: Mutex::new(None),
        }
    }

    pub fn is_running(&self) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).running
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
    ) -> (u64, u64, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let lane = self
            .next_lane_id
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let generation = self
            .next_generation
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.connections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(ConnectionEntry {
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
        // KB-261 (fleet pass 1): this whole-settings read-modify-write rides
        // the connect-edge lock, because the same first-pairing hello also
        // spawns the persist_known_identities worker - unsynchronized, the
        // interleave [persist reads][connected reads][persist writes]
        // [connected writes] let the last whole-settings writer erase the
        // other's field.
        connect_edge_settings_rmw(app, &|settings| {
            if settings.companion_last_device.as_deref() != Some(device) {
                settings.companion_last_device = Some(device.to_string());
                true
            } else {
                false
            }
        });
        let _ = crate::tray::update_tray_menu(app);
    }

    /// v1.6.2 W1-L2 (KB-255): release a connection's registration. Both
    /// keys must match: the lane id (never reused) and the registration
    /// generation. A stale socket whose keepalive death lagged behind a
    /// same-name reconnect carries the OLD pair, which no longer matches
    /// any entry, so the fresh registration survives and the tray's paired
    /// count stays honest. Registry-only (the tray re-sync lives in
    /// [`Self::note_device_disconnected`]).
    pub fn unregister_connection(&self, lane: u64, generation: u64) {
        let mut connections = self.connections.lock().unwrap_or_else(|e| e.into_inner());
        connections.retain(|entry| !(entry.lane == lane && entry.generation == generation));
    }

    /// The disconnect-edge tray re-sync (the forget_device rule, now
    /// keyed on registrations instead of names).
    pub fn note_device_disconnected(&self, app: &AppHandle) {
        let _ = crate::tray::update_tray_menu(app);
    }

    /// The deduplicated device names of the live connections, in
    /// registration order (the settings panel's device list).
    pub fn devices(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for entry in self
            .connections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
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
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|entry| entry.lane == lane)
            .map(|entry| entry.device.clone())
    }

    /// KB-261 (v1.6.4, fleet pass 1): register one authenticated
    /// connection under its EXACT identity. The identity is the hello's
    /// `name#tag` composite ([`protocol::device_identity_name`]); the
    /// Recent list, registration, and history attribution key on it, and a
    /// phone keeps all three across Mac restarts and re-pairs as long as
    /// its stored device id survives - which it does while the pairing
    /// URL's origin is stable.
    ///
    /// Fleet pass 1 REMOVED platform-name adoption, so one phone's data
    /// can never fold into another's. The v1.6.4 cut let a fresh-id hello
    /// adopt a known identity on platform-name equality alone; that match
    /// key is client-controlled and every same-platform phone shares it,
    /// so any pairing-token holder helloing while the prior phone was away
    /// (asleep, off-WiFi - the normal steady state) received the prior's
    /// Recent list and transcript history, and its own dictations were
    /// attributed to the prior - the KB-265 defect class, between
    /// household token holders, in the common two-Android-phones case
    /// where both hellos say "Linux". Adoption also broke itself: the
    /// ordinary re-pair (a network switch leaves the old socket registered
    /// up to the keepalive deadline while the phone hellos from the new
    /// origin) was refused by the live-connection guard, minted a fresh
    /// id, and pushed it to the known set - after which two same-base
    /// entries made the platform match ambiguous forever, silently. And
    /// bare no-id hellos adopted known tagged identities in both
    /// directions, violating the pinned protocol invariant that a client
    /// with no id keeps the bare name. The disclosed cost of exactness: an
    /// origin change (new LAN IP, Tailscale-vs-LAN) mints a fresh
    /// localStorage id on the phone, so its Recent list resets - the
    /// pre-v1.6.4 behavior, now safe.
    ///
    /// RACE FREEDOM: the registration insert and the known-set update
    /// happen inside ONE hold of the connections lock (lock order:
    /// connections, then known_identities; no other site takes both), so
    /// the devices list and the pairing record never observe a half-state
    /// under a reconnect storm. The method performs NO settings IO (the
    /// connection task only touches in-memory maps); persisting a newly
    /// known identity is the caller's job through the returned
    /// `persist_identities` payload (see `persist_known_identities`).
    pub fn register_connection_with_identity(
        &self,
        device: &str,
        device_id: Option<&str>,
    ) -> (
        ConnectionRegistration,
        tokio::sync::mpsc::UnboundedReceiver<String>,
    ) {
        let raw = protocol::device_identity_name(device, device_id);
        let lane = self
            .next_lane_id
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let generation = self
            .next_generation
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        // The atomic insert + known-set update: connections first, then
        // the known set; both held until the entry is pushed so no
        // concurrent registration can observe the half-state.
        let mut connections = self.connections.lock().unwrap_or_else(|e| e.into_inner());
        let mut known = self
            .known_identities
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let identity = raw.clone();
        let mut persist_identities = None;
        if !known.iter().any(|k| k == &raw) {
            // A genuinely new identity: remember it (bounded, most recent
            // last) so a restart's seed knows this phone paired. The
            // in-memory set is authoritative; the persisted copy is the
            // caller's payload.
            known.push(raw.clone());
            let keep_from = known.len().saturating_sub(KNOWN_IDENTITIES_LIMIT);
            *known = known.split_off(keep_from);
            persist_identities = Some(known.clone());
        }

        connections.push(ConnectionEntry {
            lane,
            generation,
            device: identity.clone(),
            tx,
        });
        drop(known);
        drop(connections);

        (
            ConnectionRegistration {
                device: identity,
                lane,
                generation,
                persist_identities,
            },
            rx,
        )
    }

    /// KB-261: the current in-memory known-identity set (what
    /// `persist_known_identities` writes and start() seeds).
    fn known_identities_snapshot(&self) -> Vec<String> {
        self.known_identities
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
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
            .unwrap_or_else(|e| e.into_inner())
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
            .unwrap_or_else(|e| e.into_inner())
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
        *self.pending_prefs.lock().unwrap_or_else(|e| e.into_inner()) = prefs;
    }

    /// v1.6.0: the preferences the NEXT companion session will run with
    /// (armed by the phones' press edges, defaults before any). The tray's
    /// companion status block reads this as the active output target.
    pub fn pending_session_prefs(&self) -> protocol::SessionPrefs {
        *self.pending_prefs.lock().unwrap_or_else(|e| e.into_inner())
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
        let connections = self.connections.lock().unwrap_or_else(|e| e.into_inner());
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

        // KB-261 (v1.6.4): seed the in-memory known-identity cache from the
        // persisted store here, on the enable path, so the hello path never
        // reads settings for the identity resolution (the connection task
        // only touches the in-memory maps from here on).
        *self
            .known_identities
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = settings.companion_known_identities.clone();

        // Pairing token: generate on enable, and rebind when the advertised
        // subnet changed (a token from another network never validates).
        // A corroborated Tailscale bind (KB-259: CGNAT range on a utun
        // interface) issues the token for the whole CGNAT /10, so every
        // tailnet peer validates (ip::pairing_subnet_for).
        // KB-289: the rebind's whole-settings write rides the serialized
        // RMW lock (the connect-edge workers' lock) instead of a bare
        // get/mutate/write outside any lock, so no other RIDER of that
        // lock can interleave a lost update against it. The guarantee is
        // scoped (see connect_edge_settings_rmw): the app's whole-settings
        // writers outside the lock - the shortcut rebinding among them -
        // can still interleave with it.
        let subnet = ip::pairing_subnet_for(&candidates, bind_ip);
        connect_edge_settings_rmw(app, &|settings| {
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
                true
            } else {
                false
            }
        });
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
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.running = true;
            inner.bind_ip = Some(bind_ip);
            inner.port = port;
            inner.fingerprint = Some(fingerprint);
            inner.advertised_host = advertised_host.clone();
            inner.tls_manual = tls_manual;
        }

        // KB-028 residual (fleet, cycle 14): a close parked by the previous
        // stop() must not survive into this server epoch. The parked close
        // reclaims idle lane recorders once the last session ends while the
        // server is STOPPED; this start() just bound a live listener, and
        // the phones are about to re-hello and pre-warm their recorders
        // against the new epoch - a parked close running at the next
        // session-end fold would close a re-hello'd phone's pre-warmed
        // recorder and force-stop a lane whose start committed after that
        // fold's is_recording() check. Clearing here keeps the flag parked
        // only while the server is stopped, the premise
        // run_deferred_remote_close already documents.
        if let Some(rm) = app.try_state::<Arc<crate::managers::audio::AudioRecordingManager>>() {
            rm.clear_deferred_remote_close();
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
        // Fleet pass 2 (KB-270 review): those finalize commands are
        // ASYNCHRONOUS (mpsc sends; the coordinator thread turns each into
        // a Stop effect whose pipeline reads THIS lane's session record
        // through note_stopping_session), so this thread must not touch
        // the session map at all - the pre-review synchronous clear raced
        // every pipeline into session_lane None and silently discarded the
        // dictation, and the retain-live-only sweep that replaced it
        // removed exactly the false-edged-but-unconsumed records whose
        // cancel pipelines were still in flight. Records are consumed (and
        // removed) by their own pipelines now, so the map here holds only
        // records those queued commands will consume; never touch the
        // stopping slot either (it is transient by design:
        // note_stopping_session writes it and take_stopping_session
        // consumes it on the same thread).

        self.broadcast_frame(&protocol::ServerFrame::Goodbye {
            reason: "server_stopping",
        });
        let _ = self.shutdown_tx.send(true);

        // Close the lane recorders unless a stop pipeline is using them
        // right now (each pipeline closes its own at its end). KB-028
        // (v1.6.5): the skip used to be SILENT - a disable during a LOCAL
        // recording skipped the close and nothing re-triggered it after
        // the session ended, leaking every pre-warmed idle lane recorder
        // (ring + VAD session) until the next companion start/stop or app
        // shutdown. The helper parks the skipped close on the audio
        // manager and every session-end fold runs it once nothing is
        // recording anywhere.
        if let Some(rm) = app.try_state::<Arc<crate::managers::audio::AudioRecordingManager>>() {
            rm.close_remote_stream_or_defer();
        }

        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.running = false;
        inner.bind_ip = None;
        inner.port = 0;
        inner.advertised_host = None;
        inner.tls_manual = false;
        self.connections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
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
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
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
            &candidates,
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
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_error = error;
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
        let subnet = ip::pairing_subnet_for(&candidates, bind_ip);
        // KB-289: the fresh token persists through the serialized RMW lock
        // (the connect-edge workers' lock), not a bare whole-settings
        // write outside any lock - a concurrent connect-edge writer could
        // otherwise interleave a lost update here.
        connect_edge_settings_rmw(app, &|settings| {
            settings.companion_pairing_token = Some(auth::generate_pairing_token());
            settings.companion_pairing_subnet = Some(subnet.clone());
            true
        });

        self.restart(app)
    }

    /// Register the once-per-app notice forwarder (overlay notices ride to
    /// the paired phones too, so a swallowed busy press is visible on both
    /// surfaces).
    pub fn register_notice_forwarder(self: &Arc<Self>, app: &AppHandle) {
        let mut guard = self
            .notice_listener
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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

/// KB-261: how many phone identities the Mac remembers. Bounded so a
/// kiosk Mac strangers keep scanning cannot grow the settings store
/// forever; the most recent identities survive.
const KNOWN_IDENTITIES_LIMIT: usize = 8;

/// KB-261 (fleet pass 1): serializes the connect-edge settings
/// read-modify-write cycles. A first-pairing hello spawns
/// [`persist_known_identities`] and
/// [`CompanionManager::note_device_connected`] as two detached workers,
/// and each does get_settings -> mutate one field -> write_settings over
/// the WHOLE settings object; unsynchronized, the interleave [persist
/// reads][connected reads][persist writes][connected writes] let the last
/// whole-settings writer erase the other's field - exactly the same
/// pairing that mints a new identity also changes the last device, so
/// both workers fire together on the feature's primary scenario, and a
/// lost identities persist silently shrank the Mac's pairing record
/// after the next restart (start() re-seeds from the store).
static CONNECT_EDGE_SETTINGS_RMW: Mutex<()> = Mutex::new(());

/// One settings read-modify-write cycle under [`CONNECT_EDGE_SETTINGS_RMW`]:
/// the guard spans get_settings through write_settings, so the riders of
/// this lock serialize with one another and each rider's read sees every
/// other rider's write. The closure returns whether the field changed; an
/// unchanged field writes nothing.
///
/// KB-289 (v1.6.5): the lock is the serialized RMW for every writer of
/// the companion pairing/identity fields and log_level - companion
/// `start()`'s pairing-token rebind, [`CompanionManager::reset_pairing`],
/// and the `set_log_level` command ride the same cycle the connect-edge
/// workers already did - each used to whole-settings-write outside any
/// lock, so a connect-edge interleave could erase their field (the same
/// lost update the connect-edge workers serialize against). The
/// guarantee is NOT store-wide: get_settings and write_settings remain
/// separate lock-free store operations and the app still holds
/// whole-settings writers that ride no lock (the shortcut rebinding's
/// change_binding among them), so a rider can still interleave a lost
/// update with one of THOSE; closing that would need a store-level lock
/// touching every caller, the blast radius this fix deliberately avoids.
/// The historical name stays: the KB-261 pin and the ledger anchors cite
/// it.
pub(crate) fn connect_edge_settings_rmw(
    app: &AppHandle,
    mutate: &dyn Fn(&mut crate::settings::AppSettings) -> bool,
) {
    let _guard = CONNECT_EDGE_SETTINGS_RMW
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut settings = crate::settings::get_settings(app);
    if mutate(&mut settings) {
        crate::settings::write_settings(app, settings);
    }
}

/// KB-261 (v1.6.4): persist the manager's CURRENT known-identity set to
/// settings, off the connection task (the caller owns the thread). Reads
/// the fresh in-memory set at run time - not a stale payload - so
/// out-of-order persist workers converge to the latest set, and merges
/// into the CURRENT settings (this field only) so a concurrent writer of
/// another field is not clobbered by a stale whole-settings copy - a
/// claim fleet pass 1 makes TRUE by running under
/// [`CONNECT_EDGE_SETTINGS_RMW`] with note_device_connected's cycle. Best
/// effort by design: a lost persisted entry costs one phone one
/// Recent-list reset after a restart, never a wrong merge.
pub(crate) fn persist_known_identities(app: &AppHandle, manager: &CompanionManager) {
    let identities = manager.known_identities_snapshot();
    connect_edge_settings_rmw(app, &|settings| {
        if settings.companion_known_identities != identities {
            settings.companion_known_identities = identities.clone();
            true
        } else {
            false
        }
    });
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
pub fn on_lane_session_changed<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    lane: u64,
    active: bool,
) {
    let Some(manager) = manager(app) else {
        return;
    };
    let mut sessions = manager
        .lane_sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // Fleet pass 2 (KB-270 review): the record is KEPT here on the false
    // edge, as live=false - the v1.6.4 sweep removed it at this edge on
    // the assumption the stop pipeline had already consumed the identity,
    // but the cancel path raises this same false edge BEFORE its pipeline
    // runs note_stopping_session (the coordinator's cancel arm), so the
    // removal raced the handoff: note_stopping_session found no record,
    // take_stopping_session returned None, and the stop pipeline fell back
    // to companion_last_device - the most recent phone to hello, possibly
    // a DIFFERENT phone - attributing the dictation's history row and
    // Recent-list routing to the wrong identity. Removal now happens at
    // the consumption point (note_stopping_session), so the record is
    // observably identical to the pre-branch live=false drain: every
    // present record has a pipeline that will consume it, nothing
    // accumulates (KB-270's growth bound holds: consumption removes), and
    // the next session on the lane re-inserts a fresh record at its own
    // Start commit.
    let device = match sessions.get_mut(&lane) {
        Some(session) => {
            session.live = active;
            Some(session.device.clone())
        }
        // A live edge with no record is meaningless (no session was ever
        // committed for this lane); a FALSE edge without one is the
        // normal post-consumption stop (the pipeline already took the
        // identity) and must still send the State frame and clear the
        // overlay - returning early here would leave the phone's record
        // button stuck on "recording".
        None if active => return,
        None => None,
    };
    drop(sessions);
    manager.send_frame_to_lane(lane, &protocol::ServerFrame::State { recording: active });
    let _ = app.emit_to(
        "recording_overlay",
        "companion-session-changed",
        serde_json::json!({
            "active": active,
            "device": device.filter(|_| active),
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
        manager
            .lane_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(lane, session);
        *manager
            .starting_lane
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(lane);
    }
}

/// v1.6.2 W1-L2: the lane of the companion session whose Start effect is
/// being executed (read back by the shared action's start, synchronously
/// behind begin_session on the coordinator thread). None for
/// keyboard/signal starts.
pub fn starting_session_lane<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> Option<u64> {
    manager(app).and_then(|m| *m.starting_lane.lock().unwrap_or_else(|e| e.into_inner()))
}

/// v1.6.2 multi-phone review fix: clear the starting-session slot for a
/// KEYBOARD start. begin_session is the only writer, so before this the
/// slot held the LAST phone session's lane forever after: the accessor's
/// documented contract ("None for keyboard/signal starts") was false the
/// moment any phone had ever dictated, and every consumer had to defend
/// itself with a source re-check to keep the stale lane inert. The
/// coordinator's Start arm now writes the slot for EVERY start - the
/// lane for a companion session, None for the keyboard's - so the slot
/// is always fresh for the effect being executed.
pub fn clear_starting_session<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    if let Some(manager) = manager(app) {
        *manager
            .starting_lane
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// v1.6.2 W1-L2: drop a lane's session record entirely (a start that never
/// began recording - the rollback path - so a later session on the same
/// lane starts clean).
pub fn discard_session<R: tauri::Runtime>(app: &tauri::AppHandle<R>, lane: u64) {
    if let Some(manager) = manager(app) {
        manager
            .lane_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&lane);
    }
}

/// v1.6.2 W1-L2: hand the stopping session's identity from the coordinator
/// thread (which knows the lane its Stop effect ends) to the shared
/// pipeline, whose TranscribeAction signature predates lanes. Called
/// immediately before the Stop effect runs; consumed exactly once by the
/// pipeline's first synchronous step on the same thread.
///
/// Fleet pass 2 (KB-270 review): this handoff is also the record's
/// CONSUMPTION point - the record is removed here, not at the session's
/// false edge, because the cancel path false-edges the session BEFORE its
/// pipeline reaches this seam (the coordinator's cancel arm), and a record
/// removed at the edge left this seam nothing to hand over (the pipeline
/// then fell back to companion_last_device, attributing the dictation to
/// whatever phone hello'd most recently). Removal here keeps KB-270's
/// growth bound exactly: a record exists from its Start commit to its
/// pipeline's handoff, nothing accumulates on a flapping phone.
pub fn note_stopping_session<R: tauri::Runtime>(app: &tauri::AppHandle<R>, lane: u64) {
    let Some(manager) = manager(app) else {
        return;
    };
    let mut sessions = manager
        .lane_sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(session) = sessions.remove(&lane) {
        *manager.stopping.lock().unwrap_or_else(|e| e.into_inner()) = Some(StoppingSession {
            lane,
            device: session.device,
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
    let stopping = manager
        .stopping
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()?;
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
    match hm.recent_companion_entries_for_device(device, protocol::RECENT_HISTORY_LIMIT) {
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
        .unwrap_or_else(|e| e.into_inner())
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
/// `candidates` carries the current enumeration so the expected subnet
/// corroborates the Tailscale tier through the interface name (KB-259);
/// a live bind that is no longer enumerated falls back to the narrow
/// /24, which merely suppresses this preview.
fn advertised_url_qr(
    advertised_host: Option<&str>,
    effective_ip: Option<Ipv4Addr>,
    token: Option<&str>,
    stored_pairing_subnet: Option<&str>,
    port: u16,
    candidates: &[ip::InterfaceCandidate],
) -> (Option<String>, Option<String>) {
    let Some(ip) = effective_ip else {
        return (None, None);
    };
    let Some(token) = token.filter(|t| !t.is_empty()) else {
        return (None, None);
    };
    // A token issued for another subnet never validates (auth::same_subnet
    // refuses); pairing_subnet_for widens corroborated Tailscale binds to
    // the CGNAT /10.
    if stored_pairing_subnet != Some(ip::pairing_subnet_for(candidates, ip).as_str()) {
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

    /// v1.6.2 multi-phone review pin: the starting-session slot is fresh
    /// for every start. begin_session is the only writer, so before the
    /// clear the slot held the LAST phone session's lane forever - the
    /// accessor's documented contract ("None for keyboard/signal starts")
    /// was false after any phone had ever dictated, and every consumer
    /// had to re-check the capture source to keep the stale lane inert.
    /// The coordinator's Start arm writes the slot for every start; this
    /// pins the keyboard arm's clear against that stale residue.
    #[test]
    fn the_starting_slot_reads_none_for_keyboard_starts_after_any_phone_session() {
        let app = tauri::test::mock_app();
        let manager = Arc::new(CompanionManager::new());
        app.manage(Arc::clone(&manager));
        assert_eq!(starting_session_lane(app.handle()), None);

        // A phone session commits and its start resolves.
        let (lane, _gen, _rx) = manager.register_connection("Pixel 8");
        begin_session(app.handle(), lane, protocol::SessionPrefs::default());
        assert_eq!(starting_session_lane(app.handle()), Some(lane));

        // The next KEYBOARD start's effect clears the slot before the
        // shared action reads it, so the contract holds again.
        clear_starting_session(app.handle());
        assert_eq!(
            starting_session_lane(app.handle()),
            None,
            "a keyboard start must never inherit the last phone session's lane"
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

    /// Fleet pass 2 (KB-270 review): session records survive until their
    /// pipeline CONSUMES them, in both stop orders. The v1.6.4 edge sweep
    /// removed the record at on_lane_session_changed(false), which raced
    /// the cancel path (its false edge lands BEFORE the pipeline's
    /// note_stopping_session): the handoff found nothing, and the stop
    /// pipeline attributed the dictation to companion_last_device - the
    /// most recent phone to hello, possibly a different phone. Now the
    /// false edge only drains (live=false) and note_stopping_session is
    /// the consumption point that removes.
    #[test]
    fn server_stop_leaves_records_for_their_pipelines_in_both_stop_orders() {
        let app = tauri::test::mock_app();
        let manager = Arc::new(CompanionManager::new());
        app.manage(Arc::clone(&manager));

        let (lane_a, _gen_a, _rx_a) = manager.register_connection("iPhone#7f3a");
        let (lane_b, _gen_b, _rx_b) = manager.register_connection("iPhone#c3d4");
        let prefs = protocol::SessionPrefs {
            output_target: protocol::OutputTarget::Phone,
            post_process: false,
        };
        begin_session(app.handle(), lane_a, prefs.clone());
        begin_session(app.handle(), lane_b, prefs.clone());

        // The ordinary order (a release-edge Stop): the coordinator's Stop
        // effect hands the identity over first, the false edge lands after.
        note_stopping_session(app.handle(), lane_a);
        let stopping = take_stopping_session(app.handle());
        assert_eq!(
            stopping,
            Some((lane_a, "iPhone#7f3a".to_string(), prefs.clone())),
            "the ordinary stop order resolves the identity"
        );
        on_lane_session_changed(app.handle(), lane_a, false);
        assert!(
            !manager.lane_owns_live_session(lane_a),
            "the consumed lane is no longer live"
        );

        // The CANCEL order (the review's finding): the false edge lands
        // FIRST (the coordinator's cancel arm), the pipeline's handoff
        // runs later. The drained record must still be there to hand over
        // ITS OWN identity - not companion_last_device's.
        on_lane_session_changed(app.handle(), lane_b, false);
        assert!(
            !manager.lane_owns_live_session(lane_b),
            "the false edge drained the session"
        );
        note_stopping_session(app.handle(), lane_b);
        let stopping = take_stopping_session(app.handle());
        assert_eq!(
            stopping,
            Some((lane_b, "iPhone#c3d4".to_string(), prefs)),
            "a session false-edged before its pipeline still hands over its OWN identity"
        );

        // Consumption removed both records: nothing accumulates.
        assert!(
            manager
                .lane_sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty(),
            "consumption removes the record, so no drained entry outlives its pipeline"
        );
        // The stopping slot was consumed exactly once (it is transient:
        // written by the Stop effect, consumed exactly once).
        assert!(take_stopping_session(app.handle()).is_none());
        // And a second handoff for an already-consumed lane hands out
        // nothing (a drained record no longer exists to re-adopt).
        note_stopping_session(app.handle(), lane_b);
        assert!(
            take_stopping_session(app.handle()).is_none(),
            "a consumed record no longer hands out an identity"
        );
    }

    /// The source-level companion of the test above: stop() must not touch
    /// the session map at all - no synchronous clear (the pre-fix race
    /// that discarded dictations), no retain-based sweep (the fleet pass 2
    /// finding: it removed exactly the false-edged-but-unconsumed records
    /// whose cancel pipelines were still in flight), no stopping-slot
    /// surgery. Mounting a RUNNING TLS server in a unit test is not
    /// practical, so the arm is pinned against this committed source (the
    /// same convention as the coordinator's executor pins).
    #[test]
    fn stop_leaves_the_session_map_to_its_pipelines() {
        let source = include_str!("mod.rs");
        let at = source
            .find("pub fn stop(&self, app: &AppHandle)")
            .expect("stop");
        let end = at + source[at..].find("\n    }\n").expect("fn end");
        let body = &source[at..end];
        assert!(
            !body.contains("lane_sessions"),
            "stop() must not touch the session map: every record still present belongs to a pipeline that will consume it (live lanes' queued finalizes and false-edged lanes' in-flight stop pipelines)"
        );
        assert!(
            !body.contains("retain_live_lane_sessions"),
            "the retain sweep is gone: it removed pending-consumption records"
        );
        assert!(
            !body.contains("stopping.lock"),
            "stop() must never touch the stopping slot: note_stopping_session writes it and take_stopping_session consumes it on the pipeline thread"
        );
    }

    /// KB-028 (v1.6.5), source pin (same convention as the test above -
    /// mounting a RUNNING TLS server in a unit test is not practical):
    /// stop()'s lane-recorder close may never be SILENTLY skipped while a
    /// recording is live. The pre-fix guard - close only when
    /// `!rm.is_recording()` - leaked every pre-warmed idle lane recorder
    /// whenever a LOCAL recording was the skip's reason: the local session
    /// has no lane pipeline, so nothing re-triggered the close after it
    /// ended, and the recorders (ring + VAD session) lived on until the
    /// next companion start/stop or app shutdown. The close now routes
    /// through the audio manager's close_remote_stream_or_defer, which
    /// parks the skipped close and runs it at the last session end.
    #[test]
    fn kb028_stop_defers_the_lane_recorder_close_instead_of_silently_skipping_it() {
        let source = include_str!("mod.rs");
        let at = source
            .find("pub fn stop(&self, app: &AppHandle)")
            .expect("stop");
        let end = at + source[at..].find("\n    }\n").expect("fn end");
        let body = &source[at..end];
        assert!(
            body.contains("rm.close_remote_stream_or_defer();"),
            "the lane-recorder close must route through the park-and-run helper"
        );
        assert!(
            !body.contains("if !rm.is_recording()"),
            "the silent skip is gone: a skipped close must be parked on the audio manager (KB-028)"
        );
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
        assert_ne!(
            lane_a, lane_b,
            "lane ids are per registration, never per name"
        );
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

    /// KB-261 (fleet pass 1): identities are exact, so no phone's data can
    /// ever fold into another's. The v1.6.4 platform-name adoption is
    /// pinned REMOVED here: a fresh-id same-platform hello while the prior
    /// is away (the old adoption trigger - and any pairing-token holder
    /// hits it, which shipped the prior's transcript history to the
    /// newcomer and attributed the newcomer's dictations to the prior),
    /// bare no-id hellos never folding into known tagged identities (and
    /// tagged hellos never folding into a known bare one), and no
    /// poisoning (an ordinary re-pair can no longer disable anything,
    /// because there is no cross-identity rule left to disable).
    #[test]
    fn kb261_identities_are_exact_and_never_adopt() {
        let manager = CompanionManager::new();
        // The persisted known set this server run was seeded with (start()
        // loads it into the manager).
        *manager
            .known_identities
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = vec!["iPhone#aaaa".to_string()];

        // A fresh-id same-platform hello while the prior identity is AWAY:
        // the v1.6.4 cut adopted "iPhone#aaaa" here, delivering that
        // phone's Recent list and transcript history to whichever phone
        // helloed. The identity must be its own; history keyed on
        // "iPhone#aaaa" can never ship to this connection's socket,
        // because the recent-list filter keys on the registration's own
        // identity (send_recent_history_to_lane ->
        // recent_companion_entries_for_device).
        let (newcomer, _rx) =
            manager.register_connection_with_identity("iPhone", Some("zzzz9999-0000"));
        assert_eq!(
            newcomer.device, "iPhone#zzzz",
            "a fresh-id hello must never fold into a known same-platform identity"
        );
        assert_eq!(
            newcomer.persist_identities,
            Some(vec!["iPhone#aaaa".to_string(), "iPhone#zzzz".to_string()]),
            "the newcomer's own identity is owed to persistence, bounded, most recent last"
        );

        // The ordinary re-pair the old rule broke: the prior identity's
        // connection is STILL registered (its socket lingers up to the
        // keepalive deadline after a network switch) - the newcomer still
        // gets its own identity, and nothing is poisoned for later hellos
        // (the next fresh-id hello resolves exactly the same way).
        let (repair, _rx) =
            manager.register_connection_with_identity("iPhone", Some("cccc3333-0000"));
        assert_eq!(repair.device, "iPhone#cccc");

        // The same identity re-pairing (same origin, same stored id, or a
        // Mac restart): nothing changes, nothing owed.
        let (again, _rx) =
            manager.register_connection_with_identity("iPhone", Some("aaaa1111-0000"));
        assert_eq!(again.device, "iPhone#aaaa");
        assert!(again.persist_identities.is_none());

        // A no-id hello keeps the bare name, the pinned protocol invariant:
        // it never adopts a known tagged identity (two phones sharing one
        // Recent list, the KB-265 defect class), in either direction.
        let (bare, _rx) = manager.register_connection_with_identity("iPhone", None);
        assert_eq!(
            bare.device, "iPhone",
            "a client with no id keeps the bare name (protocol.rs invariant)"
        );
        let manager_bare = CompanionManager::new();
        *manager_bare
            .known_identities
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = vec!["iPhone".to_string()];
        let (tagged, _rx) =
            manager_bare.register_connection_with_identity("iPhone", Some("bbbb2222-0000"));
        assert_eq!(
            tagged.device, "iPhone#bbbb",
            "a tagged hello must never fold into a known bare identity"
        );

        // A different platform never touches the iPhone's resolution.
        let (ipad, _rx) = manager.register_connection_with_identity("iPad", Some("dddd4444-0000"));
        assert_eq!(ipad.device, "iPad#dddd");

        // The known set stays bounded: the manager keeps the most recent
        // KNOWN_IDENTITIES_LIMIT identities (pinned here so a change is a
        // deliberate decision, not drift).
        assert_eq!(KNOWN_IDENTITIES_LIMIT, 8);
    }

    /// KB-261 (v1.6.4 review pass, re-anchored by fleet pass 1): the twin
    /// race. The v1.6.4 adoption cut resolved the identity BEFORE
    /// registering, leaving a window where a concurrent same-platform
    /// hello adopted a known identity and collapsed two phones into one
    /// Recent list (the KB-265 defect class). Identities are now exact and
    /// the registration insert and known-set update share one critical
    /// section, so N same-platform phones registering CONCURRENTLY all end
    /// up with DISTINCT identities.
    #[test]
    fn kb261_concurrent_same_platform_registrations_never_share_an_identity() {
        let manager = Arc::new(CompanionManager::new());
        const PHONES: usize = 8;
        let barrier = Arc::new(std::sync::Barrier::new(PHONES));
        let registrations = Arc::new(Mutex::new(Vec::<(String, u64, u64)>::new()));

        let mut handles = Vec::new();
        for i in 0..PHONES {
            let manager = Arc::clone(&manager);
            let barrier = Arc::clone(&barrier);
            let sink = Arc::clone(&registrations);
            handles.push(std::thread::spawn(move || {
                // Every phone is the same platform with a fresh id (a
                // reconnect storm after a Mac origin change).
                let device_id = format!("{:04x}-fresh", 0x1000 + i);
                barrier.wait();
                let (registration, _rx) =
                    manager.register_connection_with_identity("iPhone", Some(&device_id));
                sink.lock().unwrap_or_else(|e| e.into_inner()).push((
                    registration.device,
                    registration.lane,
                    registration.generation,
                ));
            }));
        }
        for handle in handles {
            handle
                .join()
                .expect("no registration panics under contention");
        }

        let registrations = registrations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert_eq!(registrations.len(), PHONES);
        let mut identities: Vec<String> = registrations.iter().map(|(d, _, _)| d.clone()).collect();
        identities.sort();
        let unique = identities.len();
        identities.dedup();
        assert_eq!(
            identities.len(),
            unique,
            "every concurrent same-platform phone must own a DISTINCT identity (got {identities:?})"
        );
        // And the in-memory known set recorded every one of them, bounded
        // by the limit.
        let known = manager.known_identities_snapshot();
        assert_eq!(
            known.len(),
            PHONES.min(KNOWN_IDENTITIES_LIMIT),
            "the known set grew by every minted identity, bounded"
        );
    }

    /// KB-261 source pin (the settings write needs the real store, which a
    /// mock app cannot mount): persistence must (1) be owned by the
    /// CALLER off the connection task (the server spawns the worker; that
    /// half is pinned in server.rs), (2) read the FRESH in-memory set at
    /// run time so out-of-order workers converge, (3) merge into the
    /// current settings rather than writing a stale whole-settings copy,
    /// and (4) be seeded from the persisted store at server start so the
    /// registration path performs no settings IO.
    #[test]
    fn kb261_new_identities_persist_bounded_through_the_settings_store() {
        let source = include_str!("mod.rs");
        let persist_at = source
            .find("pub(crate) fn persist_known_identities(")
            .expect("the persistence helper");
        let persist_end = persist_at + source[persist_at..].find("\n}\n").expect("fn end");
        let persist = &source[persist_at..persist_end];
        assert!(
            persist.contains("manager.known_identities_snapshot()"),
            "persistence reads the FRESH in-memory set at run time, never a stale payload"
        );
        assert!(
            persist.contains("if settings.companion_known_identities != identities"),
            "persistence writes only when the field changed"
        );
        assert!(
            persist.contains("settings.companion_known_identities = identities"),
            "persistence merges into the CURRENT settings (this field only)"
        );

        // The registration mints bounded sets and owes them to the caller.
        let register_at = source
            .find("pub fn register_connection_with_identity(")
            .expect("the registration");
        let register_end = register_at + source[register_at..].find("\n    }\n").expect("fn end");
        let register = &source[register_at..register_end];
        assert!(
            register.contains("KNOWN_IDENTITIES_LIMIT")
                && register.contains("persist_identities = Some("),
            "a minted identity returns the bounded set for the caller to persist"
        );
        assert!(
            !register.contains("get_settings") && !register.contains("write_settings"),
            "the registration itself performs no settings IO"
        );

        // Server start seeds the in-memory cache from the store.
        let start_at = source
            .find("pub fn start(&self, app: &AppHandle)")
            .expect("start");
        let start_end = start_at + source[start_at..].find("\n    }\n").expect("fn end");
        assert!(
            source[start_at..start_end].contains("settings.companion_known_identities.clone()"),
            "start() seeds the known-identity cache from the persisted store"
        );

        // Fleet pass 1: platform-name adoption is gone entirely - no code
        // path may match identities across phones again (the manager-level
        // pin above holds the behavior; this keeps the mechanism itself
        // from creeping back in). The needles are concatenated so this
        // assertion's own literals cannot satisfy it.
        let adoption_fn = ["sticky", "_identity", "_adoption"].concat();
        let base_fn = ["identity", "_ba", "se("].concat();
        assert!(
            !source.contains(&adoption_fn) && !source.contains(&base_fn),
            "platform-name adoption and its base splitter must not reappear"
        );
    }

    /// KB-261 (fleet pass 1) source pin: the two connect-edge settings
    /// read-modify-write cycles SERIALIZE. The first-pairing hello spawns
    /// persist_known_identities and note_device_connected as two detached
    /// workers, and each does get_settings -> mutate one field ->
    /// write_settings over the WHOLE settings object; unsynchronized, the
    /// interleave [persist reads][connected reads][persist writes]
    /// [connected writes] let the last whole-settings writer erase the
    /// other's field (the real store cannot mount under a mock app, so the
    /// lock discipline is pinned on the source, the same convention as the
    /// test above).
    #[test]
    fn kb261_connect_edge_settings_rmws_serialize_through_one_lock() {
        let source = include_str!("mod.rs");

        // The shared lock and the helper that holds it across the whole
        // get -> mutate -> write span.
        let helper_at = source
            .find("fn connect_edge_settings_rmw(")
            .expect("the connect-edge RMW helper");
        let helper_end = helper_at + source[helper_at..].find("\n}\n").expect("fn end");
        let helper = &source[helper_at..helper_end];
        let guard_at = helper.find("CONNECT_EDGE_SETTINGS_RMW").expect("the guard");
        let read_at = helper.find("get_settings").expect("the read");
        let write_at = helper.find("write_settings").expect("the write");
        assert!(
            guard_at < read_at && read_at < write_at,
            "the RMW helper must take the lock BEFORE reading settings and keep it past the write"
        );

        // Both connect-edge cycles route through the helper (a whole-
        // settings write outside it would reintroduce the lost update).
        // The method (impl indent) and the free fn (module indent) close
        // differently, so each carries its own terminator.
        for (site, terminator) in [
            ("pub(crate) fn persist_known_identities(", "\n}\n"),
            ("pub fn note_device_connected(", "\n    }\n"),
        ] {
            let at = source
                .find(site)
                .unwrap_or_else(|| panic!("the connect-edge settings writer must exist: {site}"));
            let end = at + source[at..].find(terminator).expect("fn end");
            let body = &source[at..end];
            assert!(
                body.contains("connect_edge_settings_rmw(app"),
                "{site} must run its whole-settings cycle under the connect-edge lock"
            );
            assert!(
                !body.contains("write_settings(app"),
                "{site} must not write settings outside the locked helper"
            );
        }
    }

    /// KB-289: the three whole-settings writers that lived OUTSIDE the
    /// serialized RMW lock - companion start()'s pairing-token rebind,
    /// reset_pairing, and the set_log_level command - now ride the same
    /// cycle as the connect-edge workers (a bare get/mutate/write outside
    /// the lock could interleave a lost update with any locked writer).
    /// Source pin in the KB-261 style: start() needs a real listener and
    /// set_log_level needs the log plugin state a unit test cannot mount.
    #[test]
    fn kb289_whole_settings_writers_ride_the_serialized_rmw_lock() {
        let source = include_str!("mod.rs");

        // start()'s pairing-token rebind: the RMW call replaced the inline
        // get/token_stale/write block.
        let start_at = source
            .find("pub fn start(&self, app: &AppHandle) -> Result<(), String>")
            .expect("companion start");
        let start_body = &source[start_at..start_at + 4000];
        assert!(
            start_body.contains("connect_edge_settings_rmw(app, &|settings| {"),
            "start()'s pairing-token rebind must ride the serialized RMW"
        );
        let rebind_at = start_body
            .find("connect_edge_settings_rmw(app, &|settings| {")
            .expect("the rebind");
        let rebind_window = &start_body[rebind_at..rebind_at + 900];
        assert!(
            rebind_window.contains("token_stale"),
            "the rebind closure must keep computing token staleness"
        );
        assert!(
            rebind_window.contains("generate_pairing_token"),
            "the rebind closure must keep minting the fresh token"
        );

        // reset_pairing: the fresh token persists through the helper, and
        // no bare whole-settings write remains in the body.
        let reset_at = source
            .find("pub fn reset_pairing(&self, app: &AppHandle) -> Result<(), String>")
            .expect("reset_pairing");
        let reset_body = &source[reset_at..reset_at + 1400];
        assert!(
            reset_body.contains("connect_edge_settings_rmw(app, &|settings| {"),
            "reset_pairing must ride the serialized RMW"
        );
        assert!(
            !reset_body.contains("crate::settings::write_settings(app, settings);"),
            "reset_pairing must not bare-write the whole store outside the lock"
        );

        // The set_log_level command (src/commands/mod.rs) rides the same
        // helper through the crate path.
        let commands = include_str!("../commands/mod.rs");
        let log_at = commands
            .find("pub fn set_log_level(app: AppHandle, level: LogLevel)")
            .expect("set_log_level");
        let log_body = &commands[log_at..log_at + 1700];
        assert!(
            log_body.contains("crate::companion::connect_edge_settings_rmw(&app, &|settings| {"),
            "set_log_level must ride the serialized RMW"
        );
        assert!(
            !log_body.contains("write_settings(&app, settings);"),
            "set_log_level must not bare-write the whole store outside the lock"
        );
    }

    /// KB-289 residual (fleet, cycle 14), doc pin: the helper's docs must
    /// state the lock's ACTUAL guarantee. The first cut of the widened
    /// docs claimed "the lock is the app's serialized RMW for the whole
    /// settings store" and "a concurrent whole-settings writer can no
    /// longer interleave a lost update" - contradicted by the code: only
    /// the riders serialize, while the app's other whole-settings writers
    /// (the shortcut rebinding's change_binding among them) still do bare
    /// get/mutate/write outside any lock, so a rider CAN still lose an
    /// update against one of those. A doc that overclaims a store-wide
    /// guarantee invites relying on one that does not exist.
    #[test]
    fn kb289_the_rmw_doc_states_the_scoped_guarantee() {
        let source = include_str!("mod.rs");
        let helper_at = source
            .find("pub(crate) fn connect_edge_settings_rmw(")
            .expect("the RMW helper");
        // The doc block sits directly above the fn.
        let doc = &source[helper_at - 2600..helper_at];
        assert!(
            doc.contains("serialized RMW for every writer of")
                && doc.contains("NOT store-wide"),
            "the helper doc must scope the guarantee to the riders (companion pairing/identity fields and log_level) and state it is not store-wide"
        );
        assert!(
            !doc.contains("for the whole settings store, not only the connect edge"),
            "the store-wide overclaim must not survive in the helper doc"
        );
        assert!(
            !doc.contains("concurrent whole-settings writers serialize"),
            "the doc must not claim ALL concurrent whole-settings writers serialize; only the riders do"
        );

        // start()'s rebind comment carries the same scoped wording.
        let start_at = source
            .find("pub fn start(&self, app: &AppHandle) -> Result<(), String>")
            .expect("companion start");
        let start_body = &source[start_at..start_at + 9000];
        let rebind_comment = &start_body[start_body
            .find("KB-289: the rebind's whole-settings write")
            .expect("the rebind comment")..];
        assert!(
            !rebind_comment[..900]
                .contains("a concurrent whole-settings writer can no longer interleave"),
            "the rebind comment must not claim immunity against non-riding writers"
        );
        assert!(
            rebind_comment[..900].contains("no other RIDER of that"),
            "the rebind comment must scope the immunity to the lock's riders"
        );
    }

    /// KB-270 (v1.6.4, fleet pass 2): the per-lane session records stop
    /// growing because the pipeline's handoff CONSUMES its record
    /// (note_stopping_session removes it), so a phone flapping through
    /// reconnect-and-dictate cycles leaves the map EMPTY - in the ordinary
    /// stop order AND the cancel order (false edge first), and even when a
    /// cycle's false edge lands with no pipeline at all before the next
    /// Start re-commits the lane (the next begin_session replaces the
    /// drained record in place).
    #[test]
    fn kb270_lane_session_records_are_consumed_at_every_session_end() {
        let app = tauri::test::mock_app();
        let manager = Arc::new(CompanionManager::new());
        app.manage(Arc::clone(&manager));

        // A flapping phone: N reconnect cycles, one dictation each, in the
        // production order (handoff, take, then the false edge).
        for cycle in 0..25u64 {
            let (lane, _gen, _rx) = manager.register_connection("Pixel 8");
            begin_session(app.handle(), lane, protocol::SessionPrefs::default());
            assert!(
                manager.lane_owns_live_session(lane),
                "cycle {cycle}: the session is live while recording"
            );
            note_stopping_session(app.handle(), lane);
            assert!(
                take_stopping_session(app.handle()).is_some(),
                "cycle {cycle}: the identity resolves before the end"
            );
            on_lane_session_changed(app.handle(), lane, false);
            assert!(
                manager.lane_sessions.lock().unwrap().is_empty(),
                "cycle {cycle}: a consumed session leaves no record behind"
            );
        }

        // The cancel order on the same flapping shape: false edge first
        // (the coordinator's cancel arm), pipeline handoff after. The map
        // is empty after the handoff too - and the identity it handed over
        // was the drained record's OWN (the wrong-phone guard the fleet
        // pass pinned above in
        // server_stop_leaves_records_for_their_pipelines_in_both_stop_orders).
        for cycle in 0..25u64 {
            let (lane, _gen, _rx) = manager.register_connection("Pixel 8");
            begin_session(app.handle(), lane, protocol::SessionPrefs::default());
            on_lane_session_changed(app.handle(), lane, false);
            note_stopping_session(app.handle(), lane);
            assert!(
                take_stopping_session(app.handle()).is_some(),
                "cycle {cycle}: the cancel order still resolves the identity"
            );
            assert!(
                manager.lane_sessions.lock().unwrap().is_empty(),
                "cycle {cycle}: the cancel order leaves no record either"
            );
        }

        // Two live sessions coexist; draining one leaves the other's
        // record pending, and each consumption removes exactly its own.
        let (lane_a, _ga, _ra) = manager.register_connection("iPhone#7f3a");
        let (lane_b, _gb, _rb) = manager.register_connection("iPhone#c3d4");
        begin_session(app.handle(), lane_a, protocol::SessionPrefs::default());
        begin_session(app.handle(), lane_b, protocol::SessionPrefs::default());
        on_lane_session_changed(app.handle(), lane_a, false);
        assert!(manager.lane_owns_live_session(lane_b), "B survives A's end");
        note_stopping_session(app.handle(), lane_a);
        assert!(manager.lane_owns_live_session(lane_b), "B still live");
        note_stopping_session(app.handle(), lane_b);
        assert!(manager.lane_sessions.lock().unwrap().is_empty());
    }

    /// v1.6.4 accept-wedge hardening: a panic in a task holding one of the
    /// manager's std Mutexes must not poison the companion surface. The
    /// pre-fix `.lock().unwrap()` acquisitions panicked on a poisoned lock,
    /// so one poisoned map wedged every later connection task until
    /// restart. Every hot-path acquisition is now poison-tolerant; this
    /// test POISONS the registries deliberately and asserts the manager
    /// keeps serving registrations, device lists, and session records.
    #[test]
    fn poisoned_manager_locks_never_wedge_later_connections() {
        let manager = CompanionManager::new();

        // Poison the connections registry, the session map, and the
        // identity set: panic in a task while holding each guard (caught,
        // like tokio catches a task panic).
        for poison in [
            "connections",
            "lane_sessions",
            "pending_prefs",
            "stopping",
            "known_identities",
        ] {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match poison {
                "connections" => {
                    let _guard = manager
                        .connections
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    panic!("poison {poison}");
                }
                "lane_sessions" => {
                    let _guard = manager
                        .lane_sessions
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    panic!("poison {poison}");
                }
                "pending_prefs" => {
                    let _guard = manager
                        .pending_prefs
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    panic!("poison {poison}");
                }
                "known_identities" => {
                    let _guard = manager
                        .known_identities
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    panic!("poison {poison}");
                }
                _ => {
                    let _guard = manager.stopping.lock().unwrap_or_else(|e| e.into_inner());
                    panic!("poison {poison}");
                }
            }));
            assert!(
                result.is_err(),
                "the {poison} poisoning panic must be caught"
            );
        }

        // The very next connection still registers (with its identity
        // resolution), unregisters, and reads every surface the
        // connection path touches - no unwrap panic, no wedge.
        let (registration, _rx) =
            manager.register_connection_with_identity("iPhone", Some("aaaa1111-0000"));
        let (lane, generation) = (registration.lane, registration.generation);
        assert_eq!(registration.device, "iPhone#aaaa");
        assert_eq!(manager.devices(), vec!["iPhone#aaaa".to_string()]);
        assert_eq!(manager.paired_device_count(), 1);
        manager.set_pending_session_prefs(protocol::SessionPrefs::default());
        assert_eq!(
            manager.pending_session_prefs(),
            protocol::SessionPrefs::default()
        );
        assert!(!manager.lane_owns_live_session(lane));
        manager.unregister_connection(lane, generation);
        assert!(manager.devices().is_empty());
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
                // KB-015/KB-122: the memory-gate refusal is the same
                // forward-with-the-pipeline class as the generic load
                // failure (a phone dictation can trigger it through the
                // shared load path).
                ModelLoadFailedMemoryGate,
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

        // The session ends. Production order: the coordinator's Stop
        // effect hands the identity over (note_stopping_session), the
        // pipeline's first synchronous step takes it, and only THEN does
        // the false edge land (KB-270: which now also sweeps the record).
        note_stopping_session(app.handle(), lane_a);
        let (stop_lane, _device, prefs) =
            take_stopping_session(app.handle()).expect("A's stopping session resolves");
        assert_eq!(stop_lane, lane_a);
        assert_eq!(prefs, phone_target);
        on_lane_session_changed(app.handle(), lane_a, false);
        assert!(
            manager.lane_sessions.lock().unwrap().get(&lane_a).is_none(),
            "the session END sweeps the drained record (KB-270)"
        );

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
    /// suppressed outright when there is no address or no token. The
    /// candidates carry the KB-259 corroboration: the Tailscale address
    /// rides a utun, the LAN addresses ride physical interfaces.
    #[test]
    fn pairing_url_prefers_the_advertised_host_override() {
        let lan = Ipv4Addr::new(192, 168, 1, 42);
        let lan_subnet = Some("192.168.1.0/24");
        let lan_candidates = [ip::InterfaceCandidate {
            ip: lan,
            name: "en0".to_string(),
            kind: ip::InterfaceKind::Lan,
        }];
        let ts = Ipv4Addr::new(100, 101, 7, 24);
        let ts_candidates = [ip::InterfaceCandidate {
            ip: ts,
            name: "utun9".to_string(),
            kind: ip::InterfaceKind::Tailscale,
        }];

        // Override wins over the interface IP.
        let (url, qr) = advertised_url_qr(
            Some("mac.tail1234.ts.net"),
            Some(lan),
            Some("tok"),
            lan_subnet,
            4177,
            &lan_candidates,
        );
        assert_eq!(
            url.as_deref(),
            Some("https://mac.tail1234.ts.net:4177/#t=tok")
        );
        assert!(qr.is_some(), "a valid URL always carries a QR");

        // No override: the effective IP, on the configured port.
        let (url, _) = advertised_url_qr(
            None,
            Some(lan),
            Some("tok"),
            lan_subnet,
            8443,
            &lan_candidates,
        );
        assert_eq!(url.as_deref(), Some("https://192.168.1.42:8443/#t=tok"));

        // Tailscale effective address with a token bound for it (the /10).
        let (url, _) = advertised_url_qr(
            None,
            Some(ts),
            Some("tok"),
            Some("100.64.0.0/10"),
            4177,
            &ts_candidates,
        );
        assert_eq!(url.as_deref(), Some("https://100.101.7.24:4177/#t=tok"));

        // No effective address (stopped, unresolvable choice) or no
        // token: nothing to advertise.
        assert_eq!(
            advertised_url_qr(None, None, Some("tok"), lan_subnet, 4177, &lan_candidates),
            (None, None)
        );
        assert_eq!(
            advertised_url_qr(None, Some(lan), None, lan_subnet, 4177, &lan_candidates),
            (None, None)
        );
        assert_eq!(
            advertised_url_qr(None, Some(lan), Some(""), lan_subnet, 4177, &lan_candidates),
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
        let lan_candidates = [ip::InterfaceCandidate {
            ip: lan,
            name: "en0".to_string(),
            kind: ip::InterfaceKind::Lan,
        }];
        let ts_candidates = [ip::InterfaceCandidate {
            ip: tailscale,
            name: "utun9".to_string(),
            kind: ip::InterfaceKind::Tailscale,
        }];

        // LAN token, Tailscale choice pending: no URL, no QR.
        assert_eq!(
            advertised_url_qr(
                None,
                Some(tailscale),
                Some("tok"),
                Some("192.168.1.0/24"),
                4177,
                &ts_candidates
            ),
            (None, None),
            "a /24 token advertised on a /10 address pairs against a dead token after restart"
        );

        // The reverse switch is equally suppressed.
        assert_eq!(
            advertised_url_qr(
                None,
                Some(lan),
                Some("tok"),
                Some("100.64.0.0/10"),
                4177,
                &lan_candidates
            ),
            (None, None)
        );

        // A same-/24 interface change (en0 to a bridge on the same LAN)
        // keeps the token valid, so the preview stays honest there.
        let bridge = Ipv4Addr::new(192, 168, 1, 50);
        let bridge_candidates = [ip::InterfaceCandidate {
            ip: bridge,
            name: "bridge100".to_string(),
            kind: ip::InterfaceKind::Lan,
        }];
        let (url, _) = advertised_url_qr(
            None,
            Some(bridge),
            Some("tok"),
            Some("192.168.1.0/24"),
            4177,
            &bridge_candidates,
        );
        assert_eq!(url.as_deref(), Some("https://192.168.1.50:4177/#t=tok"));

        // Never-paired state (no stored subnet): nothing yet.
        assert_eq!(
            advertised_url_qr(None, Some(lan), Some("tok"), None, 4177, &lan_candidates),
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
