use crate::audio_toolkit::{
    list_input_devices,
    vad::{
        frames_for_duration_ms, EarshotVad, SmoothedVad, VAD_OFFLINE_HANGOVER_MS, VAD_ONSET_MS,
        VAD_PREFILL_MS, VAD_STREAMING_HANGOVER_MS,
    },
    AudioRecorder, RemoteAudioSource, SileroVad, VadPolicy, VoiceActivityDetector,
};
use crate::helpers::clamshell;
use crate::managers::transcription::StreamRouter;
use crate::settings::{get_settings, write_settings, AppSettings, VadBackend};
use crate::utils;
use log::{debug, error, info, trace, warn};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{Emitter, Manager};

const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const SILERO_VAD_THRESHOLD: f32 = 0.3;
const EARSHOT_VAD_THRESHOLD: f32 = 0.5;

fn set_mute(mute: bool) {
    // Unit tests record the operation instead of shelling out: the real
    // body mutates the system volume, which a test must never do.
    #[cfg(test)]
    {
        mute_test_log::record(if mute { "mute" } else { "unmute" });
    }

    #[cfg(not(test))]
    {
        set_mute_platform(mute)
    }
}

/// Test-only log of forced-mute operations, so the mute lifecycle (apply on
/// readiness, restore on stop AND cancel) can be asserted without touching
/// the real system volume.
#[cfg(test)]
mod mute_test_log {
    use std::sync::Mutex;

    static OPS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

    pub fn record(op: &'static str) {
        OPS.lock().unwrap_or_else(|e| e.into_inner()).push(op);
    }

    pub fn take() -> Vec<&'static str> {
        std::mem::take(&mut *OPS.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

#[cfg(not(test))]
fn set_mute_platform(mute: bool) {
    // Expected behavior:
    // - Windows: works on most systems using standard audio drivers.
    // - Linux: works on many systems (PipeWire, PulseAudio, ALSA),
    //   but some distros may lack the tools used.
    // - macOS: works on most standard setups via AppleScript.
    // If unsupported, fails silently.

    #[cfg(target_os = "windows")]
    {
        unsafe {
            use windows::Win32::{
                Media::Audio::{
                    eMultimedia, eRender, Endpoints::IAudioEndpointVolume, IMMDeviceEnumerator,
                    MMDeviceEnumerator,
                },
                System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED},
            };

            macro_rules! unwrap_or_return {
                ($expr:expr) => {
                    match $expr {
                        Ok(val) => val,
                        Err(_) => return,
                    }
                };
            }

            // Initialize the COM library for this thread.
            // If already initialized (e.g., by another library like Tauri), this does nothing.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

            let all_devices: IMMDeviceEnumerator =
                unwrap_or_return!(CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL));
            let default_device =
                unwrap_or_return!(all_devices.GetDefaultAudioEndpoint(eRender, eMultimedia));
            let volume_interface = unwrap_or_return!(
                default_device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
            );

            let _ = volume_interface.SetMute(mute, std::ptr::null());
        }
    }

    #[cfg(target_os = "linux")]
    {
        use std::process::Command;

        let mute_val = if mute { "1" } else { "0" };
        let amixer_state = if mute { "mute" } else { "unmute" };

        // Try multiple backends to increase compatibility
        // 1. PipeWire (wpctl)
        if Command::new("wpctl")
            .args(["set-mute", "@DEFAULT_AUDIO_SINK@", mute_val])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return;
        }

        // 2. PulseAudio (pactl)
        if Command::new("pactl")
            .args(["set-sink-mute", "@DEFAULT_SINK@", mute_val])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return;
        }

        // 3. ALSA (amixer)
        let _ = Command::new("amixer")
            .args(["set", "Master", amixer_state])
            .output();
    }

    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        let script = format!(
            "set volume output muted {}",
            if mute { "true" } else { "false" }
        );
        let _ = Command::new("osascript").args(["-e", &script]).output();
    }
}

/// Reads the current system output mute state, mirroring `set_mute`'s backends.
///
/// Returns `Some(true)`/`Some(false)` when the state could be determined, or
/// `None` when it couldn't (unsupported platform, missing CLI tools, or an
/// error). Callers treat `None` as "unknown" and fall back to unmuting on stop,
/// so we never strand the user's audio muted.
#[cfg(target_os = "windows")]
fn get_mute() -> Option<bool> {
    unsafe {
        use windows::Win32::{
            Media::Audio::{
                eMultimedia, eRender, Endpoints::IAudioEndpointVolume, IMMDeviceEnumerator,
                MMDeviceEnumerator,
            },
            System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED},
        };

        // Matches set_mute: no-op if COM is already initialized on this thread.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        let all_devices: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let default_device = all_devices
            .GetDefaultAudioEndpoint(eRender, eMultimedia)
            .ok()?;
        let volume_interface = default_device
            .Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
            .ok()?;

        Some(volume_interface.GetMute().ok()?.as_bool())
    }
}

#[cfg(target_os = "linux")]
fn get_mute() -> Option<bool> {
    use std::process::Command;

    // 1. PipeWire (wpctl): prints "[MUTED]" in the volume line when muted.
    if let Ok(out) = Command::new("wpctl")
        .args(["get-volume", "@DEFAULT_AUDIO_SINK@"])
        .output()
    {
        if out.status.success() {
            return Some(String::from_utf8_lossy(&out.stdout).contains("[MUTED]"));
        }
    }

    // 2. PulseAudio (pactl): prints "Mute: yes" / "Mute: no".
    // Force LC_ALL=C so a localized system still emits the parseable English
    // "yes"/"no" instead of e.g. "ja"/"nein".
    if let Ok(out) = Command::new("pactl")
        .env("LC_ALL", "C")
        .args(["get-sink-mute", "@DEFAULT_SINK@"])
        .output()
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).to_lowercase();
            if s.contains("yes") {
                return Some(true);
            }
            if s.contains("no") {
                return Some(false);
            }
        }
    }

    // 3. ALSA (amixer): prints "[off]" for muted channels, "[on]" otherwise.
    // LC_ALL=C keeps the "[on]"/"[off]" tokens stable across locales.
    if let Ok(out) = Command::new("amixer")
        .env("LC_ALL", "C")
        .args(["get", "Master"])
        .output()
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            if s.contains("[off]") {
                return Some(true);
            }
            if s.contains("[on]") {
                return Some(false);
            }
        }
    }

    None
}

#[cfg(target_os = "macos")]
fn get_mute() -> Option<bool> {
    use std::process::Command;

    let out = Command::new("osascript")
        .args(["-e", "output muted of (get volume settings)"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    match String::from_utf8_lossy(&out.stdout).trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
fn get_mute() -> Option<bool> {
    None
}

/// Restores the system mute state after our forced mute, given the state
/// captured just before we muted. We only ever need to unmute - and only when
/// the system was NOT already muted beforehand. If the prior state was muted,
/// we leave it muted (the user's own state). If it's unknown (`None`), we
/// default to unmuting so audio is never left stranded muted by us.
fn restore_mute(prev_muted: Option<bool>) {
    if prev_muted != Some(true) {
        set_mute(false);
    }
}

const WHISPER_SAMPLE_RATE: usize = 16000;

/* ──────────────────────────────────────────────────────────────── */

#[derive(Clone, Debug)]
pub enum RecordingState {
    Idle,
    Recording { binding_id: String },
    Stopping,
}

/// Whether a capture-restarting settings change must be rejected right
/// now: any live recording (Recording or Stopping) would have its captured
/// samples discarded by the restart, silently killing the dictation. The
/// shared rule behind the device/channel/VAD switch guards.
fn capture_restart_forbidden(state: &RecordingState) -> bool {
    !matches!(state, RecordingState::Idle)
}

#[derive(Clone, Debug)]
pub enum MicrophoneMode {
    AlwaysOn,
    OnDemand,
}

/// Where a recording session's audio comes from. `Local` is the cpal
/// microphone path exactly as it has always run; `Remote` is a companion
/// device (phone/tablet on the LAN) pushing 16 kHz mono chunks through a
/// `RemoteAudioSource`. Both sources share the single-session state machine
/// and the recorder built by `create_audio_recorder` (same VAD, level, and
/// StreamRouter callbacks), so everything downstream of the ring is
/// byte-identical.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureSource {
    Local,
    Remote,
}

/// Tracks our forced "mute while recording" so we can restore the user's audio
/// exactly as it was. `did_mute` is true while our mute is active; `prev_muted`
/// is the system mute state captured just before we muted, used to decide
/// whether to unmute on stop (so a system that was already muted stays muted).
#[derive(Debug, Default, Clone, Copy)]
struct MuteState {
    did_mute: bool,
    prev_muted: Option<bool>,
}

/// The restore action for a forced mute: the snapshotted prior state to
/// hand to `restore_mute`, taken exactly once while our mute is active.
/// Shared decision for every teardown path (normal stop, stream close,
/// cancellation, local or companion source) so none of them can strand
/// the system muted.
fn mute_restore_action(state: &MuteState) -> Option<Option<bool>> {
    state.did_mute.then_some(state.prev_muted)
}

/// The persisted microphone preference currently in effect. Clamshell and
/// regular selections are kept distinct so losing a clamshell-only device does
/// not erase the user's normal microphone preference.
enum DesiredMicrophone {
    Default,
    Selected(String),
    Clamshell(String),
}

/// Result of resolving the persisted preference to a live cpal device.
/// `device: None` means cpal should open the system default. The unavailable
/// name is populated only when enumeration succeeded and confirmed that the
/// user's regular selected microphone is missing.
struct MicrophoneResolution {
    device: Option<cpal::Device>,
    unavailable_selected_microphone: Option<String>,
}

/* ──────────────────────────────────────────────────────────────── */

fn create_audio_recorder(
    backend: VadBackend,
    app_handle: &tauri::AppHandle,
    selected_channel: Option<u16>,
    stream_router: Arc<StreamRouter>,
) -> Result<AudioRecorder, anyhow::Error> {
    let detector: Box<dyn VoiceActivityDetector> = match backend {
        VadBackend::Silero => {
            let vad_path = app_handle
                .path()
                .resolve(
                    "resources/models/silero_vad_v4.onnx",
                    tauri::path::BaseDirectory::Resource,
                )
                .map_err(|e| anyhow::anyhow!("Failed to resolve VAD path: {e}"))?;
            Box::new(
                SileroVad::new(vad_path, SILERO_VAD_THRESHOLD)
                    .map_err(|e| anyhow::anyhow!("Failed to create SileroVad: {e}"))?,
            )
        }
        VadBackend::Earshot => Box::new(
            EarshotVad::new(EARSHOT_VAD_THRESHOLD)
                .map_err(|e| anyhow::anyhow!("Failed to create EarshotVad: {e}"))?,
        ),
    };

    // Earshot uses 16 ms frames while Silero uses 30 ms. Convert the existing
    // time-based capture profile to each detector's frame size so selecting a
    // backend does not shorten pre-roll, onset, or post-speech audio.
    let frame_samples = detector.frame_samples();
    let prefill_frames = frames_for_duration_ms(VAD_PREFILL_MS, frame_samples);
    let offline_hangover_frames = frames_for_duration_ms(VAD_OFFLINE_HANGOVER_MS, frame_samples);
    let streaming_hangover_frames =
        frames_for_duration_ms(VAD_STREAMING_HANGOVER_MS, frame_samples);
    let onset_frames = frames_for_duration_ms(VAD_ONSET_MS, frame_samples);
    let smoothed_vad = SmoothedVad::new(
        detector,
        prefill_frames,
        offline_hangover_frames,
        onset_frames,
    );

    info!(
        "Initialized {:?} VAD backend ({} samples/frame)",
        backend, frame_samples
    );

    // Recorder with VAD, a spectrum-level callback that forwards level updates to
    // the frontend, and an audio-frame callback that feeds live streaming via a
    // shared `StreamRouter` (captured directly, not via Tauri state - see its docs).
    let recorder = AudioRecorder::new()
        .map_err(|e| anyhow::anyhow!("Failed to create AudioRecorder: {}", e))?
        .with_vad(
            Box::new(smoothed_vad),
            offline_hangover_frames,
            streaming_hangover_frames,
        )
        .with_selected_channel(selected_channel)
        .with_level_callback({
            let app_handle = app_handle.clone();
            move |levels| {
                utils::emit_levels(&app_handle, &levels);
            }
        })
        .with_audio_callback({
            let router = stream_router;
            move |frame| {
                router.feed(frame);
            }
        });

    Ok(recorder)
}

/* ──────────────────────────────────────────────────────────────── */

/// One recording session's first-sample notification. Waiting on this never
/// blocks the shortcut coordinator: callers hand it to a dedicated worker.
pub struct RecordingReadiness {
    receiver: mpsc::Receiver<()>,
    generation: u64,
}

impl RecordingReadiness {
    pub fn wait(self) -> bool {
        self.receiver.recv().is_ok()
    }

    /// Bounded variant of [`RecordingReadiness::wait`] for tests: resolves
    /// after `timeout` instead of blocking forever on a stalled capture.
    #[cfg(test)]
    pub fn wait_timeout(self, timeout: std::time::Duration) -> bool {
        self.receiver.recv_timeout(timeout).is_ok()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }
}

#[derive(Clone)]
pub struct AudioRecordingManager {
    /// Never assign through this directly - route every write through
    /// `set_state()`, which keeps `recording_active` in sync.
    state: Arc<Mutex<RecordingState>>,
    mode: Arc<Mutex<MicrophoneMode>>,
    app_handle: tauri::AppHandle,

    recorder: Arc<Mutex<Option<AudioRecorder>>>,
    is_open: Arc<Mutex<bool>>,
    is_recording: Arc<Mutex<bool>>,
    mute_state: Arc<Mutex<MuteState>>,
    close_generation: Arc<AtomicU64>,
    cancel_generation: Arc<AtomicU64>,
    stream_router: Arc<StreamRouter>,
    /// Lock-free mirror of "is the state in {Recording, Stopping}",
    /// maintained by `set_state()`. The hot-path `is_recording()` reads THIS
    /// instead of the std `state` mutex, so a UI poll can no longer deadlock
    /// the main/webview thread when a worker holds `state` across a slow
    /// CoreAudio open/close.
    recording_active: Arc<AtomicBool>,
    /// Invalidates asynchronous first-sample UI/chime work when a recording is
    /// stopped or cancelled. This prevents a slow device from producing a late
    /// "ready" indication for a session the user already ended.
    capture_generation: Arc<AtomicU64>,
    /// Resolution of a *named* microphone (selected or clamshell) to its cpal
    /// device, cached so on-demand recording starts skip the full device
    /// enumeration (~40-110ms). Keyed by the resolved name, so a settings
    /// change misses naturally; cleared when an open fails (device unplugged)
    /// so the retry re-enumerates. The system-default case is never cached -
    /// the recorder resolves the current default itself, cheaply.
    cached_device: Arc<Mutex<Option<(String, cpal::Device)>>>,
    /// The recorder for REMOTE (companion) sessions. A second recorder, not
    /// a mode of the local one: the local recorder keeps its cpal stream
    /// untouched, and the remote one hands its ring producer to the
    /// companion server. Built by the same `create_audio_recorder`, so VAD,
    /// level, and StreamRouter wiring are identical for both sources.
    remote_recorder: Arc<Mutex<Option<AudioRecorder>>>,
    /// The current session's push handle into the remote recorder's ring.
    /// Cloned out for the companion server; cleared when the session ends.
    remote_source: Arc<Mutex<Option<RemoteAudioSource>>>,
    /// Which recorder a stop/cancel must talk to. Only meaningful while a
    /// session is live; reset to Local when a session ends.
    active_source: Arc<Mutex<CaptureSource>>,
    /// Factory for building recorders. Production always uses
    /// `create_audio_recorder`; tests inject a bare recorder so the manager
    /// logic can run without the Silero ONNX asset.
    recorder_factory: RecorderFactory,
}

type RecorderFactory =
    Arc<dyn Fn(&tauri::AppHandle) -> Result<AudioRecorder, anyhow::Error> + Send + Sync>;

fn default_recorder_factory(
    stream_router: Arc<StreamRouter>,
) -> impl Fn(&tauri::AppHandle) -> Result<AudioRecorder, anyhow::Error> + Send + Sync {
    move |app| {
        let settings = get_settings(app);
        create_audio_recorder(
            settings.vad_backend,
            app,
            settings.selected_channel,
            Arc::clone(&stream_router),
        )
    }
}

/// Effective post-release capture for a stop: the explicit
/// extra-recording buffer, raised to the streaming release tail when the
/// recording ran with an ACTIVE STREAM. The predicate is stream-active
/// (the router's open flag), NOT the VAD policy: a streaming model with
/// `vad_enabled = false` gets `VadPolicy::Disabled` yet still streams and
/// still loses trailing words on a quick release. Setting the tail to 0
/// restores the old behavior exactly; a non-streaming session is governed
/// by the batch buffer alone.
fn effective_release_buffer_ms(extra_ms: u64, tail_ms: u64, stream_active: bool) -> u64 {
    if stream_active {
        extra_ms.max(tail_ms)
    } else {
        extra_ms
    }
}

impl AudioRecordingManager {
    /* ---------- construction ------------------------------------------------ */

    pub fn new(
        app: &tauri::AppHandle,
        stream_router: Arc<StreamRouter>,
    ) -> Result<Self, anyhow::Error> {
        let settings = get_settings(app);
        let mode = if settings.always_on_microphone {
            MicrophoneMode::AlwaysOn
        } else {
            MicrophoneMode::OnDemand
        };

        let recorder_factory: RecorderFactory =
            Arc::new(default_recorder_factory(Arc::clone(&stream_router)));

        let manager = Self {
            state: Arc::new(Mutex::new(RecordingState::Idle)),
            mode: Arc::new(Mutex::new(mode.clone())),
            app_handle: app.clone(),

            recorder: Arc::new(Mutex::new(None)),
            is_open: Arc::new(Mutex::new(false)),
            is_recording: Arc::new(Mutex::new(false)),
            mute_state: Arc::new(Mutex::new(MuteState::default())),
            close_generation: Arc::new(AtomicU64::new(0)),
            cancel_generation: Arc::new(AtomicU64::new(0)),
            stream_router,
            recording_active: Arc::new(AtomicBool::new(false)),
            capture_generation: Arc::new(AtomicU64::new(0)),
            cached_device: Arc::new(Mutex::new(None)),
            remote_recorder: Arc::new(Mutex::new(None)),
            remote_source: Arc::new(Mutex::new(None)),
            active_source: Arc::new(Mutex::new(CaptureSource::Local)),
            recorder_factory,
        };

        // Always-on?  Open immediately. Best-effort by design: a machine
        // with no input device, revoked microphone permission, or a missing
        // VAD resource must not panic the app at every launch (this runs
        // during Tauri setup, before any window exists). The manager stays
        // functional with the stream closed - try_start_recording_for
        // re-attempts the open on every local start and surfaces the real
        // error there, where the user can act on it.
        if matches!(mode, MicrophoneMode::AlwaysOn) {
            if let Err(e) = manager.start_microphone_stream() {
                error!(
                    "Always-on microphone stream failed to open at startup \
                     (will retry on the next recording): {e:#}"
                );
            }
        }

        Ok(manager)
    }

    /* ---------- helper methods --------------------------------------------- */

    /// The persisted microphone preference currently in effect. Only runs the
    /// clamshell probe (an `ioreg` subprocess, ~10-20ms) when a clamshell
    /// microphone is actually configured.
    fn desired_microphone(&self, settings: &AppSettings) -> DesiredMicrophone {
        if let Some(clamshell_microphone) = &settings.clamshell_microphone {
            let clamshell_started = Instant::now();
            let is_clamshell = clamshell::is_clamshell().unwrap_or(false);
            debug!(
                "device resolve: clamshell_check={:?} (clamshell={})",
                clamshell_started.elapsed(),
                is_clamshell
            );
            if is_clamshell {
                return DesiredMicrophone::Clamshell(clamshell_microphone.clone());
            }
        }
        match &settings.selected_microphone {
            Some(name) => DesiredMicrophone::Selected(name.clone()),
            None => DesiredMicrophone::Default,
        }
    }

    pub fn invalidate_device_cache(&self) {
        *self.cached_device.lock().unwrap() = None;
    }

    fn resolve_microphone_device(&self, settings: &AppSettings) -> MicrophoneResolution {
        let desired = self.desired_microphone(settings);
        let (device_name, selected_microphone) = match desired {
            DesiredMicrophone::Default => {
                debug!("device resolve: no mic configured -> system default");
                return MicrophoneResolution {
                    device: None,
                    unavailable_selected_microphone: None,
                };
            }
            DesiredMicrophone::Selected(name) => (name.clone(), Some(name)),
            DesiredMicrophone::Clamshell(name) => (name, None),
        };

        // Cache hit: skip the full enumeration. A stale device (unplugged)
        // fails at open, where the caller invalidates and retries fresh.
        if let Some((cached_name, device)) = self.cached_device.lock().unwrap().as_ref() {
            if *cached_name == device_name {
                debug!("device resolve: cache hit for '{}'", device_name);
                return MicrophoneResolution {
                    device: Some(device.clone()),
                    unavailable_selected_microphone: None,
                };
            }
        }

        // Only report a selected microphone as unavailable when enumeration
        // itself succeeded. A backend enumeration error may be transient and
        // must not erase the user's persisted preference.
        let enumerate_started = Instant::now();
        let (device, enumeration_succeeded) = match list_input_devices() {
            Ok(devices) => (
                devices
                    .into_iter()
                    .find(|d| d.name == device_name)
                    .map(|d| d.device),
                true,
            ),
            Err(e) => {
                debug!("Failed to list devices, using default: {}", e);
                (None, false)
            }
        };
        debug!(
            "device resolve: enumerate={:?} (found={})",
            enumerate_started.elapsed(),
            device.is_some()
        );
        if let Some(d) = &device {
            *self.cached_device.lock().unwrap() = Some((device_name, d.clone()));
        }

        let unavailable_selected_microphone = if enumeration_succeeded && device.is_none() {
            selected_microphone
        } else {
            None
        };
        MicrophoneResolution {
            device,
            unavailable_selected_microphone,
        }
    }

    /// Keep persisted settings and the UI aligned with a successful runtime
    /// fallback. Re-read first so recovery cannot clear a microphone the user
    /// selected concurrently while the stream was being rebuilt.
    fn persist_default_microphone_after_fallback(&self, unavailable_name: &str) {
        let mut settings = get_settings(&self.app_handle);
        if settings.selected_microphone.as_deref() != Some(unavailable_name) {
            return;
        }

        settings.selected_microphone = None;
        write_settings(&self.app_handle, settings);
        let _ = self.app_handle.emit(
            "settings-changed",
            serde_json::json!({
                "setting": "selected_microphone",
                "value": "Default"
            }),
        );
    }

    fn schedule_lazy_close(&self) {
        let gen = self.close_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let app = self.app_handle.clone();
        std::thread::spawn(move || {
            std::thread::sleep(STREAM_IDLE_TIMEOUT);
            let rm = app.state::<Arc<AudioRecordingManager>>();
            // Hold state lock across the check AND close to serialize against
            // try_start_recording, preventing a race where the stream is closed
            // under an active recording.
            let state = rm.state.lock().unwrap();
            if rm.close_generation.load(Ordering::SeqCst) == gen
                && matches!(*state, RecordingState::Idle)
            {
                // stop_microphone_stream does not acquire the state lock,
                // so holding it here is safe (no deadlock).
                info!(
                    "Closing idle microphone stream after {:?}",
                    STREAM_IDLE_TIMEOUT
                );
                rm.stop_microphone_stream();
            }
        });
    }

    /* ---------- microphone life-cycle -------------------------------------- */

    /// Applies mute if mute_while_recording is enabled and a capture path is
    /// active. Session-scoped: a LOCAL session always has the mic stream
    /// open (`is_open`, the original predicate, so local behavior is
    /// unchanged), while a REMOTE (companion) session has no local stream
    /// and is covered by the recording mirror instead.
    /// Snapshots the system's prior mute state first so `remove_mute` can
    /// restore it instead of unconditionally unmuting.
    pub fn apply_mute(&self) {
        let settings = get_settings(&self.app_handle);
        if !settings.mute_while_recording {
            return;
        }

        // Lock order: is_open before mute_state (matches stop_microphone_stream).
        let is_open = self.is_open.lock().unwrap();
        let mut mute_guard = self.mute_state.lock().unwrap();
        // Already muted this session - don't re-snapshot, or a duplicate/late
        // apply would overwrite prev_muted with our own forced-muted state and
        // strand audio muted on stop.
        if mute_guard.did_mute {
            return;
        }
        if *is_open || self.is_recording() {
            mute_guard.prev_muted = get_mute();
            set_mute(true);
            mute_guard.did_mute = true;
            debug!("Mute applied (prev_muted={:?})", mute_guard.prev_muted);
        }
    }

    /// Removes mute if it was applied, restoring the system's prior mute state
    /// (a system already muted before recording stays muted).
    pub fn remove_mute(&self) {
        let mut mute_guard = self.mute_state.lock().unwrap();
        if let Some(prev_muted) = mute_restore_action(&mute_guard) {
            restore_mute(prev_muted);
            mute_guard.did_mute = false;
            debug!("Mute removed (restored prev_muted={:?})", prev_muted);
        }
    }

    pub fn preload_vad(&self) -> Result<(), anyhow::Error> {
        let mut recorder_opt = self.recorder.lock().unwrap();
        if recorder_opt.is_none() {
            *recorder_opt = Some((self.recorder_factory)(&self.app_handle)?);
        }
        Ok(())
    }

    pub fn start_microphone_stream(&self) -> Result<(), anyhow::Error> {
        let mut open_flag = self.is_open.lock().unwrap();
        if *open_flag {
            // `is_open` only records that we opened a stream at some point, not
            // that one is still running. If capture has since failed (mic
            // unplugged mid-session, USB dropout), rebuild it before the next
            // recording instead of handing the caller a stalled recorder.
            let needs_reopen = self
                .recorder
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|rec| rec.needs_reopen());

            if !needs_reopen {
                // trace, not debug: with the aliveness check in
                // try_start_recording this now fires on every keypress in
                // always-on mode.
                trace!("Microphone stream already active");
                return Ok(());
            }

            warn!("Microphone stream is no longer running (device disconnected?); reopening");

            // Torn down inline rather than via stop_microphone_stream(), which
            // takes the `is_open` lock we are already holding.
            {
                let mut mute_guard = self.mute_state.lock().unwrap();
                if mute_guard.did_mute {
                    restore_mute(mute_guard.prev_muted);
                    mute_guard.did_mute = false;
                }
            }
            if let Some(rec) = self.recorder.lock().unwrap().as_mut() {
                let _ = rec.close();
            }
            *self.is_recording.lock().unwrap() = false;
            *open_flag = false;
            self.invalidate_device_cache();
            // Fall through to the same fresh resolution and fallback path used
            // when an on-demand stream opens after its device was unplugged.
        }

        let start_time = Instant::now();

        // Don't mute immediately - caller will handle muting after audio feedback.
        // The previous stream restored audio on close, so did_mute should already
        // be false here; if it somehow isn't, restore rather than just clearing the
        // flag, which would strand system audio muted.
        {
            let mut mute_guard = self.mute_state.lock().unwrap();
            if mute_guard.did_mute {
                restore_mute(mute_guard.prev_muted);
                mute_guard.did_mute = false;
            }
        }

        // Get the selected device from settings, considering clamshell mode.
        // No pre-flight enumeration here: when nothing is configured the
        // recorder resolves the system default itself, and a machine with no
        // input devices at all fails inside open() with the same
        // "No input device found" error this used to check for.
        let settings = get_settings(&self.app_handle);
        let resolve_started = Instant::now();
        let mut resolution = self.resolve_microphone_device(&settings);
        let resolve_elapsed = resolve_started.elapsed();

        // Ensure VAD is loaded if it wasn't for whatever reason
        let vad_started = Instant::now();
        self.preload_vad()?;
        let vad_elapsed = vad_started.elapsed();

        let open_started = Instant::now();
        let mut recorder_opt = self.recorder.lock().unwrap();
        if let Some(rec) = recorder_opt.as_mut() {
            if let Err(first_err) = rec.open(resolution.device.clone()) {
                // A cached device or config may have gone stale (unplugged,
                // rate/format changed). Re-resolve from a fresh enumeration and
                // retry once before surfacing the error.
                warn!("Recorder open failed ({first_err}); re-resolving device and retrying once");
                self.invalidate_device_cache();
                resolution = self.resolve_microphone_device(&settings);
                rec.open(resolution.device.clone())
                    .map_err(|e| anyhow::anyhow!("Failed to open recorder: {}", e))?;
            }
        }
        debug!(
            "mic stream breakdown: device_resolve={:?} vad_ensure={:?} open={:?}",
            resolve_elapsed,
            vad_elapsed,
            open_started.elapsed()
        );
        drop(recorder_opt);

        *open_flag = true;
        if let Some(unavailable_name) = resolution.unavailable_selected_microphone {
            // Do this only after the default stream opened successfully. A
            // failed fallback must not erase the user's microphone preference.
            self.persist_default_microphone_after_fallback(&unavailable_name);
        }
        // This timing covers through cpal's stream.play() returning - i.e. the
        // point cpal surfaces as "stream running." It does NOT guarantee the
        // host audio device is producing samples yet; the first input callback
        // fires asynchronously one buffer period later (hardware dependent,
        // typically ~10-200ms on macOS, longer on Bluetooth/USB).
        info!(
            "Microphone stream initialized in {:?}",
            start_time.elapsed()
        );
        Ok(())
    }

    pub fn stop_microphone_stream(&self) {
        let mut open_flag = self.is_open.lock().unwrap();
        if !*open_flag {
            return;
        }

        {
            let mut mute_guard = self.mute_state.lock().unwrap();
            if mute_guard.did_mute {
                restore_mute(mute_guard.prev_muted);
            }
            mute_guard.did_mute = false;
        }

        if let Some(rec) = self.recorder.lock().unwrap().as_mut() {
            // If still recording, stop first.
            if *self.is_recording.lock().unwrap() {
                let _ = rec.stop();
                *self.is_recording.lock().unwrap() = false;
            }
            let _ = rec.close();
        }

        *open_flag = false;
        debug!("Microphone stream stopped");
    }

    /* ---------- mode switching --------------------------------------------- */

    pub fn update_mode(&self, new_mode: MicrophoneMode) -> Result<(), anyhow::Error> {
        let cur_mode = self.mode.lock().unwrap().clone();

        match (cur_mode, &new_mode) {
            (MicrophoneMode::AlwaysOn, MicrophoneMode::OnDemand) => {
                if matches!(*self.state.lock().unwrap(), RecordingState::Idle) {
                    self.close_generation.fetch_add(1, Ordering::SeqCst);
                    self.stop_microphone_stream();
                }
            }
            (MicrophoneMode::OnDemand, MicrophoneMode::AlwaysOn) => {
                self.close_generation.fetch_add(1, Ordering::SeqCst);
                self.start_microphone_stream()?;
            }
            _ => {}
        }

        *self.mode.lock().unwrap() = new_mode;
        Ok(())
    }

    /* ---------- recording --------------------------------------------------- */

    /// The one place `state` is written. Derives `recording_active` (the
    /// lock-free mirror read by `is_recording()`) from the new value itself,
    /// so the two can never drift: a new `RecordingState` variant only needs
    /// its active-set membership decided here, once.
    fn set_state(&self, guard: &mut RecordingState, new_state: RecordingState) {
        *guard = new_state;
        self.recording_active.store(
            matches!(
                *guard,
                RecordingState::Recording { .. } | RecordingState::Stopping
            ),
            Ordering::SeqCst,
        );
    }

    /// Start a recording session from `source`. The single-session state
    /// machine is shared: a remote session and a local hotkey press
    /// arbitrate exactly like the two keyboard bindings do today ("Already
    /// recording"). `Local` is byte-for-byte the old `try_start_recording`
    /// path; `Remote` opens the companion recorder instead of the cpal
    /// stream and remembers the source for stop/cancel.
    pub fn try_start_recording_for(
        &self,
        source: CaptureSource,
        binding_id: &str,
        vad_policy: VadPolicy,
    ) -> Result<RecordingReadiness, String> {
        let mut state = self.state.lock().unwrap();

        if let RecordingState::Idle = *state {
            // Cancel any pending lazy close (no-op in always-on mode, where
            // closes are never scheduled).
            self.close_generation.fetch_add(1, Ordering::SeqCst);
            match source {
                CaptureSource::Local => {
                    // Opens the stream in on-demand mode. In always-on mode
                    // the stream is normally already open and this is a
                    // cheap aliveness check - but if the capture worker died
                    // (device disconnect), it rebuilds the stream instead of
                    // leaving every subsequent start wedged on "Recorder not
                    // available".
                    if let Err(e) = self.start_microphone_stream() {
                        let msg = format!("{e}");
                        error!("Failed to open microphone stream: {msg}");
                        return Err(msg);
                    }
                }
                CaptureSource::Remote => {
                    if let Err(e) = self.start_remote_stream() {
                        let msg = format!("{e}");
                        error!("Failed to open remote capture source: {msg}");
                        return Err(msg);
                    }
                }
            }

            let recorder_slot = match source {
                CaptureSource::Local => &self.recorder,
                CaptureSource::Remote => &self.remote_recorder,
            };
            if let Some(rec) = recorder_slot.lock().unwrap().as_ref() {
                match rec.start(vad_policy) {
                    Ok(receiver) => {
                        let generation = self.capture_generation.fetch_add(1, Ordering::AcqRel) + 1;
                        *self.is_recording.lock().unwrap() = true;
                        *self.active_source.lock().unwrap() = source;
                        self.set_state(
                            &mut state,
                            RecordingState::Recording {
                                binding_id: binding_id.to_string(),
                            },
                        );
                        debug!("Recording requested for binding {binding_id} ({source:?})");
                        return Ok(RecordingReadiness {
                            receiver,
                            generation,
                        });
                    }
                    Err(error) => return Err(format!("Failed to start recorder: {error}")),
                }
            }
            Err("Recorder not available".to_string())
        } else {
            Err("Already recording".to_string())
        }
    }

    /// Ensure the remote recorder exists and is open, reusing a healthy one
    /// (the companion server pre-warms it when the feature is enabled so the
    /// press-to-capture path pays no VAD load). Rebuilds a dead worker like
    /// `start_microphone_stream` does for a disconnected mic.
    fn start_remote_stream(&self) -> Result<(), anyhow::Error> {
        let needs_reopen = self
            .remote_recorder
            .lock()
            .unwrap()
            .as_ref()
            .map_or(true, |rec| rec.needs_reopen());
        if !needs_reopen {
            return Ok(());
        }

        if let Some(rec) = self.remote_recorder.lock().unwrap().as_mut() {
            let _ = rec.close();
        }
        let mut rec = (self.recorder_factory)(&self.app_handle)?;
        let source = rec
            .open_remote()
            .map_err(|e| anyhow::anyhow!("Failed to open remote capture source: {}", e))?;
        *self.remote_source.lock().unwrap() = Some(source);
        *self.remote_recorder.lock().unwrap() = Some(rec);
        Ok(())
    }

    /// The push handle for the current remote recorder, if one is open. The
    /// companion server clones this to feed phone audio into the ring.
    pub fn remote_source(&self) -> Option<RemoteAudioSource> {
        self.remote_source.lock().unwrap().clone()
    }

    /// Pre-warm the remote recorder (loads the VAD, opens the ring) while
    /// idle so a phone press pays no recorder construction cost. Called by
    /// the companion server when the feature is enabled; refuses while any
    /// session is live.
    pub fn prewarm_remote(&self) -> Result<(), anyhow::Error> {
        let state = self.state.lock().unwrap();
        if !matches!(*state, RecordingState::Idle) {
            return Ok(()); // a session is live; its recorder already exists
        }
        drop(state);
        self.start_remote_stream()
    }

    /// Which recorder the live session (if any) is using.
    pub fn active_source(&self) -> CaptureSource {
        *self.active_source.lock().unwrap()
    }

    /// Close the remote recorder and its push handle outside a session
    /// (companion feature disabled, or app shutdown). A live remote session
    /// must be stopped first; this only tears down the idle recorder.
    pub fn close_remote_stream(&self) {
        if let Some(rec) = self.remote_recorder.lock().unwrap().as_mut() {
            let _ = rec.close();
        }
        *self.remote_recorder.lock().unwrap() = None;
        *self.remote_source.lock().unwrap() = None;
        *self.active_source.lock().unwrap() = CaptureSource::Local;
    }

    /// Replace the VAD implementation while idle. If the microphone stream is
    /// currently warm (always-on or lazy-close mode), reopen it with the new
    /// detector before reporting success. A failed reopen restores the previous
    /// recorder so the persisted setting can remain unchanged.
    pub fn update_vad_backend(&self, backend: VadBackend) -> Result<(), anyhow::Error> {
        let state = self.state.lock().unwrap();
        if !matches!(*state, RecordingState::Idle) {
            return Err(anyhow::anyhow!(
                "Cannot change the VAD backend while recording"
            ));
        }

        let settings = get_settings(&self.app_handle);
        let replacement = create_audio_recorder(
            backend,
            &self.app_handle,
            settings.selected_channel,
            Arc::clone(&self.stream_router),
        )?;
        let was_open = *self.is_open.lock().unwrap();

        // Invalidate any delayed close before swapping the recorder it targets.
        self.close_generation.fetch_add(1, Ordering::SeqCst);
        if was_open {
            self.stop_microphone_stream();
        }

        let previous_recorder = self.recorder.lock().unwrap().replace(replacement);
        if was_open {
            if let Err(change_error) = self.start_microphone_stream() {
                // Ensure a partially opened replacement cannot retain capture
                // resources before restoring the known-good detector.
                if let Some(recorder) = self.recorder.lock().unwrap().as_mut() {
                    let _ = recorder.close();
                }
                *self.recorder.lock().unwrap() = previous_recorder;

                if let Err(rollback_error) = self.start_microphone_stream() {
                    error!(
                        "Failed to restore microphone stream after VAD backend change failed: {rollback_error}"
                    );
                }
                return Err(anyhow::anyhow!(
                    "Failed to reopen microphone with {:?} VAD: {change_error}",
                    backend
                ));
            }
        }

        info!("VAD backend changed to {:?}", backend);
        drop(state);
        Ok(())
    }

    /// Switch the capture to a newly selected microphone. Rejected while a
    /// recording is live: restarting an active capture would discard its
    /// samples and desync the recording state - the same rule as
    /// [`Self::update_selected_channel`]. On rejection nothing changes,
    /// not the live capture and not the persisted preference. On
    /// acceptance the preference is persisted here (the restart resolves
    /// the device from settings at open time, so it must be on disk before
    /// the stream reopens) and an open stream restarts on the new device;
    /// a failed restart rolls the preference back and reopens the previous
    /// device (KB-104: the settings UI rolls its dropdown back on this
    /// error, and the store must agree with it instead of holding a mic
    /// the capture cannot open).
    pub fn update_selected_device(
        &self,
        selected_microphone: Option<String>,
    ) -> Result<(), anyhow::Error> {
        // Serialize against recording start/stop for the whole switch,
        // like the channel change does.
        let state = self.state.lock().unwrap();
        if capture_restart_forbidden(&state) {
            return Err(anyhow::anyhow!(
                "Cannot change the selected microphone while recording"
            ));
        }

        let mut settings = get_settings(&self.app_handle);
        let previous_device = settings.selected_microphone.clone();
        if settings.selected_microphone != selected_microphone {
            settings.selected_microphone = selected_microphone;
            write_settings(&self.app_handle, settings);
            // The same convergence signal the fallback path emits when it
            // rewrites this field, so every open surface re-reads the store.
            let _ = self.app_handle.emit(
                "settings-changed",
                serde_json::json!({
                    "setting": "selected_microphone"
                }),
            );
        }

        // Device settings changed; re-enumerate the device and restart capture.
        self.invalidate_device_cache();
        let was_open = *self.is_open.lock().unwrap();
        if was_open {
            self.close_generation.fetch_add(1, Ordering::SeqCst);
            self.stop_microphone_stream();
            if let Err(restart_error) = self.start_microphone_stream() {
                // KB-104: the reopen resolves its device from settings, so
                // the write above had to precede it - undo it after a failed
                // reopen and bring the previous device's stream back, or
                // settings_store.json would keep a microphone the capture
                // cannot open while the UI (which rolls back on this error)
                // still shows the old one.
                let mut settings = get_settings(&self.app_handle);
                if settings.selected_microphone != previous_device {
                    settings.selected_microphone = previous_device;
                    write_settings(&self.app_handle, settings);
                    let _ = self.app_handle.emit(
                        "settings-changed",
                        serde_json::json!({
                            "setting": "selected_microphone"
                        }),
                    );
                }
                if let Err(rollback_error) = self.start_microphone_stream() {
                    error!(
                        "Failed to restore the microphone stream after a failed device switch: {rollback_error}"
                    );
                }
                return Err(restart_error);
            }
        }
        Ok(())
    }

    pub fn update_selected_channel(
        &self,
        selected_channel: Option<u16>,
    ) -> Result<(), anyhow::Error> {
        // Serialize against recording start/stop. Restarting an active capture
        // would discard its samples and leave the manager's recording state out
        // of sync with the new recorder.
        let state = self.state.lock().unwrap();
        if !matches!(*state, RecordingState::Idle) {
            return Err(anyhow::anyhow!(
                "Cannot change the input channel while recording"
            ));
        }

        let previous_channel = get_settings(&self.app_handle).selected_channel;
        let was_open = *self.is_open.lock().unwrap();
        if was_open {
            self.close_generation.fetch_add(1, Ordering::SeqCst);
            self.stop_microphone_stream();
        }
        if let Some(recorder) = self.recorder.lock().unwrap().as_mut() {
            recorder.set_selected_channel(selected_channel);
        }
        if was_open {
            if let Err(error) = self.start_microphone_stream() {
                if let Some(recorder) = self.recorder.lock().unwrap().as_mut() {
                    recorder.set_selected_channel(previous_channel);
                }
                return Err(error);
            }
        }
        drop(state);
        Ok(())
    }

    /// Invalidate pending first-sample UI and audio-feedback work immediately.
    /// Called at the beginning of stop, before the slower capture drain starts.
    pub fn invalidate_recording_readiness(&self) {
        self.capture_generation.fetch_add(1, Ordering::AcqRel);
    }

    pub fn is_recording_readiness_current(&self, generation: u64) -> bool {
        self.capture_generation.load(Ordering::Acquire) == generation
    }

    pub fn cancel_generation(&self) -> u64 {
        self.cancel_generation.load(Ordering::Acquire)
    }

    pub fn was_cancelled_since(&self, generation: u64) -> bool {
        self.cancel_generation.load(Ordering::Acquire) != generation
    }

    pub fn stop_recording(&self, binding_id: &str, cancel_generation: u64) -> Option<Vec<f32>> {
        self.invalidate_recording_readiness();
        let mut state = self.state.lock().unwrap();

        match *state {
            RecordingState::Recording {
                binding_id: ref active,
            } if active == binding_id => {
                self.set_state(&mut state, RecordingState::Stopping);
                drop(state);

                // Optionally keep recording for a bit longer to capture trailing
                // audio. Streaming sessions additionally get the streaming
                // release tail (see effective_release_buffer_ms): releasing
                // the hotkey the instant a command word ends otherwise
                // truncates its final consonant and the command silently
                // fails. The predicate is STREAM-ACTIVE, not the VAD policy:
                // a streaming model with VAD disabled still streams and
                // still loses trailing words on a quick release.
                let settings = get_settings(&self.app_handle);
                let stream_active = self.stream_router.is_open();
                let buffer_ms = effective_release_buffer_ms(
                    settings.extra_recording_buffer_ms,
                    settings.streaming_release_tail_ms,
                    stream_active,
                );
                if buffer_ms > 0 {
                    debug!(
                        "Extra recording buffer: sleeping {}ms before stopping",
                        buffer_ms
                    );
                    let started = Instant::now();
                    let buffer = Duration::from_millis(buffer_ms);
                    while started.elapsed() < buffer {
                        if self.was_cancelled_since(cancel_generation) {
                            debug!("Recording stop cancelled during extra buffer");
                            break;
                        }
                        let remaining = buffer.saturating_sub(started.elapsed());
                        std::thread::sleep(remaining.min(Duration::from_millis(25)));
                    }
                }

                let source = self.active_source();
                let recorder_slot = match source {
                    CaptureSource::Local => &self.recorder,
                    CaptureSource::Remote => &self.remote_recorder,
                };
                let samples = if let Some(rec) = recorder_slot.lock().unwrap().as_ref() {
                    match rec.stop() {
                        Ok(buf) => buf,
                        Err(e) => {
                            error!("stop() failed: {e}");
                            Vec::new()
                        }
                    }
                } else {
                    error!("Recorder not available");
                    Vec::new()
                };

                *self.is_recording.lock().unwrap() = false;
                self.set_state(&mut self.state.lock().unwrap(), RecordingState::Idle);

                match source {
                    CaptureSource::Remote => {
                        // The phone session is over: close the remote
                        // recorder so a stale socket cannot keep pushing
                        // into a dead ring, and clear the push handle.
                        self.close_remote_stream();
                    }
                    CaptureSource::Local => {
                        // In on-demand mode, close the mic (lazily if the
                        // setting is enabled)
                        if matches!(*self.mode.lock().unwrap(), MicrophoneMode::OnDemand) {
                            if get_settings(&self.app_handle).lazy_stream_close {
                                self.schedule_lazy_close();
                            } else {
                                self.stop_microphone_stream();
                            }
                        }
                    }
                }

                if self.was_cancelled_since(cancel_generation) {
                    debug!("Recording stop cancelled; discarding captured samples");
                    return None;
                }

                // Pad if very short
                let s_len = samples.len();
                // debug!("Got {} samples", s_len);
                if s_len < WHISPER_SAMPLE_RATE && s_len > 0 {
                    let mut padded = samples;
                    padded.resize(WHISPER_SAMPLE_RATE * 5 / 4, 0.0);
                    Some(padded)
                } else {
                    Some(samples)
                }
            }
            _ => None,
        }
    }
    pub fn is_recording(&self) -> bool {
        // Lock-free: mirrors the `state` {Recording, Stopping} membership via
        // an atomic maintained by `set_state()`. Polled from the webview/main
        // thread, so it MUST NOT take the `state` mutex (a worker can hold it
        // across a slow CoreAudio open/close → main-thread deadlock / UI
        // freeze).
        self.recording_active.load(Ordering::SeqCst)
    }

    /// Cancel any ongoing recording without returning audio samples
    pub fn cancel_recording(&self) {
        self.invalidate_recording_readiness();
        self.cancel_generation.fetch_add(1, Ordering::AcqRel);
        let mut state = self.state.lock().unwrap();

        match *state {
            RecordingState::Recording { .. } => {
                self.set_state(&mut state, RecordingState::Idle);
                drop(state);

                // Restore the forced mute the same way the normal stop path
                // does (actions.rs calls remove_mute on stop). Without this,
                // cancellation never restores it: in always-on mode the local
                // stream stays open so stop_microphone_stream never runs, the
                // remote path's close_remote_stream has no mute restore, and
                // the on-demand lazy close defers it by the 30 s idle
                // timeout - the system stayed muted until the next
                // NON-cancelled dictation completed (KB-002).
                self.remove_mute();

                let source = self.active_source();
                let recorder_slot = match source {
                    CaptureSource::Local => &self.recorder,
                    CaptureSource::Remote => &self.remote_recorder,
                };
                if let Some(rec) = recorder_slot.lock().unwrap().as_ref() {
                    let _ = rec.stop(); // Discard the result
                }

                *self.is_recording.lock().unwrap() = false;

                match source {
                    CaptureSource::Remote => self.close_remote_stream(),
                    CaptureSource::Local => {
                        // In on-demand mode, close the mic (lazily if the
                        // setting is enabled)
                        if matches!(*self.mode.lock().unwrap(), MicrophoneMode::OnDemand) {
                            if get_settings(&self.app_handle).lazy_stream_close {
                                self.schedule_lazy_close();
                            } else {
                                self.stop_microphone_stream();
                            }
                        }
                    }
                }
            }
            RecordingState::Stopping => {
                debug!("Cancellation requested while recording is stopping");
                // Defensive twin of the restore above: a cancel that lands
                // while the stop pipeline is finalizing must not leave our
                // forced mute behind even if the pipeline's own teardown is
                // interrupted. remove_mute is a no-op when we did not mute.
                self.remove_mute();
            }
            RecordingState::Idle => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_release_buffer_ms_uses_the_stream_active_predicate() {
        // A streaming session gets at least the release tail: a quick
        // hotkey release right after a command word otherwise truncates
        // its tail (the final text ends up with a partial or missing
        // word and the command silently fails).
        assert_eq!(effective_release_buffer_ms(0, 200, true), 200);
        // An explicit larger batch buffer wins.
        assert_eq!(effective_release_buffer_ms(500, 200, true), 500);
        // No stream (batch session, or the stream already closed): the
        // tail never applies, regardless of the VAD policy.
        assert_eq!(effective_release_buffer_ms(300, 200, false), 300);
        assert_eq!(effective_release_buffer_ms(0, 200, false), 0);
        // The off path: a zero tail restores the old behavior exactly.
        assert_eq!(effective_release_buffer_ms(0, 0, true), 0);
    }

    // Manager-level remote-session tests (start/stop on the remote source
    // with the fed audio returned) are NOT runnable under tauri::test's
    // MockRuntime: AudioRecordingManager pins tauri::AppHandle (the Wry
    // runtime) because get_settings and the settings store are pinned the
    // same way throughout the app, and mock apps hand out
    // AppHandle<MockRuntime>. The remote audio path is covered instead at
    // the recorder level (audio_toolkit/audio/recorder/tests.rs:
    // remote_source_round_trips_fed_samples and friends) and the
    // single-session arbitration - the same rule local hotkeys follow - at
    // the coordinator level (transcription_coordinator tests:
    // companion_edges_*).

    /// The mute-restore decision shared by every teardown path (normal
    /// stop, stream close, cancellation - local or companion source): a
    /// forced mute yields the snapshotted prior state exactly once; no
    /// forced mute yields nothing. This is the seam cancel_recording now
    /// routes through (remove_mute), so cancelling restores the system
    /// audio the same instant the normal stop does instead of stranding
    /// it muted until the next completed dictation (KB-002).
    /// The capture-restart guard behind the device/channel/VAD switch
    /// rejections: a live recording (Recording or Stopping) must never be
    /// restarted underneath, because the restart discards its captured
    /// samples - switching the microphone mid-dictation used to kill the
    /// recording silently (round 3). Idle is the only state that permits a
    /// capture-shape change.
    #[test]
    fn capture_restart_is_forbidden_while_recording_or_stopping() {
        assert!(
            !capture_restart_forbidden(&RecordingState::Idle),
            "idle: the capture may be restarted"
        );
        assert!(
            capture_restart_forbidden(&RecordingState::Recording {
                binding_id: "transcribe".to_string()
            }),
            "a live recording must never have its capture restarted"
        );
        assert!(
            capture_restart_forbidden(&RecordingState::Stopping),
            "a recording in its stop pipeline still holds samples: no restart"
        );
    }

    /// The mute-restore decision shared by every teardown path (normal
    /// stop, stream close, cancellation - local or companion source): a
    /// forced mute yields the snapshotted prior state exactly once; no
    /// forced mute yields nothing. This is the seam cancel_recording now
    /// routes through (remove_mute), so cancelling restores the system
    /// audio the same instant the normal stop does instead of stranding
    /// it muted until the next completed dictation (KB-002).
    #[test]
    fn mute_restore_action_yields_the_snapshot_only_while_forced() {
        assert_eq!(
            mute_restore_action(&MuteState::default()),
            None,
            "no forced mute (mute_while_recording off, or readiness never reached): nothing to restore"
        );

        // The live dictation shape: forced over an unmuted system.
        assert_eq!(
            mute_restore_action(&MuteState {
                did_mute: true,
                prev_muted: Some(false),
            }),
            Some(Some(false)),
            "a forced mute restores the snapshotted unmuted state"
        );

        // A system the user had already muted stays muted.
        assert_eq!(
            mute_restore_action(&MuteState {
                did_mute: true,
                prev_muted: Some(true),
            }),
            Some(Some(true)),
            "the user's own mute is restored, not lifted"
        );

        // Unknown prior state restores as unknown (restore_mute defaults
        // to unmuting so audio is never left muted by us).
        assert_eq!(
            mute_restore_action(&MuteState {
                did_mute: true,
                prev_muted: None,
            }),
            Some(None),
        );
    }

    /// The restore semantics under the test spy (no real system volume is
    /// touched): only a snapshot of Some(true) keeps the mute; everything
    /// else unmutes, and repeated restores are single-shot.
    #[test]
    fn restore_mute_unmutes_unless_the_user_was_already_muted() {
        let _ = mute_test_log::take();

        restore_mute(Some(false));
        assert_eq!(mute_test_log::take(), vec!["unmute"]);

        restore_mute(None);
        assert_eq!(
            mute_test_log::take(),
            vec!["unmute"],
            "unknown prior state defaults to unmuting"
        );

        restore_mute(Some(true));
        assert_eq!(
            mute_test_log::take(),
            Vec::<&'static str>::new(),
            "a pre-existing user mute is never lifted"
        );
    }
}
