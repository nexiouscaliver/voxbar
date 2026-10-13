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

/// KB-237: whether flipping always-on off may close the local mic stream
/// right now. Only a live LOCAL recording defers the close; its own stop or
/// cancel pipeline performs it once the samples are drained (both check the
/// mode, which is already OnDemand by then). A companion lane session NEVER
/// defers the close: lane recorders are fully independent of the local
/// stream (the v1.6.2 W1-L2 per-lane split), so a lane session leaves the
/// state Idle and the flip closes the Mac's microphone immediately even
/// while a phone records. Before that split the remote session shared this
/// state machine, this guard read as "not Idle", a flip during a phone
/// session was skipped, and the remote stop arm never closed the local
/// stream: the local microphone stayed hot forever after the user had
/// turned always-on off. Deliberately the same predicate family as
/// [`capture_restart_forbidden`] (one rule about live local recordings, two
/// consumers with different questions), so the privacy contract and the
/// capture-shape contract can never drift apart.
fn mode_flip_may_close_local_stream(state: &RecordingState) -> bool {
    !capture_restart_forbidden(state)
}

#[derive(Clone, Debug)]
pub enum MicrophoneMode {
    AlwaysOn,
    OnDemand,
}

/// Where a recording session's audio comes from. `Local` is the cpal
/// microphone path exactly as it has always run; `Remote` is a companion
/// device (phone/tablet on the LAN) pushing 16 kHz mono chunks through a
/// `RemoteAudioSource` (v1.6.2 W1-L2: one recorder PER CONNECTION LANE, so
/// several phones can record simultaneously). Both sources share the
/// recorder construction (same VAD, level, and StreamRouter wiring), so
/// everything downstream of each ring is byte-identical.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureSource {
    Local,
    Remote,
}

/// v1.6.2 W1-L2: which session owns the process's ONE streaming engine.
/// The ASR model is a singleton: at most one live session may feed
/// `StreamRouter` (its frames would otherwise interleave two speakers into
/// one transcript). The owner is claimed on the coordinator thread at the
/// single serialized instant a start commits (first session wins); every
/// other concurrently-recording session runs with the Offline VAD policy
/// and batch-transcribes its ring at stop. A `Remote` lane's recorder feeds
/// the router only while IT holds the token, and the local recorder feeds
/// it only while a local session holds it, so a session that starts while
/// another's stop pipeline is still draining can never pollute the
/// draining stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamOwner {
    /// No live session owns the stream (no stream is open at all, or the
    /// owner's stop pipeline already finalized).
    None,
    /// The keyboard/signal session on the local microphone.
    Local,
    /// The companion connection identified by its server-assigned lane id.
    Lane(u64),
}

/// The lock-free encoding of [`StreamOwner`] the recorder callbacks check
/// per frame (a relaxed atomic load, never a mutex: this runs on the audio
/// path). 0 means none, 1 means local, lane ids encode as lane + 1 (lane
/// ids start at 1, so the encoding is injective).
pub(crate) fn stream_owner_token(owner: StreamOwner) -> u64 {
    match owner {
        StreamOwner::None => 0,
        StreamOwner::Local => 1,
        StreamOwner::Lane(lane) => lane + 1,
    }
}

/// The inverse of [`stream_owner_token`]. Pure, and paired with it in tests.
pub(crate) fn stream_owner_from_token(token: u64) -> StreamOwner {
    match token {
        0 => StreamOwner::None,
        1 => StreamOwner::Local,
        lane => StreamOwner::Lane(lane - 1),
    }
}

/// v1.6.2 fleet pass 1: what an Escape actually cancelled, resolved by
/// [`AudioRecordingManager::cancel_recording`]. The caller scopes the
/// transcription-side teardown (stream, engine) to this target only, so
/// cancelling one session never touches another lane's live work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelTarget {
    /// Nothing was recording: the cancel tore down no capture session.
    None,
    /// The local (keyboard/signal) session was cancelled.
    Local,
    /// One companion lane's session was cancelled.
    Lane(u64),
}

/// v1.6.2 fleet pass 1: which cancel generation an Escape resolution bumps
/// - pure, so the cross-fire rule is pinnable without an AppHandle. A
/// lane-resolved cancel bumps ONLY that lane's generation; every other
/// resolution bumps the local machine's shared generation (the keyboard
/// user's own abort). No resolution may bump a lane it did not cancel.
pub(crate) fn cancel_bump_lane(target: CancelTarget) -> Option<u64> {
    match target {
        CancelTarget::Lane(lane) => Some(lane),
        _ => None,
    }
}

/// v1.6.2 W1-L2: one companion connection lane's recorder pair. Each
/// authenticated connection owns its recorder and its ring independently,
/// so one phone's session (start, stop, disconnect) cannot touch another
/// phone's capture. RAM budget per lane, documented once here:
///
/// * ring: 2 s at 16 kHz f32 = 32 000 samples x 4 B = 128 KiB
/// * VAD: one Silero ONNX session per recorder (the detector is stateful -
///   hangover/prefill - so recorders can never share one instance). The
///   silero_vad_v4.onnx asset is 1.8 MiB on disk plus the runtime session
///   workspace, so each concurrently OPEN lane recorder costs a few MiB.
///
/// Discipline that keeps the total bounded: a lane's recorder CLOSES at its
/// session's stop (same as the pre-v1.6.2 shared recorder did), and
/// pre-warming keeps at most ONE idle lane recorder alive - the most
/// recently connected phone's - so the steady-state idle cost is one
/// recorder no matter how many phones are paired. The number of
/// simultaneously RECORDING lanes is bounded by the phones actually
/// pressing, which is a human bound, not a protocol bound.
struct RemoteLane {
    recorder: AudioRecorder,
    source: RemoteAudioSource,
    /// True while this lane's recording session is live (between
    /// `try_start_remote_lane_recording` and its stop).
    recording: bool,
    /// The capture-generation snapshot this lane's recording started with;
    /// the lane's readiness watcher validates against it (per-lane, so one
    /// lane's stop never invalidates another lane's pending first-sample
    /// cue the way the global generation does for local sessions).
    generation: u64,
}

/// v1.6.2 multi-phone review pin: the LOCAL machine's contribution to the
/// shared recording mirror (`recording_active`). The active set is
/// {Recording, Stopping} - Stopping is the local stop pipeline's capture
/// tail (the release-buffer window plus the recorder stop, which still
/// holds the microphone and can still be cancelled), the exact membership
/// `set_state` derives. Every fold site that recomputes the mirror from
/// outside `set_state` (a lane's stop, the lane-recorder teardown, the
/// cancel lane arm) MUST route through this one predicate: the lane-stop
/// fold once inlined a Recording-only match, and with stop pipelines
/// running on their own spawned threads a phone lane's stop could land
/// inside the Mac session's Stopping tail and flip the shared mirror to
/// false while the Mac was still capturing (readers: the Escape gate, the
/// local-LLM keep-warm idle timer, the failed-start tray/overlay guard).
pub(crate) fn local_capture_active(state: &RecordingState) -> bool {
    matches!(
        state,
        RecordingState::Recording { .. } | RecordingState::Stopping
    )
}

/// v1.6.2 W1-L2 RAM discipline, pure so it is pinnable without the VAD
/// asset a real pre-warm loads: which open lane recorders a pre-warm of
/// `target` retires. Every idle recorder OTHER than the target goes (the
/// most-recent-connection rule: after the call at most one idle
/// pre-warmed recorder exists, the target's), while the target itself
/// stays whatever its state and a RECORDING lane always stays (its stop
/// pipeline owns the teardown; retiring it would discard a live
/// dictation's recorder out from under the session).
fn prewarm_retires(lanes: impl Iterator<Item = (u64, bool)>, target: u64) -> Vec<u64> {
    lanes
        .filter(|&(lane, recording)| lane != target && !recording)
        .map(|(lane, _)| lane)
        .collect()
}

/// KB-272 (v1.6.4): the routing core of
/// [`AudioRecordingManager::push_remote_chunk`], free so it is testable
/// with REAL lane recorders (the manager pins tauri::AppHandle and cannot
/// be constructed under tauri::test's MockRuntime; see the test-module
/// note). True means the chunk entered THIS lane's own ring; false means
/// the lane has no recorder (never pre-warmed, or its stop pipeline
/// already removed the entry) and the caller must count the chunk as
/// admitted-but-dropped, never swallow it. Takes only the lanes map -
/// never the recording-state lock - so phone audio ingestion can never
/// block on a Mac session transition.
fn route_remote_chunk(
    lanes: &Mutex<std::collections::HashMap<u64, RemoteLane>>,
    lane: u64,
    samples: &[f32],
) -> bool {
    // Fleet pass 2: poison-tolerant, like every companion-manager lock
    // (see companion/mod.rs's locking note). This guard runs on EVERY
    // phone audio frame on the connection task, and the map is poisonable
    // in practice (try_start_remote_lane_recording opens the CoreAudio
    // device and the VAD under it); a poisoned `.unwrap()` here panicked
    // every later push, killing all phone dictation until restart with a
    // stderr-only trace.
    let lanes = lanes.lock().unwrap_or_else(|e| e.into_inner());
    match lanes.get(&lane) {
        Some(entry) => {
            entry.source.push_chunk(samples);
            true
        }
        None => false,
    }
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

/// The apply decision for a forced "mute while recording", shared by every
/// guard `apply_mute` runs: the setting must be on, a capture path must be
/// active (local stream open OR a live recording), this session must not
/// have applied already (a duplicate/late apply would overwrite
/// `prev_muted` with our own forced-muted state and strand audio muted on
/// stop), and - the AUD-12 gate - the process-exit teardown must not have
/// begun. Once `RunEvent::Exit` starts restoring the mute, a readiness
/// watcher that wakes after the restore must no-op instead of re-stranding
/// the system OUTPUT muted past process death. The exit flag is loaded
/// INSIDE `apply_mute`'s `mute_state` critical section, so the watcher's
/// check-then-act window (generation check, then apply) cannot slip a
/// re-mute between the flag store and the restore: an apply completing
/// before the restore leaves `did_mute = true` so `remove_mute` restores
/// it, and an apply starting after the flag store sees the flag and
/// no-ops.
fn should_apply_forced_mute(
    mute_while_recording: bool,
    capture_active: bool,
    already_applied_this_session: bool,
    exit_teardown_started: bool,
) -> bool {
    mute_while_recording
        && capture_active
        && !already_applied_this_session
        && !exit_teardown_started
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
    stream_owner: Arc<AtomicU64>,
    owner: StreamOwner,
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

    // v1.6.2 W1-L2: the router feed is gated on stream ownership. The
    // process has ONE streaming engine; a recorder whose session does not
    // hold the token must never feed it (two speakers interleaved into one
    // transcript, or a late-starting local session polluting a draining
    // companion stream). The gate is a relaxed atomic load on the audio
    // path - identical cost to the router's own open check.
    let owner_token = stream_owner_token(owner);
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
                if stream_owner.load(Ordering::Relaxed) == owner_token {
                    router.feed(frame);
                }
            }
        });

    Ok(recorder)
}

/// v1.6.2 W1-L2: build the recorder for ONE companion connection lane. Same
/// construction as the local path ([`create_audio_recorder`]: same VAD
/// settings, same level callback, same router wiring), except the router
/// feed is gated on THIS lane holding the stream-ownership token and the
/// recorder is opened remote (its ring producer handed to the phone's
/// connection) rather than on a cpal device.
fn create_remote_lane_recorder(
    app_handle: &tauri::AppHandle,
    stream_router: Arc<StreamRouter>,
    stream_owner: Arc<AtomicU64>,
    lane: u64,
) -> Result<AudioRecorder, anyhow::Error> {
    let settings = get_settings(app_handle);
    create_audio_recorder(
        settings.vad_backend,
        app_handle,
        settings.selected_channel,
        stream_router,
        stream_owner,
        StreamOwner::Lane(lane),
    )
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
    /// v1.6.2 fleet pass 1: the PER-LANE cancel generations, the lane twin
    /// of `cancel_generation`. A stop pipeline gates on its own session's
    /// generation only, and an Escape bumps exactly the generation of the
    /// session it tore down (see [`AudioRecordingManager::cancel_recording`])
    /// - under the one global generation, cancelling one phone discarded
    /// another phone's in-flight stop pipeline (its captured audio included)
    /// whenever the two overlapped.
    lane_cancel_generations: Arc<Mutex<std::collections::HashMap<u64, u64>>>,
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
    /// AUD-12: set by `RunEvent::Exit` BEFORE its `remove_mute()` restore.
    /// `apply_mute` reads this inside its `mute_state` critical section, so
    /// the detached readiness watcher (actions.rs) cannot apply the forced
    /// mute after the exit restore has run and re-strand the system output
    /// muted past process death. Normal stop/cancel paths leave it false.
    exit_teardown: Arc<AtomicBool>,
    /// Resolution of a *named* microphone (selected or clamshell) to its cpal
    /// device, cached so on-demand recording starts skip the full device
    /// enumeration (~40-110ms). Keyed by the resolved name, so a settings
    /// change misses naturally; cleared when an open fails (device unplugged)
    /// so the retry re-enumerates. The system-default case is never cached -
    /// the recorder resolves the current default itself, cheaply.
    cached_device: Arc<Mutex<Option<(String, cpal::Device)>>>,
    /// v1.6.2 W1-L2: one recorder pair PER companion connection lane (see
    /// [`RemoteLane`] for the RAM budget and retirement rules). Replaces the
    /// single shared `remote_recorder`/`remote_source` pair, which forced
    /// every phone to share one ring and one session.
    remote_lanes: Arc<Mutex<std::collections::HashMap<u64, RemoteLane>>>,
    /// Lock-free mirror of "any lane is recording", so `set_state` (which
    /// holds the `state` mutex) can fold it into `recording_active` without
    /// ever taking the lanes lock (lock order: never state -> lanes).
    remote_lanes_recording: Arc<AtomicBool>,
    /// The lane that most recently began recording; the cancel path's tie
    /// break when several phones record at once (a local recording always
    /// wins; 0 means no lane has recorded since the last teardown).
    last_recording_lane: Arc<AtomicU64>,
    /// v1.6.2 W1-L2: the streaming-engine ownership token (see
    /// [`StreamOwner`]). Read per frame by every recorder's audio callback
    /// through [`stream_owner_token`].
    stream_owner: Arc<AtomicU64>,
    /// KB-010: a lid (clamshell) change that arrived while a local recording
    /// was live could not restart the stream (the guarded restart refuses to
    /// discard a live recording's samples), so the watcher parks the request
    /// here and the local stop and cancel pipelines perform it once the
    /// state returns to Idle.
    clamshell_reresolve_pending: Arc<AtomicBool>,
    /// KB-028 (v1.6.5): the companion teardown's lane-recorder close was
    /// SKIPPED while anything recorded (a mid-flight stop pipeline owns its
    /// lane's recorder), but a disable during a LOCAL recording had no lane
    /// pipeline coming - the skip was silent, nothing re-triggered the
    /// close after the local session ended, and every pre-warmed idle lane
    /// recorder (ring + VAD session) leaked until the next companion
    /// start/stop or app shutdown. The teardown now routes through
    /// [`Self::close_remote_stream_or_defer`], which parks the skipped
    /// close here; every session-end fold (local stop, local cancel, lane
    /// stop, lane cancel) runs [`Self::run_deferred_remote_close`] once
    /// NOTHING is recording anywhere.
    remote_close_pending: Arc<AtomicBool>,
    /// Factory for building recorders. Production always uses
    /// `create_audio_recorder`; tests inject a bare recorder so the manager
    /// logic can run without the Silero ONNX asset.
    recorder_factory: RecorderFactory,
}

type RecorderFactory =
    Arc<dyn Fn(&tauri::AppHandle) -> Result<AudioRecorder, anyhow::Error> + Send + Sync>;

fn default_recorder_factory(
    stream_router: Arc<StreamRouter>,
    stream_owner: Arc<AtomicU64>,
) -> impl Fn(&tauri::AppHandle) -> Result<AudioRecorder, anyhow::Error> + Send + Sync {
    move |app| {
        let settings = get_settings(app);
        create_audio_recorder(
            settings.vad_backend,
            app,
            settings.selected_channel,
            Arc::clone(&stream_router),
            Arc::clone(&stream_owner),
            StreamOwner::Local,
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

/// KB-294 (v1.6.5): the release-buffer window ONE lane's stop pays. The
/// local stop's tail exists to keep capturing while the user's finger is
/// still on the mic; a lane's tail exists to keep capturing TRAILING AUDIO
/// FROM THE PHONE - so when the lane's route (its connection) is already
/// gone, no chunk can ever arrive again and the sleep is pure added
/// latency on top of a mid-dictation disconnect. Pure so the leak-shape
/// is pinnable without a live connection: route gone means NO window,
/// regardless of the settings; route live keeps
/// [`effective_release_buffer_ms`] exactly.
fn lane_release_buffer_ms(
    route_live: bool,
    extra_ms: u64,
    tail_ms: u64,
    stream_active: bool,
) -> u64 {
    if route_live {
        effective_release_buffer_ms(extra_ms, tail_ms, stream_active)
    } else {
        0
    }
}

/// KB-294: whether THIS lane's audio route is still live. A lane's only
/// push route is its connection's socket (`push_remote_chunk` keys on the
/// lane id, and lane ids are never reused, so a reconnect cannot revive a
/// dead lane's route). The disconnect edge unregisters the connection
/// BEFORE queueing the forced finalize, so by the time the stop pipeline
/// reaches the release-buffer decision the registration read is already
/// false - the exact mid-dictation-disconnect shape whose tail must not
/// be paid. No companion manager state (tests, pre-init teardowns) also
/// reads as route-gone: nothing can push on a lane without one.
fn lane_route_live(app: &tauri::AppHandle, lane: u64) -> bool {
    app.try_state::<std::sync::Arc<crate::companion::CompanionManager>>()
        .map(|manager| manager.connection_device(lane).is_some())
        .unwrap_or(false)
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
        let stream_owner: Arc<AtomicU64> =
            Arc::new(AtomicU64::new(stream_owner_token(StreamOwner::None)));

        let recorder_factory: RecorderFactory = Arc::new(default_recorder_factory(
            Arc::clone(&stream_router),
            Arc::clone(&stream_owner),
        ));

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
            lane_cancel_generations: Arc::new(Mutex::new(std::collections::HashMap::new())),
            stream_router,
            recording_active: Arc::new(AtomicBool::new(false)),
            capture_generation: Arc::new(AtomicU64::new(0)),
            exit_teardown: Arc::new(AtomicBool::new(false)),
            cached_device: Arc::new(Mutex::new(None)),
            remote_lanes: Arc::new(Mutex::new(std::collections::HashMap::new())),
            remote_lanes_recording: Arc::new(AtomicBool::new(false)),
            last_recording_lane: Arc::new(AtomicU64::new(0)),
            stream_owner,
            clamshell_reresolve_pending: Arc::new(AtomicBool::new(false)),
            remote_close_pending: Arc::new(AtomicBool::new(false)),
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
        // Every gate in one shared decision (see should_apply_forced_mute):
        // the duplicate-apply veto (a late apply would overwrite prev_muted
        // with our own forced-muted state and strand audio muted on stop),
        // the capture-active guard, and the AUD-12 exit gate. The exit flag
        // is loaded HERE, inside the mute_state critical section, so the
        // readiness watcher's check-then-act window cannot re-mute after the
        // exit restore: an apply holding this lock finishes before
        // remove_mute runs (did_mute = true -> restored), and an apply
        // starting later sees the flag and no-ops.
        let exit_teardown_started = self.exit_teardown.load(Ordering::SeqCst);
        if !should_apply_forced_mute(
            settings.mute_while_recording,
            *is_open || self.is_recording(),
            mute_guard.did_mute,
            exit_teardown_started,
        ) {
            return;
        }
        mute_guard.prev_muted = get_mute();
        set_mute(true);
        mute_guard.did_mute = true;
        debug!("Mute applied (prev_muted={:?})", mute_guard.prev_muted);
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
                // KB-237: a companion lane session keeps the state Idle, so
                // the close also runs mid phone session (the lane recorders
                // are independent of the local stream); only a live LOCAL
                // recording defers to its stop pipeline.
                if mode_flip_may_close_local_stream(&self.state.lock().unwrap()) {
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
    /// its active-set membership decided here, once. v1.6.2 W1-L2: the
    /// mirror folds in the per-lane recording count through its own
    /// lock-free atomic, so this never takes the lanes lock while holding
    /// the state lock (lock order: lanes methods may take state, never the
    /// reverse).
    fn set_state(&self, guard: &mut RecordingState, new_state: RecordingState) {
        *guard = new_state;
        let local_active = local_capture_active(guard);
        let lanes_active = self.remote_lanes_recording.load(Ordering::SeqCst);
        self.recording_active
            .store(local_active || lanes_active, Ordering::SeqCst);
    }

    /// Start a LOCAL recording session. The single-session state machine is
    /// shared by the two keyboard bindings exactly as before ("Already
    /// recording" when one is live); companion lanes address their own
    /// recorders through [`Self::try_start_remote_lane_recording`] and never
    /// pass through here. `Local` is byte-for-byte the old
    /// `try_start_recording` path.
    pub fn try_start_recording_for(
        &self,
        binding_id: &str,
        vad_policy: VadPolicy,
    ) -> Result<RecordingReadiness, String> {
        let mut state = self.state.lock().unwrap();

        if let RecordingState::Idle = *state {
            // Cancel any pending lazy close (no-op in always-on mode, where
            // closes are never scheduled).
            self.close_generation.fetch_add(1, Ordering::SeqCst);
            // Opens the stream in on-demand mode. In always-on mode the
            // stream is normally already open and this is a cheap aliveness
            // check - but if the capture worker died (device disconnect),
            // it rebuilds the stream instead of leaving every subsequent
            // start wedged on "Recorder not available".
            if let Err(e) = self.start_microphone_stream() {
                let msg = format!("{e}");
                error!("Failed to open microphone stream: {msg}");
                return Err(msg);
            }

            if let Some(rec) = self.recorder.lock().unwrap().as_ref() {
                match rec.start(vad_policy) {
                    Ok(receiver) => {
                        let generation = self.capture_generation.fetch_add(1, Ordering::AcqRel) + 1;
                        *self.is_recording.lock().unwrap() = true;
                        self.set_state(
                            &mut state,
                            RecordingState::Recording {
                                binding_id: binding_id.to_string(),
                            },
                        );
                        debug!("Recording requested for binding {binding_id} (Local)");
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

    // ------------------------------------------------------------------
    // v1.6.2 W1-L2: per-lane remote sessions. Each companion connection
    // owns its recorder and its ring; one phone's start/stop/disconnect
    // cannot touch another phone's capture (KB-110/KB-111 die by
    // construction).
    // ------------------------------------------------------------------

    /// Ensure one lane's recorder exists and is open, reusing a healthy one
    /// (pre-warmed on the connection's hello). Rebuilds a dead worker like
    /// `start_microphone_stream` does for a disconnected mic. Lock-free
    /// except the lanes map itself; never takes the `state` lock.
    fn ensure_remote_lane(&self, lane: u64) -> Result<(), anyhow::Error> {
        let needs_reopen = {
            let lanes = self.remote_lanes.lock().unwrap_or_else(|e| e.into_inner());
            lanes.get(&lane).is_none_or(|l| l.recorder.needs_reopen())
        };
        if !needs_reopen {
            return Ok(());
        }

        let mut lanes = self.remote_lanes.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(old) = lanes.get_mut(&lane) {
            let _ = old.recorder.close();
        }
        let mut rec = create_remote_lane_recorder(
            &self.app_handle,
            Arc::clone(&self.stream_router),
            Arc::clone(&self.stream_owner),
            lane,
        )?;
        let source = rec
            .open_remote()
            .map_err(|e| anyhow::anyhow!("Failed to open remote capture source: {}", e))?;
        lanes.insert(
            lane,
            RemoteLane {
                recorder: rec,
                source,
                recording: false,
                generation: 0,
            },
        );
        Ok(())
    }

    /// Start ONE lane's recording session. Independent of every other lane
    /// and of the local machine: a second phone may start while the first
    /// still records (the pre-v1.6.2 "Already recording" cross-phone
    /// refusal is gone); only THIS lane's own live session refuses a double
    /// start.
    pub fn try_start_remote_lane_recording(
        &self,
        lane: u64,
        binding_id: &str,
        vad_policy: VadPolicy,
    ) -> Result<RecordingReadiness, String> {
        if let Err(e) = self.ensure_remote_lane(lane) {
            let msg = format!("{e}");
            error!("Failed to open remote capture source for lane {lane}: {msg}");
            return Err(msg);
        }
        let mut lanes = self.remote_lanes.lock().unwrap_or_else(|e| e.into_inner());
        let entry = lanes
            .get_mut(&lane)
            .ok_or_else(|| "Recorder not available".to_string())?;
        if entry.recording {
            return Err("Already recording".to_string());
        }
        match entry.recorder.start(vad_policy) {
            Ok(receiver) => {
                let generation = self.capture_generation.fetch_add(1, Ordering::AcqRel) + 1;
                entry.recording = true;
                entry.generation = generation;
                // Publish the lane mirrors, then fold the shared mirror with
                // the LOCAL machine's activity (state lock taken only AFTER
                // the lanes lock is released - see the lock-order note on
                // set_state).
                self.remote_lanes_recording.store(true, Ordering::SeqCst);
                self.last_recording_lane.store(lane, Ordering::SeqCst);
                drop(lanes);
                self.recording_active.store(true, Ordering::SeqCst);
                debug!("Recording requested for binding {binding_id} (lane {lane})");
                Ok(RecordingReadiness {
                    receiver,
                    generation,
                })
            }
            Err(error) => Err(format!("Failed to start recorder: {error}")),
        }
    }

    /// Stop ONE lane's recording session and return its captured samples.
    /// The per-lane twin of [`Self::stop_recording`]: same release-buffer
    /// window, same stop-pause handshake, same short-buffer padding - but
    /// scoped to this lane's recorder, and closing it (the RAM budget rule:
    /// no idle lane recorder outlives its session). Other lanes and the
    /// local machine are untouched.
    pub fn stop_remote_lane_recording(
        &self,
        lane: u64,
        cancel_generation: u64,
    ) -> Option<Vec<f32>> {
        // The entry removal below is this lane's readiness invalidation
        // (per-lane generation: another lane's watchers never noticed).
        let mut lanes = self.remote_lanes.lock().unwrap_or_else(|e| e.into_inner());
        // No recorder for this lane: nothing was recording on it.
        let mut entry = lanes.remove(&lane)?;
        let was_recording = entry.recording;
        entry.recording = false;
        let still_recording = lanes.values().any(|l| l.recording);
        self.remote_lanes_recording
            .store(still_recording, Ordering::SeqCst);
        drop(lanes);

        let samples = if was_recording {
            // Optionally keep recording for a bit longer to capture trailing
            // audio. The predicate is STREAM-ACTIVE, not the VAD policy
            // (see stop_recording). KB-294: the tail is skipped entirely
            // when the lane's route is already gone - a mid-dictation
            // disconnect unregisters the connection before queueing the
            // forced finalize, so trailing audio cannot arrive and the
            // sleep was pure latency on the disconnect's recovery path.
            let settings = get_settings(&self.app_handle);
            let stream_active = self.stream_router.is_open();
            let route_live = lane_route_live(&self.app_handle, lane);
            if !route_live {
                debug!(
                    "lane {lane} route gone before its stop: skipping the \
                     release-buffer tail (KB-294)"
                );
            }
            let buffer_ms = lane_release_buffer_ms(
                route_live,
                settings.extra_recording_buffer_ms,
                settings.streaming_release_tail_ms,
                stream_active,
            );
            if buffer_ms > 0 {
                debug!(
                    "Extra recording buffer: sleeping {}ms before stopping lane {lane}",
                    buffer_ms
                );
                let started = Instant::now();
                let buffer = Duration::from_millis(buffer_ms);
                while started.elapsed() < buffer {
                    if self.was_cancelled_since(Some(lane), cancel_generation) {
                        debug!("Recording stop cancelled during extra buffer");
                        break;
                    }
                    let remaining = buffer.saturating_sub(started.elapsed());
                    std::thread::sleep(remaining.min(Duration::from_millis(25)));
                }
            }
            // Fleet pass: this entry is out of the map, so no push can
            // reach its ring any more (the map lookup is the only route
            // in, and the server's admission already refuses the
            // not-live session) - nothing exists to acknowledge the
            // stop-pause handshake, so every healthy phone stop paid the
            // full 2 s PAUSE_ACK_TIMEOUT inside stop() before
            // transcription even began. Closing the SOURCE first marks
            // the producer gone, which the consumer reads as an immediate
            // acknowledgement: fast return, the ring's samples intact.
            entry.source.close();
            match entry.recorder.stop() {
                Ok(buf) => buf,
                Err(e) => {
                    error!("stop() failed for lane {lane}: {e}");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        // The phone session is over: close the lane's recorder so a stale
        // socket cannot keep pushing into a dead ring, and release the
        // recorder's RAM (ring + VAD session) back.
        let _ = entry.recorder.close();

        // Fold the shared recording mirror with the local machine's state.
        // Multi-phone review pin: through local_capture_active, so the
        // Mac session's own Stopping tail (this fold runs on the stop
        // pipeline's spawned thread and CAN land inside it) keeps the
        // mirror honest exactly as set_state does. Fleet pass 3: the
        // release-buffer sleep above can span another lane's whole start,
        // and the still_recording snapshot taken BEFORE it would clobber
        // that start's mirror to false while it records - so the fold
        // re-loads remote_lanes_recording fresh, never the stale snapshot.
        let local_active = local_capture_active(&*self.state.lock().unwrap());
        let lanes_active = self.remote_lanes_recording.load(Ordering::SeqCst);
        self.recording_active
            .store(local_active || lanes_active, Ordering::SeqCst);

        // KB-028: this lane's end may be the last session anywhere - run a
        // companion teardown's parked lane-recorder close if so.
        self.run_deferred_remote_close();

        if self.was_cancelled_since(Some(lane), cancel_generation) {
            debug!("Recording stop cancelled; discarding captured samples");
            return None;
        }

        // Pad if very short
        let s_len = samples.len();
        if s_len < WHISPER_SAMPLE_RATE && s_len > 0 {
            let mut padded = samples;
            padded.resize(WHISPER_SAMPLE_RATE * 5 / 4, 0.0);
            Some(padded)
        } else {
            Some(samples)
        }
    }

    /// Whether one lane's recording session is live (the server's admission
    /// truth and disconnect-finalize decision).
    pub fn lane_is_recording(&self, lane: u64) -> bool {
        self.remote_lanes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&lane)
            .is_some_and(|l| l.recording)
    }

    /// Whether a lane's readiness work (first-sample cue, chime, mute
    /// apply) is still current: the SAME recording that handed out the
    /// generation must still be live. Per-lane by construction - another
    /// lane's stop cannot invalidate this lane's pending cue, and a
    /// restarted session on the same lane gets a fresh generation.
    pub fn lane_readiness_current(&self, lane: u64, generation: u64) -> bool {
        self.remote_lanes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&lane)
            .is_some_and(|l| l.recording && l.generation == generation)
    }

    /// Push one audio chunk into ONE lane's ring (the server routes each
    /// connection's binary frames here). False when the lane has no
    /// recorder (never pre-warmed, or its session already closed it).
    /// KB-272: delegates to [`route_remote_chunk`], the free core that is
    /// behaviorally tested with real recorders (the manager itself pins
    /// tauri::AppHandle and cannot be constructed under the mock runtime;
    /// see the test-module note below).
    pub fn push_remote_chunk(&self, lane: u64, samples: &[f32]) -> bool {
        route_remote_chunk(&self.remote_lanes, lane, samples)
    }

    /// Pre-warm ONE lane's recorder (loads the VAD, opens the ring) while
    /// idle so that phone's press pays no recorder construction cost.
    /// v1.6.2 RAM discipline: pre-warming retires every OTHER idle lane
    /// recorder first, so at most one idle pre-warmed recorder exists at a
    /// time - the most recently connected phone's (W1-L2).
    pub fn prewarm_remote_lane(&self, lane: u64) -> Result<(), anyhow::Error> {
        {
            let mut lanes = self.remote_lanes.lock().unwrap_or_else(|e| e.into_inner());
            let retired =
                prewarm_retires(lanes.iter().map(|(&l, entry)| (l, entry.recording)), lane);
            for retired_lane in retired {
                if let Some(mut entry) = lanes.remove(&retired_lane) {
                    debug!(
                        "companion: retiring idle lane {retired_lane} recorder \
                         (pre-warm keeps only the most recent connection)"
                    );
                    let _ = entry.recorder.close();
                }
            }
        }
        self.ensure_remote_lane(lane)
    }

    /// Close ONE lane's idle recorder (its connection went away without a
    /// session, or after its session's stop already closed it). A lane
    /// still recording is left alone - its stop pipeline owns the teardown.
    pub fn close_remote_lane(&self, lane: u64) {
        let mut lanes = self.remote_lanes.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = lanes.get(&lane) {
            if entry.recording {
                return;
            }
        }
        if let Some(mut entry) = lanes.remove(&lane) {
            let _ = entry.recorder.close();
        }
    }

    /// Close every lane recorder (companion feature disabled, server stop,
    /// or app shutdown). A lane still recording is stopped first - its
    /// samples are discarded on this path, which only the
    /// no-more-audio-source teardowns take (the graceful stop finalizes
    /// each live session BEFORE calling here).
    pub fn close_remote_stream(&self) {
        // Fleet pass: take the whole map under the lock, then stop and
        // close OUTSIDE it. stop()'s pause handshake is acknowledged
        // through push_remote_chunk / ack_remote_pause, which need this
        // same lock - stopping under it serialized the full 2 s
        // PAUSE_ACK_TIMEOUT per recording lane with every phone's pushes
        // locked out.
        let drained: Vec<_> = {
            let mut lanes = self.remote_lanes.lock().unwrap_or_else(|e| e.into_inner());
            lanes.drain().collect()
        };
        for (_, mut entry) in drained {
            if entry.recording {
                // The teardown discards this lane's samples and its map
                // entry is already gone (no push route left): close the
                // source first so the stop cannot wait out the pause
                // timeout - without it a force teardown spent 2 s per
                // recording lane.
                entry.source.close();
                let _ = entry.recorder.stop();
            }
            let _ = entry.recorder.close();
        }
        // Recompute under the lock (not a blind false): a lane start that
        // committed after the drain must keep its truth, and the store
        // shares the start's critical section so the two cannot interleave.
        {
            let lanes = self.remote_lanes.lock().unwrap_or_else(|e| e.into_inner());
            let still = lanes.values().any(|l| l.recording);
            self.remote_lanes_recording.store(still, Ordering::SeqCst);
        }
        self.last_recording_lane.store(0, Ordering::SeqCst);
        // Fold the shared mirror with the local machine's state and the
        // lanes' CURRENT truth: a lane start can commit between the drain
        // above and this fold, and folding from the drain's result alone
        // (implicitly "no lane records") would clobber that start's mirror
        // to false mid-recording. Same fresh-atomic discipline as set_state.
        let local_active = local_capture_active(&*self.state.lock().unwrap());
        let lanes_active = self.remote_lanes_recording.load(Ordering::SeqCst);
        self.recording_active
            .store(local_active || lanes_active, Ordering::SeqCst);
    }

    /// KB-028 (v1.6.5): close every lane recorder now, or - while any
    /// session (local or lane) is live - park the close in
    /// `remote_close_pending` for the moment the LAST one ends. The
    /// companion server-stop path calls this instead of a bare
    /// [`Self::close_remote_stream`]: its skip-while-recording guard used
    /// to leak every pre-warmed idle recorder whenever a LOCAL recording
    /// was the reason for the skip, because no lane pipeline existed to
    /// close them and nothing re-triggered the close after the local
    /// session ended. The deferred run never discards a live lane's
    /// dictation: it waits for `is_recording()` to read false, and every
    /// lane pipeline removes its own map entry BEFORE its release-buffer
    /// window, so a drain then finds only idle (or pipeline-less) lanes.
    pub fn close_remote_stream_or_defer(&self) {
        if self.is_recording() {
            debug!(
                "companion: lane-recorder close deferred while a recording \
                 is live (KB-028); the last session end runs it"
            );
            self.remote_close_pending.store(true, Ordering::SeqCst);
        } else {
            self.close_remote_stream();
        }
    }

    /// KB-028 (v1.6.5): run a parked lane-recorder close once NOTHING is
    /// recording anywhere. A lane still mid-pipeline owns its recorder
    /// (its dictation must survive the teardown), so this is a no-op while
    /// any session is live and the LAST session-end fold re-checks here. A
    /// lane start can never race this clear-and-close window: the flag is
    /// only parked while the companion server is stopped, and lane starts
    /// require a live server connection.
    fn run_deferred_remote_close(&self) {
        if !self.remote_close_pending.load(Ordering::SeqCst) || self.is_recording() {
            return;
        }
        self.remote_close_pending.store(false, Ordering::SeqCst);
        debug!("companion: running the deferred lane-recorder close (KB-028)");
        self.close_remote_stream();
    }

    /// v1.6.2 W1-L2: claim the streaming-engine token for one session.
    /// Called ONLY from the coordinator thread's Start effect (the single
    /// serialized instant the start commits), after confirming the engine
    /// is actually free, so no two sessions can ever both believe they own
    /// the stream.
    pub fn set_stream_owner(&self, owner: StreamOwner) {
        self.stream_owner
            .store(stream_owner_token(owner), Ordering::Release);
    }

    /// The current streaming-engine owner.
    pub fn stream_owner(&self) -> StreamOwner {
        stream_owner_from_token(self.stream_owner.load(Ordering::Acquire))
    }

    /// Release the token, but only if `owner` still holds it (the stop
    /// pipeline of a session that lost the token to a teardown must not
    /// clear a newer owner's claim).
    pub fn clear_stream_owner_if(&self, owner: StreamOwner) {
        let token = stream_owner_token(owner);
        self.stream_owner
            .compare_exchange(token, 0, Ordering::AcqRel, Ordering::Acquire)
            .ok();
    }

    /// Replace the VAD implementation while idle. If the microphone stream is
    /// currently warm (always-on or lazy-close mode), reopen it with the new
    /// detector before reporting success. A failed reopen restores the previous
    /// recorder so the persisted setting can remain unchanged.
    pub fn update_vad_backend(&self, backend: VadBackend) -> Result<(), anyhow::Error> {
        let state = self.state.lock().unwrap();
        if capture_restart_forbidden(&state) || self.remote_lanes_recording.load(Ordering::SeqCst) {
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
            Arc::clone(&self.stream_owner),
            StreamOwner::Local,
        )?;
        let was_open = *self.is_open.lock().unwrap();

        // v1.6.2 W1-L2: idle lane recorders were built under the OLD
        // backend; retire them so the next press rebuilds each lane with
        // the new one (a recording lane would have blocked us above).
        {
            let mut lanes = self.remote_lanes.lock().unwrap_or_else(|e| e.into_inner());
            let idle: Vec<u64> = lanes
                .iter()
                .filter(|(_, entry)| !entry.recording)
                .map(|(l, _)| *l)
                .collect();
            for lane in idle {
                if let Some(mut entry) = lanes.remove(&lane) {
                    let _ = entry.recorder.close();
                }
            }
        }

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
                // KB-196: restoring the recorder's in-memory channel alone
                // leaves the stream it describes still cold - AlwaysOn's
                // warm capture then stays down until the next session opens
                // one. Retry the reopen on the previous channel (the same
                // rollback shape as the device switch above and the VAD
                // backend swap) and still surface the original error.
                if let Err(rollback_error) = self.start_microphone_stream() {
                    error!(
                        "Failed to restore microphone stream after failed channel switch: {rollback_error}"
                    );
                }
                return Err(error);
            }
        }
        drop(state);
        Ok(())
    }

    /* ---------- KB-010: lid (clamshell) driven re-resolution ---------- */

    /// KB-010: re-resolve the desired microphone after the lid state
    /// changed (the watcher in helpers/clamshell.rs) and restart the warm
    /// stream on the newly desired device. Mirrors the guarded restart of
    /// [`Self::update_selected_channel`]: a live local recording is never
    /// restarted underneath (its samples would be discarded); the request
    /// is parked in `clamshell_reresolve_pending` and the stop or cancel
    /// pipeline performs it once the state is Idle again. A cold on-demand
    /// stream needs nothing: the next session open resolves the device
    /// fresh through the same `desired_microphone` path.
    pub fn handle_clamshell_change(&self) {
        self.restart_open_stream_on_new_device();
    }

    /// Perform a clamshell re-resolve that a live recording deferred, once
    /// the state is Idle again. Only an always-on stream needs it: an
    /// on-demand stream has just been closed by its stop pipeline, and its
    /// next open resolves the (possibly flipped) lid state fresh.
    fn run_deferred_clamshell_reresolve(&self) {
        if !self
            .clamshell_reresolve_pending
            .swap(false, Ordering::SeqCst)
        {
            return;
        }
        if !matches!(*self.mode.lock().unwrap(), MicrophoneMode::AlwaysOn) {
            debug!(
                "Deferred clamshell re-resolve dropped: the stream is on-demand \
                 and the next session open resolves the lid state fresh"
            );
            return;
        }
        debug!("Performing the clamshell re-resolve deferred by a recording");
        self.restart_open_stream_on_new_device();
    }

    /// Stop an open stream and reopen it, which re-runs
    /// `desired_microphone` (and therefore the clamshell probe) against the
    /// current environment. Best-effort by design, like the always-on
    /// startup open: a failed reopen is logged and the next recording start
    /// re-attempts it where the user can act on the error.
    ///
    /// Fleet pass 3 (the KB-010 lock scope): the recording-state check and
    /// the restart run under ONE hold of the state lock, exactly like the
    /// device, channel, and clamshell-preference switches - and for the
    /// same reason: a hotkey start that committed between the old Idle
    /// sample (taken under a lock dropped before this ran) and the
    /// restart's force-stop had its fresh recorder's ring destroyed and
    /// the dictation silently discarded. Holding the lock across the
    /// CoreAudio stop/start is deadlock-free (update_clamshell_microphone
    /// already holds it across the same pair; neither helper takes it).
    /// A live recording defers instead of restarting underneath.
    ///
    /// Fleet pass 3 (the KB-010 mode blindness): only the ALWAYS-ON stream
    /// reopens. An on-demand stream sits open only inside its lazy-close
    /// window; the restart's close_generation bump already cancels that
    /// timer, so reopening it here left the mic hot with no close ever
    /// armed (the stranded-hot-mic class KB-237 documents). The on-demand
    /// stream closes now and the next session open resolves the flipped
    /// lid fresh through the same desired_microphone path.
    fn restart_open_stream_on_new_device(&self) {
        // KB-273 (v1.6.4): a lid flip can only change the desired
        // microphone when a clamshell microphone is configured; with none
        // configured, desired_microphone resolves identically for an open
        // and a closed lid, so the stop/start (and the state-lock hold
        // across it, which blocks every start/stop pipeline for the length
        // of the CoreAudio reopen - the UI-freeze class) served nothing.
        // Gate BEFORE the lock: the common no-clamshell-mic flip now costs
        // nothing at all.
        if get_settings(&self.app_handle)
            .clamshell_microphone
            .as_deref()
            .is_none()
        {
            debug!(
                "Capture environment change skipped: no clamshell microphone is \
                 configured, so the lid state cannot change the desired microphone"
            );
            return;
        }
        let state = self.state.lock().unwrap();
        if capture_restart_forbidden(&state) {
            drop(state);
            self.clamshell_reresolve_pending
                .store(true, Ordering::SeqCst);
            debug!(
                "Capture environment change deferred: a local recording is \
                 live; the stop pipeline will re-resolve the microphone"
            );
            return;
        }
        let was_open = *self.is_open.lock().unwrap();
        if !was_open {
            return;
        }
        self.invalidate_device_cache();
        self.close_generation.fetch_add(1, Ordering::SeqCst);
        self.stop_microphone_stream();
        if !matches!(*self.mode.lock().unwrap(), MicrophoneMode::AlwaysOn) {
            debug!(
                "Capture environment change closed the on-demand stream; the \
                 next session open resolves the lid state fresh"
            );
            return;
        }
        if let Err(e) = self.start_microphone_stream() {
            error!(
                "Failed to reopen the microphone after a capture environment \
                 change (lid state or clamshell preference): {e:#}"
            );
        }
    }

    /// KB-010: switch the clamshell (lid-closed) microphone preference.
    /// Same contract as [`Self::update_selected_device`]: rejected outright
    /// while a local recording is live (nothing changes, not the live
    /// capture and not the persisted preference); on acceptance the
    /// preference is persisted here so the restart below resolves the new
    /// device from disk, an open stream restarts on it, and a failed
    /// restart rolls the preference back (the settings UI rolls its
    /// dropdown back on this error, and the store must agree with it).
    pub fn update_clamshell_microphone(
        &self,
        clamshell_microphone: Option<String>,
    ) -> Result<(), anyhow::Error> {
        // Serialize against recording start/stop for the whole switch, like
        // the device and channel changes do.
        let state = self.state.lock().unwrap();
        if capture_restart_forbidden(&state) {
            return Err(anyhow::anyhow!(
                "Cannot change the clamshell microphone while recording"
            ));
        }

        let mut settings = get_settings(&self.app_handle);
        let previous_device = settings.clamshell_microphone.clone();
        if settings.clamshell_microphone != clamshell_microphone {
            settings.clamshell_microphone = clamshell_microphone;
            write_settings(&self.app_handle, settings);
            // The same convergence signal the device switch emits, so every
            // open surface re-reads the store.
            let _ = self.app_handle.emit(
                "settings-changed",
                serde_json::json!({
                    "setting": "clamshell_microphone"
                }),
            );
        }

        let was_open = *self.is_open.lock().unwrap();
        if was_open {
            self.invalidate_device_cache();
            self.close_generation.fetch_add(1, Ordering::SeqCst);
            self.stop_microphone_stream();
            if let Err(restart_error) = self.start_microphone_stream() {
                // The reopen resolves its device from settings, so undo the
                // write above and bring the previous configuration's stream
                // back (the same rollback shape as the device switch).
                let mut settings = get_settings(&self.app_handle);
                if settings.clamshell_microphone != previous_device {
                    settings.clamshell_microphone = previous_device;
                    write_settings(&self.app_handle, settings);
                    let _ = self.app_handle.emit(
                        "settings-changed",
                        serde_json::json!({
                            "setting": "clamshell_microphone"
                        }),
                    );
                }
                if let Err(rollback_error) = self.start_microphone_stream() {
                    error!(
                        "Failed to restore the microphone stream after a failed \
                         clamshell microphone switch: {rollback_error}"
                    );
                }
                return Err(restart_error);
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

    /// AUD-12: mark the process-exit teardown as begun. Called by the
    /// `RunEvent::Exit` handler BEFORE `remove_mute()`, symmetric with how
    /// stop and cancel invalidate readiness first: the detached readiness
    /// watcher's generation check then rejects a late wake, and - the real
    /// gate, because that check is only advisory - `apply_mute` loads the
    /// flag inside its `mute_state` critical section and no-ops, so the
    /// watcher can never re-mute the system output after the exit restore.
    pub fn begin_exit_teardown(&self) {
        self.exit_teardown.store(true, Ordering::SeqCst);
        self.invalidate_recording_readiness();
    }

    pub fn is_recording_readiness_current(&self, generation: u64) -> bool {
        self.capture_generation.load(Ordering::Acquire) == generation
    }

    /// v1.6.2 fleet pass 1: the cancel generation ONE session's stop
    /// pipeline gates on - `None` is the local machine's shared generation,
    /// `Some(lane)` that lane's own. Capture it at pipeline start and pass
    /// it back through [`Self::was_cancelled_since`]; only a cancel that
    /// resolved to THIS session bumps it.
    pub fn cancel_generation(&self, lane: Option<u64>) -> u64 {
        match lane {
            Some(lane) => self
                .lane_cancel_generations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&lane)
                .copied()
                .unwrap_or(0),
            None => self.cancel_generation.load(Ordering::Acquire),
        }
    }

    /// Whether the session identified by `lane` (None = the local machine)
    /// was cancelled since `generation` was captured. Same lane routing as
    /// [`Self::cancel_generation`].
    pub fn was_cancelled_since(&self, lane: Option<u64>, generation: u64) -> bool {
        self.cancel_generation(lane) != generation
    }

    /// Bump exactly one session's cancel generation: the cancel teardown's
    /// own target, never any other lane's (the cross-fire the single global
    /// bump allowed).
    fn bump_cancel_generation(&self, lane: Option<u64>) {
        match lane {
            Some(lane) => {
                let mut lanes = self
                    .lane_cancel_generations
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let generation = lanes.entry(lane).or_insert(0);
                *generation += 1;
            }
            None => {
                self.cancel_generation.fetch_add(1, Ordering::AcqRel);
            }
        }
    }

    /// v1.6.2 fleet pass 2: bump the cancel generations of exactly these
    /// lanes - the abort-all resolution an Escape takes when nothing is
    /// recording anymore and only stop pipelines remain (their gates, and
    /// the transcription-error arm's cancelled check, then classify the
    /// abort cleanly instead of pasting or reporting a spurious failure).
    /// A lane no pipeline ever captured is a harmless no-op bump.
    pub fn bump_lane_cancel_generations(&self, lanes: &[u64]) {
        if lanes.is_empty() {
            return;
        }
        let mut map = self
            .lane_cancel_generations
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for lane in lanes {
            *map.entry(*lane).or_insert(0) += 1;
        }
    }

    /// KB-270 (v1.6.4): drop one dead lane's cancel-generation entry. The
    /// map grew by an entry per lane id forever (ids are never reused), so
    /// a flapping phone grew it monotonically. Safe only when the lane has
    /// no pipeline in flight: a pipeline holding a captured generation for
    /// this lane would read the default 0 after the removal and classify
    /// its own drain as a cancellation, discarding the dictation - the
    /// coordinator's retirement rule (lane_machine_retirable) calls this
    /// only for a fully drained machine whose connection is gone.
    /// Fleet pass 2: poison-tolerant, like route_remote_chunk's guard -
    /// this runs on the COORDINATOR thread inside retire_dead_lane, and a
    /// poisoned `.unwrap()` here would end that thread permanently (its
    /// catch_unwind logs once and exits, no restart), extending a poison's
    /// blast radius from one pipeline to keyboard plus all phone
    /// dictation.
    pub fn drop_lane_cancel_generation(&self, lane: u64) {
        self.lane_cancel_generations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&lane);
    }

    /// Stop the LOCAL recording session and return its captured samples.
    /// v1.6.2 W1-L2: companion lane sessions stopped through
    /// [`Self::stop_remote_lane_recording`] never pass through here, so a
    /// phone session's stop cannot end (or steal the buffer of) a keyboard
    /// session, and vice versa.
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
                        if self.was_cancelled_since(None, cancel_generation) {
                            debug!("Recording stop cancelled during extra buffer");
                            break;
                        }
                        let remaining = buffer.saturating_sub(started.elapsed());
                        std::thread::sleep(remaining.min(Duration::from_millis(25)));
                    }
                }

                let samples = if let Some(rec) = self.recorder.lock().unwrap().as_ref() {
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

                // In on-demand mode, close the mic (lazily if the setting
                // is enabled)
                if matches!(*self.mode.lock().unwrap(), MicrophoneMode::OnDemand) {
                    if get_settings(&self.app_handle).lazy_stream_close {
                        self.schedule_lazy_close();
                    } else {
                        self.stop_microphone_stream();
                    }
                }

                // KB-010: a lid change that arrived mid-recording re-resolves
                // the always-on stream now that the state is Idle.
                self.run_deferred_clamshell_reresolve();

                // KB-028: a companion disable that arrived mid-recording
                // parked its lane-recorder close; the local session's end
                // (this fold) is what re-triggers it.
                self.run_deferred_remote_close();

                if self.was_cancelled_since(None, cancel_generation) {
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

    /// Whether the LOCAL machine's own session is live (Recording or
    /// Stopping) WITHOUT the per-lane fold. v1.6.2 fleet pass 2: the
    /// coordinator's start-success probe for a KEYBOARD start must read
    /// exactly the keyboard session's truth - the folded mirror stays true
    /// while any phone lane records, which reported a failed keyboard
    /// start as started and skipped its rollback.
    pub fn local_is_recording(&self) -> bool {
        matches!(
            *self.state.lock().unwrap(),
            RecordingState::Recording { .. } | RecordingState::Stopping
        )
    }

    /// Cancel any ongoing recording without returning audio samples.
    /// v1.6.2 W1-L2 cancel rule, documented once here: a LOCAL recording
    /// always wins (the keyboard is the surface the user is pressing
    /// Escape on); with no local recording, the MOST RECENTLY STARTED lane
    /// session is the one cancelled - the phone session the operator just
    /// saw start. Other lanes keep recording.
    ///
    /// v1.6.2 fleet pass 1: returns the target that was cancelled so the
    /// caller can scope the transcription-side teardown to it, and bumps
    /// EXACTLY that target's cancel generation - the lane arm leaves the
    /// local generation alone and every other lane's alone, so one phone's
    /// Escape can no longer discard another phone's in-flight stop pipeline
    /// (the cross-fire the single global bump allowed). The local
    /// generation still bumps even when nothing is recording: that is the
    /// keyboard user's way to abort their own stop pipeline that is already
    /// past capture (mid-transcription), which no lane arm may steal.
    pub fn cancel_recording(&self) -> CancelTarget {
        self.invalidate_recording_readiness();
        let mut state = self.state.lock().unwrap();

        // The per-lane cancel first: a lane session's teardown never touches
        // the local machine, so it runs before the local arm's match.
        let cancel_lane = if matches!(*state, RecordingState::Idle) {
            let lane = self.last_recording_lane.load(Ordering::SeqCst);
            (lane != 0 && self.lane_is_recording(lane)).then_some(lane)
        } else {
            None
        };
        if let Some(lane) = cancel_lane {
            self.bump_cancel_generation(cancel_bump_lane(CancelTarget::Lane(lane)));
            drop(state);
            // Fleet pass: the entry leaves the map under the lock and the
            // recorder stops OUTSIDE it. stop()'s pause handshake is
            // acknowledged through push_remote_chunk / ack_remote_pause,
            // which need this same lock - stopping under it deadlocked the
            // ack for the full 2 s PAUSE_ACK_TIMEOUT with every phone's
            // pushes locked out and the shortcut dispatch thread blocked.
            let cancelled = {
                let mut lanes = self.remote_lanes.lock().unwrap_or_else(|e| e.into_inner());
                let entry = lanes.remove(&lane);
                let still_recording = lanes.values().any(|l| l.recording);
                self.remote_lanes_recording
                    .store(still_recording, Ordering::SeqCst);
                entry
            };
            if let Some(mut entry) = cancelled {
                // A removed entry has no push route left and the cancel
                // discards the audio anyway: close the source first so the
                // stop cannot wait out the pause timeout.
                entry.source.close();
                if entry.recording {
                    let _ = entry.recorder.stop(); // Discard the result
                }
                let _ = entry.recorder.close();
            }
            // Fleet pass 3: fold from the lanes' CURRENT truth (the atomic),
            // never the still_recording snapshot - another lane can start in
            // the gap between the snapshot and this fold, and the snapshot
            // would clobber its mirror to false mid-recording.
            let local_active = local_capture_active(&*self.state.lock().unwrap());
            let lanes_active = self.remote_lanes_recording.load(Ordering::SeqCst);
            self.recording_active
                .store(local_active || lanes_active, Ordering::SeqCst);
            self.remove_mute();
            // KB-028: a cancelled lane's end is a session end too - a parked
            // companion close runs here when nothing else records.
            self.run_deferred_remote_close();
            return CancelTarget::Lane(lane);
        }

        let target = match *state {
            RecordingState::Recording { .. } => {
                self.set_state(&mut state, RecordingState::Idle);
                drop(state);

                // Restore the forced mute the same way the normal stop path
                // does (actions.rs calls remove_mute on stop). Without this,
                // cancellation never restores it: in always-on mode the local
                // stream stays open so stop_microphone_stream never runs,
                // the remote path's close_remote_stream has no mute restore, and
                // the on-demand lazy close defers it by the 30 s idle
                // timeout - the system stayed muted until the next
                // NON-cancelled dictation completed (KB-002).
                self.remove_mute();

                if let Some(rec) = self.recorder.lock().unwrap().as_ref() {
                    let _ = rec.stop(); // Discard the result
                }

                *self.is_recording.lock().unwrap() = false;

                // In on-demand mode, close the mic (lazily if the setting
                // is enabled)
                if matches!(*self.mode.lock().unwrap(), MicrophoneMode::OnDemand) {
                    if get_settings(&self.app_handle).lazy_stream_close {
                        self.schedule_lazy_close();
                    } else {
                        self.stop_microphone_stream();
                    }
                }

                // KB-010: a lid change that arrived mid-recording re-resolves
                // the always-on stream now that the state is Idle.
                self.run_deferred_clamshell_reresolve();
                // KB-028: the cancelled local session's end re-triggers a
                // companion disable's parked lane-recorder close.
                self.run_deferred_remote_close();
                CancelTarget::Local
            }
            RecordingState::Stopping => {
                debug!("Cancellation requested while recording is stopping");
                // Defensive twin of the restore above: a cancel that lands
                // while the stop pipeline is finalizing must not leave our
                // forced mute behind even if the pipeline's own teardown is
                // interrupted. remove_mute is a no-op when we did not mute.
                self.remove_mute();
                CancelTarget::Local
            }
            RecordingState::Idle => CancelTarget::None,
        };
        // Every non-lane resolution is the local machine's business (the
        // Idle bump included: it aborts the LOCAL stop pipeline already past
        // capture, and cannot touch any lane's scoped generation).
        self.bump_cancel_generation(cancel_bump_lane(target));
        target
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

    /// KB-294 (v1.6.5): a lane whose route (its connection) is already
    /// gone pays NO release-buffer window. The disconnect edge unregisters
    /// the connection BEFORE queueing the forced finalize, so by the time
    /// the stop pipeline reaches the tail decision no chunk can ever
    /// arrive - the sleep was pure added latency on the mid-dictation
    /// disconnect's recovery path. Route LIVE keeps
    /// effective_release_buffer_ms exactly (the trailing-consonant tail
    /// still applies while the phone is connected).
    #[test]
    fn kb294_a_dead_lanes_route_pays_no_release_buffer_window() {
        // Route live: the full predicate, value for value.
        assert_eq!(lane_release_buffer_ms(true, 0, 200, true), 200);
        assert_eq!(lane_release_buffer_ms(true, 500, 200, true), 500);
        assert_eq!(lane_release_buffer_ms(true, 300, 200, false), 300);
        assert_eq!(lane_release_buffer_ms(true, 0, 0, true), 0);
        // Route gone: no window at all, whatever the settings say - the
        // leak-shape the old code paid on every mid-dictation disconnect.
        assert_eq!(lane_release_buffer_ms(false, 500, 200, true), 0);
        assert_eq!(lane_release_buffer_ms(false, 0, 200, true), 0);
        assert_eq!(lane_release_buffer_ms(false, 300, 0, false), 0);
    }

    /// KB-294, source pin (the interleave needs a live connection, so the
    /// lane stop's decision is pinned against this committed source, the
    /// cancel-path convention): the lane stop must gate the window on the
    /// lane's OWN route through lane_route_live / lane_release_buffer_ms -
    /// not call effective_release_buffer_ms directly, which has no
    /// route-gone arm and silently reintroduces the paid tail.
    #[test]
    fn kb294_lane_stop_gates_the_tail_on_the_lanes_own_route() {
        let source = include_str!("audio.rs");
        let at = source
            .find("pub fn stop_remote_lane_recording(")
            .expect("the lane stop");
        let body = &source[at..source.find("pub fn lane_is_recording").expect("its end")];
        assert!(
            body.contains("lane_route_live(&self.app_handle, lane)"),
            "the lane stop must read the lane's own route liveness"
        );
        assert!(
            body.contains("lane_release_buffer_ms("),
            "the lane stop must size its window through the route-aware helper"
        );
        assert!(
            !body.contains("effective_release_buffer_ms"),
            "a direct effective_release_buffer_ms call has no route-gone arm (KB-294)"
        );
        // The route read itself keys on the CONNECTION (the lane's only
        // push route), never on the local machine's stream state.
        let helper_at = source
            .find("fn lane_route_live(app: &tauri::AppHandle, lane: u64)")
            .expect("the route liveness helper");
        let helper_end = helper_at + source[helper_at..].find("\n}\n").expect("helper end");
        let helper = &source[helper_at..helper_end];
        assert!(
            helper.contains("connection_device(lane).is_some()"),
            "route liveness is the lane's connection registration"
        );
    }

    /// KB-028 (v1.6.5), source pin (reproducing the leak needs the real
    /// companion teardown racing a live local recording, so the shape is
    /// pinned against this committed source, the convention above): the
    /// companion teardown's lane-recorder close may never be SILENTLY
    /// skipped while a recording is live. The pre-fix shape in
    /// CompanionManager::stop - skip close_remote_stream() whenever
    /// is_recording() - leaked every pre-warmed idle lane recorder (ring +
    /// VAD session) whenever a LOCAL recording was the skip's reason: no
    /// lane pipeline existed to close them and nothing re-triggered the
    /// close when the local session ended. The close now parks in
    /// remote_close_pending and runs once nothing records anywhere.
    #[test]
    fn kb028_the_lane_recorder_close_parks_and_runs_at_every_session_end() {
        let source = include_str!("audio.rs");

        // 1. The park helper: parks exactly while a session is live,
        //    closes immediately otherwise.
        let defer_at = source
            .find("pub fn close_remote_stream_or_defer(&self)")
            .expect("the park helper");
        let defer_end = defer_at + source[defer_at..].find("\n    }\n").expect("fn end");
        let defer_body = &source[defer_at..defer_end];
        assert!(
            defer_body.contains("self.is_recording()"),
            "the park decision reads the folded recording mirror"
        );
        assert!(
            defer_body.contains("remote_close_pending.store(true"),
            "a skipped close must be PARKED, never dropped"
        );
        assert!(
            defer_body.contains("self.close_remote_stream();"),
            "the idle path closes immediately, exactly as before"
        );

        // 2. The deferred runner: never fires while anything still
        //    records (a lane mid-pipeline owns its recorder), then closes.
        let run_at = source
            .find("fn run_deferred_remote_close(&self)")
            .expect("the deferred runner");
        let run_end = run_at + source[run_at..].find("\n    }\n").expect("fn end");
        let run_body = &source[run_at..run_end];
        assert!(
            run_body.contains(
                "!self.remote_close_pending.load(Ordering::SeqCst) || self.is_recording()"
            ),
            "the runner waits for NOTHING to be recording anywhere"
        );
        assert!(
            run_body.contains("self.close_remote_stream();"),
            "the runner performs the parked close"
        );

        // 3. Exactly the four session-end folds re-trigger it: the lane
        //    stop, the local stop, and both cancel arms. A session end
        //    added without this call silently reintroduces the leak for
        //    that session shape. (Scoped to the production half of the
        //    file - this pin's own text is not a call site, and the
        //    file's top holds cfg(test) helper blocks.)
        let production = &source[..source.find("\nmod tests {").expect("the test module")];
        assert_eq!(
            production
                .matches("self.run_deferred_remote_close();")
                .count(),
            4,
            "KB-028: the parked close must run at all four session-end folds: \
             the lane stop, the local stop, and the two cancel arms"
        );
    }

    /// v1.6.2 multi-phone review pin: the shared recording mirror's local
    /// contribution is {Recording, Stopping} - Stopping is the Mac
    /// session's capture tail (release buffer plus recorder stop, still
    /// holding the mic), not idle. The lane-stop fold once inlined a
    /// Recording-only match, so a phone lane's stop landing inside the
    /// Mac's Stopping tail (stop pipelines run on their own spawned
    /// threads) flipped the mirror to false mid-capture.
    #[test]
    fn local_capture_active_counts_the_stopping_tail() {
        assert!(!local_capture_active(&RecordingState::Idle));
        assert!(local_capture_active(&RecordingState::Recording {
            binding_id: "transcribe".to_string()
        }));
        assert!(local_capture_active(&RecordingState::Stopping));
    }

    /// Source pin for the same fix (the lane-stop fold needs real
    /// recorders, so its body is pinned against this committed source,
    /// the cancel-path convention): stop_remote_lane_recording must fold
    /// the mirror through the shared local_capture_active predicate, not
    /// an inline Recording-only match that re-opens the Stopping hole.
    #[test]
    fn stop_remote_lane_folds_the_mirror_through_the_shared_predicate() {
        let source = include_str!("audio.rs");
        let at = source
            .find("pub fn stop_remote_lane_recording(")
            .expect("the lane stop");
        let body = &source[at..at
            + source[at..]
                .find("pub fn lane_is_recording")
                .expect("its end")];
        assert!(
            body.contains("local_capture_active(&*self.state.lock().unwrap())"),
            "the lane-stop fold must use the shared local-activity predicate"
        );
        assert!(
            !body.contains("RecordingState::Recording { .. }"),
            "no inline Recording-only membership match may live in the fold"
        );
    }

    /// Fleet pass 3, source pin (the interleave needs real recorders, so
    /// the folds are pinned against this committed source, the convention
    /// above): every lane-stop-shaped fold of the shared recording mirror
    /// must read the lanes' CURRENT truth - the remote_lanes_recording
    /// atomic, exactly as set_state does - never a still_recording
    /// snapshot taken before the release-buffer sleep or the lock drop. A
    /// lane start that lands inside that gap stores recording_active=true;
    /// a fold from the stale snapshot then clobbers it to false
    /// MID-RECORDING, and every mirror reader (the Escape gate, the
    /// failed-start surface guard, the local-LLM keep-warm planner, the
    /// server-stop close decision) reads a lie.
    #[test]
    fn lane_stop_folds_read_the_lanes_atomic_not_the_stale_snapshot() {
        let source = include_str!("audio.rs");
        let stop_at = source
            .find("pub fn stop_remote_lane_recording(")
            .expect("the lane stop");
        let stop_body = &source[stop_at..source.find("pub fn lane_is_recording").expect("its end")];
        let cancel_at = source
            .find("pub fn cancel_recording(&self) -> CancelTarget")
            .expect("cancel_recording");
        let cancel_body = &source
            [cancel_at..cancel_at + source[cancel_at..].find("\n    }\n}\n").expect("fn end")];
        let close_at = source
            .find("pub fn close_remote_stream(&self)")
            .expect("close_remote_stream");
        let close_body =
            &source[close_at..source.find("pub fn set_stream_owner").expect("its end")];
        for (body, name) in [
            (stop_body, "stop_remote_lane_recording"),
            (cancel_body, "cancel_recording's lane arm"),
            (close_body, "close_remote_stream"),
        ] {
            assert!(
                body.contains("self.remote_lanes_recording.load(Ordering::SeqCst)"),
                "{name}: the fold must read the lanes' CURRENT truth, not a pre-window snapshot"
            );
            assert!(
                body.contains("store(local_active || lanes_active"),
                "{name}: the fold must OR the fresh lanes truth in"
            );
            assert!(
                !body.contains("local_active || still_recording"),
                "{name}: folding from the stale still_recording snapshot clobbers a lane that started during the window"
            );
        }
    }

    /// Fleet pass 3, source pin (the interleave needs real CoreAudio, so
    /// the lock scope is pinned against this committed source, the
    /// convention above): the capture-environment restart must hold the
    /// recording-state lock across the WHOLE switch - the guard, the stop,
    /// and the reopen - exactly like update_selected_device,
    /// update_selected_channel, and update_clamshell_microphone. The lid
    /// watcher used to sample Idle under a lock it dropped before the
    /// restart ran, so a hotkey start that committed in between had its
    /// fresh recorder force-stopped and its ring destroyed (the silent
    /// "no audio" outcome). The deferral arm is the ONLY place the guard
    /// lock may be dropped, and the lid entry point must delegate
    /// wholesale instead of sampling-and-releasing ahead of the restart.
    #[test]
    fn capture_environment_restart_holds_the_state_lock_across_the_switch() {
        let source = include_str!("audio.rs");
        let at = source
            .find("fn restart_open_stream_on_new_device(&self)")
            .expect("the capture-environment restart");
        let body = &source[at..source
            .find("pub fn update_clamshell_microphone")
            .expect("its end")];
        let lock_at = body
            .find("let state = self.state.lock().unwrap();")
            .expect("the restart takes the state lock");
        let guard_at = body
            .find("capture_restart_forbidden(&state)")
            .expect("the restart re-checks the recording state under the lock");
        let stop_at = body
            .find("self.stop_microphone_stream();")
            .expect("the restart stops the stream");
        let start_at = body
            .find("self.start_microphone_stream()")
            .expect("the restart reopens the stream");
        assert!(
            lock_at < guard_at && guard_at < stop_at && stop_at < start_at,
            "the state lock must span the guard, the stop, and the reopen"
        );
        assert_eq!(
            body.matches("drop(state);").count(),
            1,
            "exactly one unlock, and it must live in the deferral arm"
        );
        let drop_at = body.find("drop(state);").expect("the deferral unlock");
        let defer_at = body
            .find("clamshell_reresolve_pending")
            .expect("the deferral parking");
        assert!(
            drop_at < defer_at && defer_at < stop_at,
            "the unlock belongs to the deferral arm, which returns before any stop"
        );

        // The lid entry point must not sample Idle ahead of the restart
        // under a lock it drops - that TOCTOU was the hole.
        let handler_at = source
            .find("pub fn handle_clamshell_change(&self)")
            .expect("the lid entry point");
        let handler_body = &source[handler_at
            ..source
                .find("fn run_deferred_clamshell_reresolve")
                .expect("its end")];
        assert!(
            !handler_body.contains("drop(state);")
                && !handler_body.contains("capture_restart_forbidden"),
            "the lid entry point must delegate the guarded restart, not sample-and-release"
        );
    }

    /// Fleet pass 3, source pin (same convention): the capture-environment
    /// restart is MODE-AWARE. Only the always-on stream reopens - the
    /// restart's close_generation bump cancels the armed lazy-close timer,
    /// so re-opening an on-demand stream inside its 30 s lazy-close window
    /// left the mic hot indefinitely with no close ever armed again (the
    /// stranded-hot-mic class KB-237 documents). The on-demand arm closes
    /// and returns BEFORE any reopen; the next session open resolves the
    /// flipped lid fresh.
    #[test]
    fn capture_environment_restart_reopens_only_the_always_on_stream() {
        let source = include_str!("audio.rs");
        let at = source
            .find("fn restart_open_stream_on_new_device(&self)")
            .expect("the capture-environment restart");
        let body = &source[at..source
            .find("pub fn update_clamshell_microphone")
            .expect("its end")];
        let stop_at = body
            .find("self.stop_microphone_stream();")
            .expect("the stop");
        let mode_gate_at = body
            .find("matches!(*self.mode.lock().unwrap(), MicrophoneMode::AlwaysOn)")
            .expect("the restart gates the reopen on the always-on mode");
        let start_at = body
            .find("self.start_microphone_stream()")
            .expect("the reopen");
        assert!(
            stop_at < mode_gate_at && mode_gate_at < start_at,
            "the mode gate must sit between the stop and the reopen: an on-demand stream closes and returns before any reopen"
        );
        // The deferred path keeps its own mode gate, so a deferred
        // on-demand re-resolve is dropped rather than restarted.
        let deferred_at = source
            .find("fn run_deferred_clamshell_reresolve(&self)")
            .expect("the deferred re-resolve");
        let deferred_body = &source[deferred_at..at];
        assert!(
            deferred_body.contains("MicrophoneMode::AlwaysOn"),
            "the deferred re-resolve must stay gated on the always-on mode"
        );
    }

    /// KB-273 (v1.6.4) source pin: the lid-driven capture restart is GATED
    /// on a configured clamshell microphone BEFORE the state lock is ever
    /// taken. A lid flip can only change the desired microphone when a
    /// clamshell microphone is configured; without one, the old path still
    /// took the state lock and ran the full CoreAudio stop/start across it
    /// on EVERY flip (blocking every start/stop pipeline for the reopen's
    /// duration - the UI-freeze class) to resolve the identical device.
    /// The stop/reopen stays under the state lock WHEN a clamshell
    /// microphone is configured: the fleet-pass-3 lock-across-switch
    /// discipline (pinned above) closes a real start-commit TOCTOU, and
    /// this gate is what removes the pointless restarts entirely.
    #[test]
    fn kb273_lid_restart_is_gated_on_a_configured_clamshell_microphone_first() {
        let source = include_str!("audio.rs");
        let at = source
            .find("fn restart_open_stream_on_new_device(&self)")
            .expect("the capture-environment restart");
        let end = source[at..]
            .find("pub fn update_clamshell_microphone")
            .expect("its end");
        let body = &source[at..at + end];
        let gate_at = body
            .find("clamshell_microphone")
            .expect("the clamshell-microphone gate");
        let gate_return_at = body[..gate_at + 400]
            .find("return;")
            .expect("the gate returns before any restart work");
        let lock_at = body
            .find("let state = self.state.lock().unwrap();")
            .expect("the restart takes the state lock after the gate");
        assert!(
            gate_at < gate_return_at && gate_return_at < lock_at,
            "the clamshell gate must return BEFORE the state lock is taken"
        );
        // The gate is the FIRST thing in the function: nothing ahead of it
        // takes a lock or touches capture.
        let before_gate = &body[..gate_at];
        assert!(
            !before_gate.contains(".lock()"),
            "nothing ahead of the gate may take a lock (the every-lid-flip freeze)"
        );
    }

    /// KB-272 (v1.6.4) source pin, the manager half: push_remote_chunk
    /// must DELEGATE to the free routing core (which the behavioral test
    /// below executes with real recorders) and the core's bool must be
    /// EXACTLY lane-map membership - a hit pushes into that lane's own
    /// source under the lanes lock (never the state lock), a miss returns
    /// false without touching anything. The companion server counts the
    /// false arm as an admitted-but-dropped chunk (see the server-side
    /// KB-272 pins).
    #[test]
    fn kb272_push_remote_chunk_delegates_to_the_testable_routing_core() {
        let source = include_str!("audio.rs");
        let at = source
            .find("pub fn push_remote_chunk(&self, lane: u64, samples: &[f32]) -> bool {")
            .expect("push_remote_chunk");
        let end = at + source[at..].find("\n    }\n").expect("fn end");
        let body = &source[at..end];
        assert!(
            body.contains("route_remote_chunk(&self.remote_lanes, lane, samples)"),
            "the method must delegate its whole body to the free core (so the behavioral test covers the exact code the server runs)"
        );
        let core_at = source
            .find("fn route_remote_chunk(")
            .expect("the routing core");
        let core_end = core_at + source[core_at..].find("\n}\n").expect("core end");
        let core = &source[core_at..core_end];
        assert!(
            core.contains("entry.source.push_chunk(samples);"),
            "a hit pushes into THIS lane's own source"
        );
        assert!(
            core.contains("None => false,"),
            "a lane with no recorder returns false (the server's admitted-dropped counter)"
        );
        assert!(
            !core.contains("self.state"),
            "the push must never take the recording-state lock (phone audio ingestion would block on every Mac session transition)"
        );
    }

    /// KB-272 (v1.6.4): the routing core EXECUTED with real lane recorders
    /// (AudioRecorder::open_remote builds the exact RemoteLane pair the
    /// production pre-warm builds; only the manager shell around the map is
    /// out of reach under the mock runtime). A hit routes the chunk into
    /// THIS lane's own ring and it round-trips through the real
    /// stop pipeline shape (source close first, then stop); a lane whose
    /// entry a stop pipeline already REMOVED returns false - the exact
    /// admitted-but-dropped shape the server counts; a lane that never
    /// existed returns false forever without panicking or accruing
    /// overruns; and one lane's ring never receives another lane's chunk.
    #[test]
    fn kb272_route_remote_chunk_round_trips_real_recorders_and_reports_membership() {
        let mut lane7_rec = AudioRecorder::new().expect("recorder for lane 7");
        let lane7_source = lane7_rec.open_remote().expect("open lane 7 source");
        let mut lane12_rec = AudioRecorder::new().expect("recorder for lane 12");
        let lane12_source = lane12_rec.open_remote().expect("open lane 12 source");

        let mut lanes = std::collections::HashMap::new();
        lanes.insert(
            7u64,
            RemoteLane {
                recorder: lane7_rec,
                source: lane7_source,
                recording: true,
                generation: 1,
            },
        );
        lanes.insert(
            12u64,
            RemoteLane {
                recorder: lane12_rec,
                source: lane12_source,
                recording: false,
                generation: 2,
            },
        );
        let lanes = Mutex::new(lanes);

        // A hit: the chunk enters lane 7's ring (the recorder must be
        // started for the consumer to drain it into a recording).
        let ready = {
            let mut guard = lanes.lock().unwrap();
            guard
                .get_mut(&7)
                .expect("lane 7")
                .recorder
                .start(VadPolicy::Disabled)
                .expect("start")
        };
        let speech: Vec<f32> = (0..1600).map(|i| (i as f32 * 0.005) - 4.0).collect();
        assert!(
            route_remote_chunk(&lanes, 7, &speech),
            "a live lane's chunk is delivered"
        );
        ready
            .recv_timeout(Duration::from_secs(1))
            .expect("lane 7 capture ready");

        // Isolation: lane 12 receives its own chunk and lane 7's stays
        // untouched (lane 12 is not started, so its chunk only needs to
        // land in the ring; the assertion below drains lane 7 only).
        assert!(route_remote_chunk(&lanes, 12, &[0.25, -0.25]));

        // The stop pipeline's shape for lane 7: remove the entry, close the
        // source first, then stop - the captured audio comes back.
        let mut taken = lanes.lock().unwrap().remove(&7).expect("take lane 7");
        taken.source.close();
        let samples = taken.recorder.stop().expect("stop returns the samples");
        assert_eq!(
            &samples[..speech.len()],
            &speech[..],
            "the routed chunk round-trips through the real ring"
        );
        let _ = taken.recorder.close();

        // KB-272 proper: after the stop pipeline removed the entry, the
        // admission may still say live (the Mac's session state has not
        // flipped yet) - the route must return false for every further
        // chunk, the exact admitted-but-dropped shape the server counts.
        for _ in 0..3 {
            assert!(
                !route_remote_chunk(&lanes, 7, &speech),
                "a removed lane reports non-delivery for every chunk"
            );
        }
        // A lane that never existed: false forever, no panic, no overrun
        // leak through any recorder (lane 12's ring is untouched).
        for _ in 0..3 {
            assert!(!route_remote_chunk(&lanes, 99, &speech));
        }
        let mut lane12 = lanes.lock().unwrap().remove(&12).expect("lane 12 intact");
        let lane12_overruns = lane12.source.overrun_samples();
        let _ = lane12.recorder.close();
        assert_eq!(lane12_overruns, 0, "chunks for unknown lanes touch nothing");
    }

    /// Fleet pass 2: the routing core survives a POISONED lanes guard. The
    /// companion manager converted every std Mutex it owns to
    /// poison-tolerant acquisition for exactly this failure class (a panic
    /// under the guard - try_start_remote_lane_recording opens the
    /// CoreAudio device and the VAD while holding it), but
    /// route_remote_chunk's `.lock().unwrap()` was left behind: after one
    /// poison, EVERY phone audio frame panicked its connection task - all
    /// phone dictation dead until restart, stderr-only. The guard is
    /// bookkeeping (route by lane key), so recovery beats poisoning the
    /// whole phone-ingestion surface.
    #[test]
    fn kb272_route_remote_chunk_survives_a_poisoned_lanes_guard() {
        let lanes = Mutex::new(std::collections::HashMap::new());

        // Poison the guard: panic in a task while holding it (caught, the
        // way tokio catches a task panic).
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = lanes.lock().unwrap();
            panic!("poison the lanes guard");
        }));

        // Every later phone frame must still route - an unknown lane
        // reports false, the admitted-dropped shape - instead of
        // panicking its connection task.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            route_remote_chunk(&lanes, 7, &[0.5, -0.5])
        }));
        assert_eq!(
            result.expect("a poisoned guard must not panic the routing core"),
            false,
            "an unknown lane still reports not-delivered under poison"
        );
    }

    /// Fleet pass 2, source pin (the manager shell cannot be constructed
    /// under the mock runtime; see the module note): the two NEW audio-side
    /// lock sites on the phone path stay poison-tolerant like the
    /// companion manager's -
    /// route_remote_chunk (every phone audio frame, connection task) and
    /// drop_lane_cancel_generation (the coordinator thread's
    /// retire_dead_lane, whose catch_unwind logs once and exits with no
    /// restart - a poison there ends keyboard AND phone dictation). A
    /// `.lock().unwrap()` at either site reintroduces the exact failure
    /// class the companion locks were converted for.
    ///
    /// KB-279 (v1.6.5): the two slices above pinned only the sites the
    /// fleet pass converted, and every OTHER acquisition on the audio
    /// side's `remote_lanes` and `lane_cancel_generations` maps still
    /// unwrapped - the manager-side hardening stopped at the boundary. A
    /// panic under any of them (try_start_remote_lane_recording opens the
    /// CoreAudio device and the VAD under the lanes guard) re-wedged
    /// connections the same way. The enumeration below walks the two MAPS
    /// wholesale, whitespace-stripped so multi-line acquisitions count:
    /// every acquisition must be poison-tolerant and no plain
    /// `.lock().unwrap()` acquisition may remain on either map.
    #[test]
    fn phone_path_lock_sites_are_poison_tolerant() {
        let source = include_str!("../managers/audio.rs");
        for site in [
            "fn route_remote_chunk(",
            "pub fn drop_lane_cancel_generation(",
        ] {
            let at = source
                .find(site)
                .unwrap_or_else(|| panic!("{site} must exist"));
            let end = at
                + source[at..]
                    .find("\n    }\n")
                    .or_else(|| source[at..].find("\n}\n"))
                    .expect("fn end");
            let body = &source[at..end];
            assert!(
                body.contains("unwrap_or_else(|e| e.into_inner())"),
                "{site} must acquire its lock poison-tolerantly (see companion/mod.rs's locking note)"
            );
            assert!(
                !body.contains(".lock().unwrap()"),
                "{site} must not panic on a poisoned lock"
            );
        }

        // KB-279: enumerate the two maps wholesale. Strip whitespace so a
        // rustfmt-split acquisition (`self\n    .remote_lanes\n    .lock()`)
        // is seen as the same site as a one-liner.
        let normalized: String = source.chars().filter(|c| !c.is_whitespace()).collect();
        for map in ["remote_lanes", "lane_cancel_generations"] {
            let acquire = format!("{map}.lock()");
            let tolerant = format!("{map}.lock().unwrap_or_else(|e|e.into_inner())");
            let total = normalized.matches(&acquire).count();
            let good = normalized.matches(&tolerant).count();
            assert!(
                total > 0,
                "KB-279: the {map} enumeration found no lock sites - the pin went stale"
            );
            assert_eq!(
                good,
                total,
                "KB-279: every {map} acquisition must be poison-tolerant; \
                 {} plain .lock().unwrap() site(s) remain",
                total - good
            );
            assert!(
                !normalized.contains(&format!("{map}.lock().unwrap()")),
                "KB-279: a plain .lock().unwrap() on {map} panics on poison"
            );
        }
    }

    /// Fleet pass, source pin (the interleavings need real sockets, so the
    /// scopes are pinned against this committed source, the convention
    /// above): no lane path may call recorder.stop() while holding the
    /// remote_lanes lock, and every lane stop closes the source BEFORE
    /// stopping the recorder. stop()'s pause handshake is acknowledged
    /// through push_remote_chunk / ack_remote_pause, which take that same
    /// lock - a stop under it deadlocks its own ack for the full 2 s
    /// PAUSE_ACK_TIMEOUT, stalling every phone's audio ingestion and (on
    /// the cancel path) the shortcut dispatch thread. And a lane whose map
    /// entry is gone has no push route left, so without the source-close
    /// first the stop pays the same timeout on every healthy phone stop.
    #[test]
    fn lane_stops_run_outside_the_lanes_lock_and_close_the_source_first() {
        let source = include_str!("audio.rs");

        // The healthy stop: the source closes immediately before the stop.
        let stop_at = source
            .find("pub fn stop_remote_lane_recording(")
            .expect("the lane stop");
        let stop_body = &source[stop_at..source.find("pub fn lane_is_recording").expect("its end")];
        let source_close_at = stop_body
            .find("entry.source.close();")
            .expect("the healthy stop closes the source first");
        let recorder_stop_at = stop_body
            .find("entry.recorder.stop()")
            .expect("the recorder stop");
        assert!(
            source_close_at < recorder_stop_at,
            "the source must be closed before the recorder stops, or the pause handshake waits out its timeout"
        );

        // The cancel arm: the entry is removed under the lock inside a
        // scoped block, and the stop runs only on the taken entry after
        // that block ends.
        let cancel_at = source
            .find("pub fn cancel_recording(&self) -> CancelTarget")
            .expect("cancel_recording");
        let cancel_body = &source
            [cancel_at..cancel_at + source[cancel_at..].find("\n    }\n}\n").expect("fn end")];
        let remove_at = cancel_body
            .find("lanes.remove(&lane)")
            .expect("the cancel removes the entry under the lock");
        let handoff_at = cancel_body
            .find("if let Some(mut entry) = cancelled {")
            .expect("the cancel stops outside the lock, on the taken entry");
        let cancel_stop_at = cancel_body
            .find("entry.recorder.stop()")
            .expect("the cancel's recorder stop");
        assert!(
            remove_at < handoff_at && handoff_at < cancel_stop_at,
            "the cancel's stop must run on the entry AFTER it left the map, never under the lanes lock"
        );

        // The force teardown: the whole map drains inside a scoped block
        // and the stops run in the drained loop after it.
        let close_at = source
            .find("pub fn close_remote_stream(&self)")
            .expect("close_remote_stream");
        let close_body =
            &source[close_at..source.find("pub fn set_stream_owner").expect("its end")];
        let drain_at = close_body
            .find("lanes.drain()")
            .expect("the force teardown drains under the lock");
        let loop_at = close_body
            .find("for (_, mut entry) in drained {")
            .expect("the teardown loop on the drained entries");
        let teardown_stop_at = close_body
            .find("entry.recorder.stop()")
            .expect("the teardown's recorder stop");
        assert!(
            drain_at < loop_at && loop_at < teardown_stop_at,
            "the teardown's stops must run on drained entries AFTER the map lock was released"
        );
    }

    /// v1.6.2 W1-L2 pin: the pre-warm RAM rule is most-recent-connection
    /// only. A pre-warm retires every idle recorder OTHER than its
    /// target, never the target itself, and never a recording lane (its
    /// stop pipeline owns the teardown). Without the rule, N paired
    /// phones kept N idle VAD sessions and N rings alive at once (see the
    /// RemoteLane budget above); with it, the steady-state idle cost is
    /// one recorder no matter how many phones are paired.
    #[test]
    fn prewarm_retires_every_idle_recorder_except_the_target_and_spares_recording_lanes() {
        // Three phones paired: lane 7 mid-dictation, lanes 9 and 12 idle.
        // Pre-warming 12 (the most recent connection) retires only 9.
        let mut retired = prewarm_retires([(7u64, true), (9, false), (12, false)].into_iter(), 12);
        retired.sort_unstable();
        assert_eq!(retired, vec![9]);

        // A fresh connection pre-warming a lane that has no recorder yet
        // (the reconnect scenario): every older idle recorder retires.
        let mut retired = prewarm_retires([(9u64, false), (12, false)].into_iter(), 5);
        retired.sort_unstable();
        assert_eq!(retired, vec![9, 12]);

        // A recording target stays (its session keeps its recorder), and
        // it still retires the other idle lanes while sparing its fellow
        // recording lane.
        let mut retired = prewarm_retires([(3u64, true), (4, true), (6, false)].into_iter(), 3);
        retired.sort_unstable();
        assert_eq!(retired, vec![6]);

        // All lanes recording: a pre-warm retires nothing (every teardown
        // belongs to a stop pipeline), and an empty map retires nothing.
        assert!(prewarm_retires([(3u64, true), (4, true)].into_iter(), 9).is_empty());
        assert!(prewarm_retires(std::iter::empty(), 9).is_empty());
    }

    /// Fleet pass 1: the cancel cross-fire rule, pinned pure. A cancel
    /// resolution may bump ONLY the generation of the session it tore
    /// down: a lane-resolved Escape names its own lane, and no resolution
    /// may ever name a lane it did not cancel. Under the single global
    /// generation, cancelling one phone discarded another phone's
    /// in-flight stop pipeline (its captured audio with it).
    #[test]
    fn cancel_bump_lane_scopes_the_bump_to_the_cancelled_target() {
        assert_eq!(cancel_bump_lane(CancelTarget::Lane(7)), Some(7));
        assert_eq!(cancel_bump_lane(CancelTarget::Lane(9)), Some(9));
        // Local and nothing-resolved cancels belong to the LOCAL machine's
        // shared generation (None), never a lane's.
        assert_eq!(cancel_bump_lane(CancelTarget::Local), None);
        assert_eq!(cancel_bump_lane(CancelTarget::None), None);
    }

    /// The source-level pin for the same fix: cancel_recording bumps only
    /// through the target-scoped helper - the unconditional global
    /// `cancel_generation.fetch_add` that cross-fired every concurrent
    /// lane stop pipeline must stay out of it (the manager needs a Tauri
    /// AppHandle, so the arm is pinned against this committed source, the
    /// same convention as the coordinator's executor pins).
    #[test]
    fn cancel_recording_bumps_only_through_the_target_scoped_helper() {
        let source = include_str!("audio.rs");
        let fn_at = source
            .find("pub fn cancel_recording(&self) -> CancelTarget")
            .expect("cancel_recording");
        let body_end = fn_at + source[fn_at..].find("\n    }\n}\n").expect("fn end");
        let body = &source[fn_at..body_end];
        assert!(
            body.contains("bump_cancel_generation(cancel_bump_lane(CancelTarget::Lane(lane)))"),
            "the lane arm must bump its own lane's generation only"
        );
        assert!(
            body.contains("bump_cancel_generation(cancel_bump_lane(target))"),
            "every non-lane resolution must route through the same scoped helper"
        );
        assert!(
            !body.contains("cancel_generation.fetch_add"),
            "cancel_recording must never bump the shared generation directly: the pre-fix unconditional bump is what discarded other lanes' stop pipelines"
        );
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

    /// KB-237 privacy pin: flipping always-on off may close the local mic
    /// stream unless a LOCAL recording is live. A companion lane session
    /// keeps the state Idle (lane recorders are independent of the local
    /// stream), so a flip during a phone session closes the Mac's
    /// microphone immediately; a live local recording defers the close to
    /// its own stop or cancel pipeline, which performs it because the mode
    /// is OnDemand by then. Before the v1.6.2 per-lane split the remote
    /// session shared the single state machine, this guard read as "not
    /// Idle", the flip was skipped mid phone session, and the remote stop
    /// arm never touched the local stream: the local mic stayed hot
    /// forever after the user had turned always-on off. Manager-level
    /// update_mode is not runnable under tauri::test's MockRuntime (see
    /// the note above), so the guard update_mode routes through is pinned
    /// here.
    #[test]
    fn always_on_flip_closes_the_local_stream_unless_a_local_recording_is_live() {
        assert!(
            mode_flip_may_close_local_stream(&RecordingState::Idle),
            "idle (including during a companion lane session): close the local stream now"
        );
        assert!(
            !mode_flip_may_close_local_stream(&RecordingState::Recording {
                binding_id: "transcribe".to_string()
            }),
            "a live LOCAL recording defers the close to its stop pipeline"
        );
        assert!(
            !mode_flip_may_close_local_stream(&RecordingState::Stopping),
            "a recording in its stop pipeline still holds samples: the stop pipeline closes"
        );
    }

    /// v1.6.2 W1-L2: the streaming-engine ownership token codec. Every
    /// recorder's audio callback compares its owner's token against the
    /// atomic on every frame, so the encoding must be injective over the
    /// owner space (None, Local, and every lane id) and round-trip
    /// exactly - a collision would silently let a non-owner recorder feed
    /// the stream.
    #[test]
    fn stream_owner_tokens_round_trip_and_never_collide() {
        let owners = [
            StreamOwner::None,
            StreamOwner::Local,
            StreamOwner::Lane(1),
            StreamOwner::Lane(2),
            StreamOwner::Lane(7),
            StreamOwner::Lane(u64::MAX - 1),
        ];
        let mut tokens: Vec<u64> = Vec::new();
        for owner in owners {
            let token = stream_owner_token(owner);
            assert_eq!(
                stream_owner_from_token(token),
                owner,
                "the owner token must round-trip for {owner:?}"
            );
            tokens.push(token);
        }
        tokens.sort_unstable();
        let unique = tokens.len();
        tokens.dedup();
        assert_eq!(
            tokens.len(),
            unique,
            "distinct owners must encode to distinct tokens (a collision would gate the wrong recorder into the stream)"
        );
        assert_eq!(
            stream_owner_token(StreamOwner::None),
            0,
            "zero is the no-owner sentinel the recorder gates read as closed"
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

    /// AUD-12: the Exit mute restore must be FINAL. The detached readiness
    /// watcher (spawned at actions.rs:1258) applies the forced mute only
    /// after blocking first-sample checks, guarded by
    /// `is_recording_readiness_current(generation)` (actions.rs:1294). Every
    /// restore path EXCEPT process exit invalidates that readiness first
    /// (stop: actions.rs:1356-1357; cancel: `cancel_recording`
    /// audio.rs:1350), but the `RunEvent::Exit` handler (lib.rs:1206-1215)
    /// calls only `remove_mute()`. A watcher that wakes after the exit
    /// restore therefore still passes its generation check, and
    /// `apply_mute`'s capture guard (`*is_open || self.is_recording()`,
    /// audio.rs:742) is still satisfied - exit closes no stream and clears
    /// no recording state - so the late apply re-strands the system OUTPUT
    /// muted past process death.
    ///
    /// Manager-level construction is not runnable under tauri::test's
    /// MockRuntime (see the note above: the manager pins
    /// `tauri::AppHandle`), so the red test pins the missing pure decision
    /// core of `apply_mute` instead. CONTRACT the implementer must provide
    /// (a module-level production fn next to `mute_restore_action`, routed
    /// through by `AudioRecordingManager::apply_mute` so the real call site
    /// inherits it):
    ///
    /// ```text
    /// fn should_apply_forced_mute(
    ///     mute_while_recording: bool,          // the setting (audio.rs:729-731)
    ///     capture_active: bool,                // is_open || is_recording (audio.rs:742)
    ///     already_applied_this_session: bool,  // mute_state.did_mute (audio.rs:739-741)
    ///     exit_teardown_started: bool,         // shutdown flag RunEvent::Exit sets
    ///                                         // BEFORE remove_mute (lib.rs:1213)
    /// ) -> bool
    /// ```
    ///
    /// Apply only when every gate passes. `exit_teardown_started` is the new
    /// AUD-12 gate: once the Exit handler has begun its restore, no late
    /// readiness apply may re-mute. The flag must be READ INSIDE apply_mute's
    /// `mute_state` critical section (not only in the watcher's check at
    /// actions.rs:1294, which is a check-then-act window the exit restore can
    /// interleave with): an apply completing before the restore leaves
    /// did_mute = true so remove_mute restores it, and an apply starting
    /// after the flag store sees the flag and no-ops.
    #[test]
    fn late_readiness_apply_cannot_re_mute_after_exit_teardown() {
        // The watcher's normal path is unchanged: a live session over an
        // active capture still applies the forced mute on first samples.
        assert!(
            should_apply_forced_mute(true, true, false, false),
            "normal readiness apply: the watcher still mutes on first samples"
        );

        // AUD-12: exit teardown has begun. The capture is STILL active at
        // that instant (exit tears down neither the stream nor the recording
        // state - that is exactly why the late apply strands the mute), so
        // only the exit gate can veto it.
        assert!(
            !should_apply_forced_mute(true, true, false, true),
            "AUD-12: a readiness watcher waking after the exit restore must not re-mute"
        );

        // The existing duplicate-apply veto keeps its meaning: a second apply
        // must never overwrite prev_muted with our own forced state.
        assert!(
            !should_apply_forced_mute(true, true, true, false),
            "duplicate/late apply while already muted: no re-snapshot"
        );
        assert!(
            !should_apply_forced_mute(true, true, true, true),
            "duplicate apply stays vetoed during exit teardown"
        );

        // The pre-existing guards keep their meaning.
        assert!(
            !should_apply_forced_mute(false, true, false, false),
            "mute_while_recording off: never apply"
        );
        assert!(
            !should_apply_forced_mute(true, false, false, false),
            "no active capture path: nothing to mute for"
        );
        assert!(
            !should_apply_forced_mute(true, false, false, true),
            "exit teardown vetoes the apply regardless of capture state"
        );
    }
}
