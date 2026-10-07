use crate::audio_toolkit::{
    apply_custom_words, apply_terminal_punctuation, apply_voice_deletion, detect_output_language,
    interim_display_transform, normalize_spoken_punctuation, normalize_transcription_output,
    remove_filler_words, remove_trailing_word_from_buffer, OutputLanguageEvidence,
    VoiceDeletionOutcome,
};
use crate::chinese_script::{convert_chinese_script, ChineseVariety};
use crate::engine_supervisor::{
    DeviceInfo, DeviceSelector, EngineError, EngineSupervisor, LoadSpec, LoadedInfo,
    StreamProgress, Unloading,
};
use crate::managers::audio::AudioRecordingManager;
use crate::managers::model::{EngineType, ModelInfo, ModelManager, ModelSource};
use crate::memory;
use crate::settings::{
    get_settings, AppSettings, ChineseScript, ModelUnloadTimeout, OrtAcceleratorSetting,
    TranscribeAcceleratorSetting,
};
use anyhow::Result;
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use tauri::{AppHandle, Emitter, Manager};
use tauri_specta::Event;
use transcribe_cpp::{Backend, RunExtension, RunOptions, StreamOptions, Task, WhisperRunOptions};
use transcribe_rs::{
    onnx::{
        canary::CanaryModel,
        cohere::CohereModel,
        gigaam::GigaAMModel,
        moonshine::{MoonshineModel, MoonshineVariant, StreamingModel},
        parakeet::{ParakeetModel, ParakeetParams, TimestampGranularity},
        sense_voice::{SenseVoiceModel, SenseVoiceParams},
        Quantization,
    },
    SpeechModel, TranscribeOptions,
};

const STREAM_PERF_LOG_INTERVAL: Duration = Duration::from_secs(5);

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// Editorial rank for a RAM auto-fallback candidate. Direct registry ids
/// resolve through the rank table; an alternate quant found on disk has the
/// id `"{repo_id}/{filename}"`, which that table (keyed by the descriptor's
/// default file) misses - resolve those through the catalog's file table so
/// they compete at their base model's rank instead of sorting last.
/// Unranked/unknown models (user-added Hugging Face repos, local customs,
/// legacy entries without a rank) return `u32::MAX`: they rank AFTER every
/// ranked catalog model but stay eligible, so a user-added model is only
/// chosen when no cataloged model fits. Ties among unranked candidates are
/// settled by the resolver's footprint-then-id rule, keeping the order
/// deterministic.
fn fallback_rank(info: &ModelInfo) -> u32 {
    let direct = crate::catalog::rank_of(&info.id);
    if direct != u32::MAX {
        return direct;
    }
    match &info.source {
        ModelSource::HuggingFace { repo_id, .. } => {
            crate::catalog::file_in_catalog(&info.filename, Some(repo_id.as_str()))
                .and_then(|(desc, _)| desc.recommended_rank)
                .unwrap_or(u32::MAX)
        }
        _ => u32::MAX,
    }
}

/// The pure half of the fallback inventory: map every downloaded model
/// except `failed_id` to its resolver candidate. Split from the method so the
/// inventory contract (custom and user-added Hugging Face models included,
/// ranked after the catalog) is unit-testable without an app handle.
fn fallback_candidate_list(
    models: &[ModelInfo],
    failed_id: &str,
) -> Vec<memory::FallbackCandidate> {
    models
        .iter()
        .filter(|info| info.is_downloaded && info.id != failed_id)
        .map(|info| memory::FallbackCandidate {
            rank: fallback_rank(info),
            footprint_bytes: info.size_mb.saturating_mul(1024 * 1024),
            id: info.id.clone(),
        })
        .collect()
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelStateEvent {
    pub event_type: String,
    pub model_id: Option<String>,
    pub model_name: Option<String>,
    pub error: Option<String>,
}

/// One-shot notification that a RAM auto-fallback fired: the selected model
/// was refused by the memory gate and the named fallback model is being
/// loaded for this dictation. The frontend toasts it transiently.
#[derive(Clone, Debug, Serialize)]
pub struct ModelFallbackEvent {
    pub requested_model_name: String,
    pub fallback_model_name: String,
}

/// Live transcription snapshot emitted to the overlay during a streaming run.
/// `committed` is the append-only, flicker-free prefix; `tentative` is the
/// volatile suffix the model may still rewrite.
#[derive(Clone, Debug, Serialize, Deserialize, Type, tauri_specta::Event)]
pub struct StreamTextEvent {
    pub committed: String,
    pub tentative: String,
}

/// Phase of the streaming overlay card, emitted to drive its UI state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum StreamPhase {
    /// Receiving audio / live text (or waiting for the stream to begin). Rust
    /// does not emit this today; the frontend starts in this phase and Rust only
    /// emits transitions away from it.
    Listening,
    /// Finalizing or post-processing - show a spinner.
    Working,
}

/// Semantic kind of "working" phase, used to localize the spinner label.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum StreamWorkKind {
    Transcribing,
    Polishing,
}

/// Emitted to switch the streaming overlay to a working spinner.
#[derive(Clone, Debug, Serialize, Deserialize, Type, tauri_specta::Event)]
pub struct StreamPhaseEvent {
    pub phase: StreamPhase,
    /// Present only when `phase` is `Working`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<StreamWorkKind>,
}

/// Commands sent to the streaming worker thread. Audio frames and the finalize
/// request travel the same channel so FIFO ordering guarantees every fed frame
/// is processed before finalize runs.
enum StreamCmd {
    Feed(Vec<f32>),
    /// Flush the stream and reply with the final text, or `None` if no
    /// usable stream exists (caller should fall back to batch transcription).
    /// `Err(Cancelled)` if the finalize itself was cancelled.
    Finalize(mpsc::Sender<StreamFinalizeReply>),
    Cancel,
}

type StreamFinalizeReply = Result<Option<FinalizedStreamText>, EngineError>;

struct FinalizedStreamText {
    text: String,
    output_language: OutputLanguageEvidence,
    /// The streaming model's supported languages, for text-based detection.
    supported_languages: Vec<String>,
}

/// Byte length of the longest common prefix of `a` and `b`, always landing on
/// a `char` boundary of both (the prefix is built from equal `char`s).
fn common_prefix_len(a: &str, b: &str) -> usize {
    a.chars()
        .zip(b.chars())
        .take_while(|(x, y)| x == y)
        .map(|(c, _)| c.len_utf8())
        .sum()
}

/// Concatenate `base` with `rest`, collapsing a single duplicated space/tab
/// at the seam (a manual word deletion keeps the separator that preceded the
/// removed word, while the engine's continuation often re-supplies one).
fn join_raw(base: &str, rest: &str) -> String {
    if base.ends_with([' ', '\t']) && rest.starts_with([' ', '\t']) {
        let mut joined = String::with_capacity(base.len() + rest.len());
        joined.push_str(base);
        joined.push_str(&rest[1..]);
        joined
    } else {
        format!("{base}{rest}")
    }
}

/// The live dictation buffer of an active streaming session.
///
/// The engine owns the authoritative raw text and only hands out snapshots
/// (an append-only `committed` prefix plus a volatile `tentative` suffix), so
/// manual buffer edits (the delete-last-word hotkey) cannot rewrite what the
/// engine holds. Instead the buffer keeps its own raw-domain copy:
///
/// * `base` holds the buffer content at the moment of the last manual edit,
///   with every edit applied;
/// * `raw_seen` is the engine's FULL snapshot (committed + tentative) at
///   that moment. New material on any later snapshot is the part beyond the
///   common prefix with `raw_seen`, appended to `base`.
///
/// Anchoring on the full snapshot (not just the committed prefix) is what
/// keeps a deleted word deleted: a word removed while still tentative does
/// not come back when the engine commits it verbatim afterwards, because
/// those bytes sit inside `raw_seen` and are never re-consumed. The known
/// artifact: if the engine REVISES that region while committing it (a
/// hypothesis correction), the common prefix stops at the first differing
/// byte and the revised word re-enters the buffer. That is rare and
/// self-limiting; everything after the divergence behaves normally.
///
/// Everything is recomputed from scratch on every tick; nothing is applied
/// incrementally to previous output, so a spoken phrase split across chunk
/// boundaries cannot hide and tentative rewrites show up naturally.
struct StreamSessionBuffer {
    /// True between `begin` (the engine stream actually started) and
    /// `combine_final`/`end`. Only then may hotkey edits apply.
    live: bool,
    /// Toggles for the interim display transform, captured when the stream
    /// begins (a mid-session toggle applies from the next session, matching
    /// how `PreviewScript` captures `chinese_script` today).
    spoken_punctuation: bool,
    voice_deletion: bool,
    preview_script: PreviewScript,
    supported_languages: Vec<String>,
    base: String,
    raw_seen: String,
    last_full: String,
}

impl Default for StreamSessionBuffer {
    fn default() -> Self {
        Self {
            live: false,
            spoken_punctuation: true,
            voice_deletion: true,
            preview_script: PreviewScript::new(
                crate::settings::ChineseScript::AsTranscribed,
                &OutputLanguageEvidence::Unknown,
            ),
            supported_languages: Vec::new(),
            base: String::new(),
            raw_seen: String::new(),
            last_full: String::new(),
        }
    }
}

impl StreamSessionBuffer {
    fn begin(
        &mut self,
        preview_script: PreviewScript,
        spoken_punctuation: bool,
        voice_deletion: bool,
        supported_languages: &[String],
    ) {
        self.live = true;
        self.spoken_punctuation = spoken_punctuation;
        self.voice_deletion = voice_deletion;
        self.preview_script = preview_script;
        self.supported_languages = supported_languages.to_vec();
        self.base.clear();
        self.raw_seen.clear();
        self.last_full.clear();
    }

    fn end(&mut self) {
        self.live = false;
        self.base.clear();
        self.raw_seen.clear();
        self.last_full.clear();
    }

    /// The raw-domain working buffer for an engine snapshot.
    fn combine(&self, snapshot: &str) -> String {
        let keep = common_prefix_len(&self.raw_seen, snapshot);
        join_raw(&self.base, &snapshot[keep..])
    }

    /// Record a snapshot and render what the overlay should display: the
    /// combined raw buffer, script-converted, then the interim display
    /// transform (spoken punctuation, voice deletion; deliberately nothing
    /// else, see [`interim_display_transform`]).
    fn render(&mut self, committed: &str, tentative: &str) -> String {
        let snapshot = format!("{committed}{tentative}");
        self.last_full = snapshot.clone();
        let raw = self.combine(&snapshot);
        let (converted, _) = self
            .preview_script
            .convert(&raw, "", &self.supported_languages);
        interim_display_transform(&converted, self.spoken_punctuation, self.voice_deletion)
    }

    /// Apply the delete-last-word hotkey to the buffer. Returns the refreshed
    /// display text, or `None` when no live session buffer exists (no
    /// stream, batch model, or the session already finalized).
    fn delete_last_word(&mut self) -> Option<String> {
        if !self.live {
            return None;
        }
        let buffer = self.combine(&self.last_full);
        self.base = remove_trailing_word_from_buffer(&buffer);
        self.raw_seen = self.last_full.clone();
        let raw = self.combine(&self.last_full);
        let (converted, _) = self
            .preview_script
            .convert(&raw, "", &self.supported_languages);
        Some(interim_display_transform(
            &converted,
            self.spoken_punctuation,
            self.voice_deletion,
        ))
    }

    /// Fold the engine's final raw text into the buffer and end the session.
    /// With no manual edits this is exactly the engine text unchanged.
    fn combine_final(&mut self, final_raw: String) -> String {
        let combined = if self.live {
            let keep = common_prefix_len(&self.raw_seen, &final_raw);
            join_raw(&self.base, &final_raw[keep..])
        } else {
            final_raw
        };
        self.end();
        combined
    }
}

/// Routes real-time audio frames to the active streaming worker. Shared between
/// the [`TranscriptionManager`] (opens/closes the route) and the audio recorder's
/// per-frame callback (feeds frames). The recorder holds an `Arc<StreamRouter>`
/// directly, so a frame with no stream pending costs a single relaxed atomic
/// load - no Tauri state lookup, no mutex lock.
pub struct StreamRouter {
    /// Command channel to the active streaming worker, present from
    /// `start_stream` until `finalize_stream`/`cancel_stream`.
    tx: Mutex<Option<mpsc::Sender<StreamCmd>>>,
    /// True while a stream is pending or active (channel is open). The audio
    /// callback checks this first to avoid the mutex lock when no stream runs.
    open: Arc<AtomicBool>,
}

impl StreamRouter {
    fn new() -> Self {
        Self {
            tx: Mutex::new(None),
            open: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Open a fresh command channel for a new streaming session, returning the
    /// receiver the worker should drain. Caller must ensure no prior channel is
    /// still open.
    fn open(&self) -> mpsc::Receiver<StreamCmd> {
        let (tx, rx) = mpsc::channel::<StreamCmd>();
        *self.tx.lock().unwrap() = Some(tx);
        self.open.store(true, Ordering::Relaxed);
        rx
    }

    /// Take the sender out (closing the channel to new feeds). Returns the
    /// sender so the caller can send the final `Finalize`/`Cancel` command.
    fn take(&self) -> Option<mpsc::Sender<StreamCmd>> {
        self.open.store(false, Ordering::Relaxed);
        self.tx.lock().unwrap().take()
    }

    /// Drop the channel and mark closed without sending a final command (used
    /// when the worker exits without a finalize/cancel handshake).
    fn clear(&self) {
        self.open.store(false, Ordering::Relaxed);
        *self.tx.lock().unwrap() = None;
    }

    /// Forward a 16 kHz frame to the active streaming worker. Cheap no-op (a
    /// single relaxed atomic load) when no stream is pending.
    pub fn feed(&self, frame: &[f32]) {
        if !self.open.load(Ordering::Relaxed) {
            return;
        }
        if let Some(tx) = self.tx.lock().unwrap().as_ref() {
            let _ = tx.send(StreamCmd::Feed(frame.to_vec()));
        }
    }

    /// Whether a stream is pending or active.
    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Relaxed)
    }
}

/// In-process ONNX engines. transcribe-cpp models (whisper family, parakeet,
/// custom .bin/.gguf) live in the [`EngineSupervisor`]'s worker process
/// instead, so a native crash or hang there cannot take the app down.
enum OnnxEngine {
    Parakeet(ParakeetModel),
    Moonshine(MoonshineModel),
    MoonshineStreaming(StreamingModel),
    SenseVoice(SenseVoiceModel),
    GigaAM(GigaAMModel),
    Canary(CanaryModel),
    Cohere(CohereModel),
}

/// RAII guard that clears the `is_loading` flag and notifies waiters on drop.
/// Ensures the loading flag is always reset, even on early returns or panics.
pub struct LoadingGuard {
    is_loading: Arc<Mutex<bool>>,
    loading_condvar: Arc<Condvar>,
}

impl Drop for LoadingGuard {
    fn drop(&mut self) {
        // Recover from a poisoned mutex instead of panicking -
        // a panic inside Drop calls abort().
        let mut is_loading = match self.is_loading.lock() {
            Ok(g) => g,
            Err(e) => {
                warn!("Recovered poisoned is_loading mutex during LoadingGuard drop - a panic occurred earlier this session");
                e.into_inner()
            }
        };
        *is_loading = false;
        self.loading_condvar.notify_all();
    }
}

/// RAII guard that clears the streaming worker flags on any worker exit -
/// normal return, early return, or a panic that unwinds the detached worker
/// thread. Tokens prevent an older worker from clearing a newer worker's
/// state if a start/finalize race ever slips through.
struct StreamWorkerGuard {
    worker_id: u64,
    active_stream_worker: Arc<AtomicU64>,
    stream_active: Arc<AtomicBool>,
}

impl Drop for StreamWorkerGuard {
    fn drop(&mut self) {
        if self.active_stream_worker.load(Ordering::Acquire) == self.worker_id {
            self.stream_active.store(false, Ordering::Release);
        }
        let _ = self.active_stream_worker.compare_exchange(
            self.worker_id,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

#[derive(Clone)]
pub struct TranscriptionManager {
    /// The transcribe-cpp engine: owns the worker process, its model, and
    /// recovery from crashes and hangs.
    engine: EngineSupervisor,
    /// The loaded ONNX engine, if the current model is an ONNX one.
    onnx: Arc<Mutex<Option<OnnxEngine>>>,
    model_manager: Arc<ModelManager>,
    app_handle: AppHandle,
    current_model_id: Arc<Mutex<Option<String>>>,
    last_activity: Arc<AtomicU64>,
    shutdown_signal: Arc<AtomicBool>,
    watcher_handle: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
    is_loading: Arc<Mutex<bool>>,
    loading_condvar: Arc<Condvar>,
    reload_model_on_next_use: Arc<AtomicBool>,
    /// Routes real-time audio frames to the active streaming worker; see
    /// [`StreamRouter`]. Shared with the audio recorder so per-frame feeds skip
    /// Tauri state and the manager lock.
    router: Arc<StreamRouter>,
    /// True only while a transcribe-cpp stream is actually in flight (set by
    /// the worker once the stream begins). Used for overlay/UI decisions.
    stream_active: Arc<AtomicBool>,
    /// Streaming uses three independent flags: router open = frames should
    /// route, worker active = no second worker may start, stream active = UI
    /// should show a live session.
    ///
    /// Monotonic id source for stream workers; zero means "no worker".
    next_stream_worker_id: Arc<AtomicU64>,
    /// Nonzero while a stream worker exists, even if it has not leased the engine
    /// yet. This prevents a second worker from starting after finalize/cancel
    /// closes the router but before the first worker has fully exited.
    active_stream_worker: Arc<AtomicU64>,
    /// The live dictation buffer of the current streaming session: renders
    /// interim overlay text, absorbs manual hotkey edits, and folds into the
    /// finalize path. See [`StreamSessionBuffer`].
    session_buffer: Arc<Mutex<StreamSessionBuffer>>,
}

impl TranscriptionManager {
    pub fn new(app_handle: &AppHandle, model_manager: Arc<ModelManager>) -> Result<Self> {
        let manager = Self {
            engine: EngineSupervisor::new(!transcribe_gpu_disabled_for_host()),
            onnx: Arc::new(Mutex::new(None)),
            model_manager,
            app_handle: app_handle.clone(),
            current_model_id: Arc::new(Mutex::new(None)),
            last_activity: Arc::new(AtomicU64::new(Self::now_ms())),
            shutdown_signal: Arc::new(AtomicBool::new(false)),
            watcher_handle: Arc::new(Mutex::new(None)),
            is_loading: Arc::new(Mutex::new(false)),
            loading_condvar: Arc::new(Condvar::new()),
            reload_model_on_next_use: Arc::new(AtomicBool::new(false)),
            router: Arc::new(StreamRouter::new()),
            stream_active: Arc::new(AtomicBool::new(false)),
            next_stream_worker_id: Arc::new(AtomicU64::new(1)),
            active_stream_worker: Arc::new(AtomicU64::new(0)),
            session_buffer: Arc::new(Mutex::new(StreamSessionBuffer::default())),
        };

        // Start the idle watcher
        {
            let app_handle_cloned = app_handle.clone();
            let manager_cloned = manager.clone();
            let shutdown_signal = manager.shutdown_signal.clone();
            let handle = thread::spawn(move || {
                debug!("Idle watcher thread started");
                while !shutdown_signal.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_secs(10)); // Check every 10 seconds

                    // Check shutdown signal again after sleep
                    if shutdown_signal.load(Ordering::Relaxed) {
                        break;
                    }

                    let settings = get_settings(&app_handle_cloned);
                    let timeout = settings.model_unload_timeout;

                    // Skip Immediately - that variant is handled by
                    // maybe_unload_immediately() after each transcription.
                    // Treating it as 0s here would unload the model mid-recording.
                    if timeout == ModelUnloadTimeout::Immediately {
                        continue;
                    }

                    // While recording, keep the idle timer fresh so the
                    // model is never unloaded mid-session.
                    let is_recording = app_handle_cloned
                        .try_state::<Arc<AudioRecordingManager>>()
                        .is_some_and(|a| a.is_recording());
                    if is_recording {
                        manager_cloned.touch_activity();
                        continue;
                    }

                    if let Some(limit_seconds) = timeout.to_seconds() {
                        let last = manager_cloned.last_activity.load(Ordering::Relaxed);
                        let now_ms = TranscriptionManager::now_ms();
                        let idle_ms = now_ms.saturating_sub(last);
                        let limit_ms = limit_seconds * 1000;

                        if idle_ms > limit_ms {
                            // idle -> unload
                            if manager_cloned.is_model_loaded() {
                                let unload_start = std::time::Instant::now();
                                info!(
                                    "Model idle for {}s (limit: {}s), unloading",
                                    idle_ms / 1000,
                                    limit_seconds
                                );
                                match manager_cloned.unload_model() {
                                    Ok(()) => {
                                        let unload_duration = unload_start.elapsed();
                                        info!(
                                            "Model unloaded due to inactivity (took {}ms)",
                                            unload_duration.as_millis()
                                        );
                                    }
                                    Err(e) => {
                                        error!("Failed to unload idle model: {}", e);
                                    }
                                }
                            }
                        }
                    }
                }
                debug!("Idle watcher thread shutting down gracefully");
            });
            *manager.watcher_handle.lock().unwrap() = Some(handle);
        }

        Ok(manager)
    }

    /// Lock the ONNX engine mutex, recovering from poison if a previous transcription panicked.
    fn lock_onnx(&self) -> MutexGuard<'_, Option<OnnxEngine>> {
        self.onnx.lock().unwrap_or_else(|poisoned| {
            warn!("Engine mutex was poisoned by a previous panic, recovering");
            poisoned.into_inner()
        })
    }

    pub fn is_model_loaded(&self) -> bool {
        // A transcribe-cpp model stays loaded while it is busy, so a batch
        // run or stream in progress never reads as "unloaded".
        self.engine.loaded().is_some() || self.lock_onnx().is_some()
    }

    /// Footprint of the currently-resident model, credited back to the
    /// free-memory reading by the gate in `load_model_with_device` (those
    /// pages are freed before the replacement model's peak).
    ///
    /// TranscribeCpp: the worker process's measured RSS when both a model is
    /// loaded and the pid probe succeeds (either unavailable → no credit -
    /// conservative). In-process ONNX engines: the resident model's
    /// `size_mb`-derived estimate (the same estimate class the gate uses for
    /// the incoming model). Nothing resident → 0.
    fn resident_model_footprint_bytes(&self) -> u64 {
        if self.engine.loaded().is_some() {
            let measured = self.engine.worker_pid().and_then(memory::rss_bytes_for_pid);
            return memory::resident_credit(measured, None);
        }
        if self.lock_onnx().is_some() {
            let estimate = self
                .get_current_model()
                .and_then(|id| self.model_manager.get_model_info(&id))
                .map(|info| info.size_mb.saturating_mul(1024 * 1024));
            return memory::resident_credit(None, estimate);
        }
        0
    }

    /// Resident model's footprint for the tray status line (spec F4):
    /// `Some((bytes, measured))` when a model is resident - `measured` is
    /// `true` only for the transcribe-cpp worker's RSS; `false` marks the
    /// size-derived estimate (in-process ONNX engines, or the worker's RSS
    /// could not be read). `None` when nothing is resident or no estimate
    /// resolves - callers omit the segment. Distinct from the gate's
    /// [`Self::resident_model_footprint_bytes`] above, which credits
    /// TranscribeCpp by measured RSS only (conservative for load decisions).
    pub fn resident_model_footprint(&self) -> Option<(u64, bool)> {
        let estimate = || {
            self.get_current_model()
                .and_then(|id| self.model_manager.get_model_info(&id))
                .map(|info| info.size_mb.saturating_mul(1024 * 1024))
        };
        if self.engine.loaded().is_some() {
            if let Some(rss) = self.engine.worker_pid().and_then(memory::rss_bytes_for_pid) {
                return Some((rss, true));
            }
            // RSS read failed → fall back to the ~-estimate (spec F4).
            return estimate().map(|e| (e, false));
        }
        if self.lock_onnx().is_some() {
            return estimate().map(|e| (e, false));
        }
        None
    }

    /// Downloaded-model candidates for the RAM auto-fallback (see the memory
    /// gate in `load_model_with_device_internal`): every downloaded model
    /// except the one that just failed: catalog entries at their editorial
    /// rank, user-added Hugging Face and custom models after them (see
    /// [`fallback_rank`]), with the same size-derived footprint the gate
    /// compares. Nothing is ever downloaded for this list: it only inventories
    /// what is already on disk.
    fn fallback_candidates(&self, failed_id: &str) -> Vec<memory::FallbackCandidate> {
        fallback_candidate_list(&self.model_manager.get_available_models(), failed_id)
    }

    /// Accelerator changes should not disturb the current transcription. Mark
    /// the cached engine stale; the next model-use path reloads it with the
    /// latest settings.
    pub fn reload_model_on_next_use(&self) {
        self.reload_model_on_next_use.store(true, Ordering::Release);
    }

    /// An explicit transcribe.cpp accelerator or GPU device change is the
    /// user's way of trying the GPU again: forget every GPU failure so far,
    /// so the next load may use (and list) the GPU. The ONNX accelerator does
    /// not affect this.
    pub fn retry_transcribe_gpu(&self, reason: &str) {
        self.engine.retry_gpu(reason);
    }

    /// The engine gives up on a model by itself when its worker can't be
    /// restarted (e.g. the file is gone, or it crashes on CPU too). Keep the
    /// loaded model id and the UI in step, so the next use loads the model
    /// again and reports why it can't.
    fn forget_model_if_engine_dropped(&self, model_id: &str, error: &str) {
        if self.engine.loaded().is_some() {
            return;
        }
        {
            let mut current_model = self.current_model_id.lock().unwrap();
            if current_model.as_deref() != Some(model_id) {
                return;
            }
            *current_model = None;
        }
        warn!(
            "Transcription model '{}' is no longer loaded: {}",
            model_id, error
        );
        let _ = self.app_handle.emit(
            "model-state-changed",
            ModelStateEvent {
                event_type: "unloaded".to_string(),
                model_id: None,
                model_name: None,
                error: Some(error.to_string()),
            },
        );
    }

    /// Atomically check whether a model load is in progress and, if not, mark
    /// one as starting. Returns a [`LoadingGuard`] whose [`Drop`] impl will
    /// clear the flag and wake waiters. Returns `None` if a load is already in
    /// progress.
    pub fn try_start_loading(&self) -> Option<LoadingGuard> {
        let mut is_loading = self.is_loading.lock().unwrap();
        if *is_loading {
            return None;
        }
        *is_loading = true;
        Some(LoadingGuard {
            is_loading: self.is_loading.clone(),
            loading_condvar: self.loading_condvar.clone(),
        })
    }

    /// Unload the model. Returns once the transcribe-cpp worker has exited,
    /// which frees all of its memory (e.g. before its file is deleted).
    pub fn unload_model(&self) -> Result<()> {
        let unload_start = std::time::Instant::now();
        debug!("Starting to unload model");
        self.begin_unload().wait();
        debug!(
            "Model unloaded manually (took {}ms)",
            unload_start.elapsed().as_millis()
        );
        Ok(())
    }

    /// Unload the model without waiting for the transcribe-cpp worker to
    /// exit, so it never blocks the caller (e.g. the event loop) behind a
    /// transcription in progress. The model reads as unloaded at once.
    pub fn request_unload(&self) {
        drop(self.begin_unload());
    }

    /// Every state change of an unload, done now on the caller's thread: a
    /// dictation started right after sees no model and loads it again, and
    /// its load is queued behind this unload, so the unload can never affect
    /// it. Only the worker's exit is left to wait for.
    fn begin_unload(&self) -> Unloading {
        let unloading = self.engine.unload();
        // Dropping an ONNX engine frees its resources.
        *self.lock_onnx() = None;
        {
            let mut current_model = self.current_model_id.lock().unwrap();
            *current_model = None;
        }

        // Emit unloaded event
        let _ = self.app_handle.emit(
            "model-state-changed",
            ModelStateEvent {
                event_type: "unloaded".to_string(),
                model_id: None,
                model_name: None,
                error: None,
            },
        );
        unloading
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    /// Reset the idle timer to now.
    fn touch_activity(&self) {
        self.last_activity.store(Self::now_ms(), Ordering::Relaxed);
    }

    /// Unloads the model immediately if the setting is enabled and the model
    /// is loaded. Never waits for the worker to exit, so it is safe on any
    /// thread, including the event loop.
    pub fn maybe_unload_immediately(&self, context: &str) {
        let settings = get_settings(&self.app_handle);
        if settings.model_unload_timeout == ModelUnloadTimeout::Immediately
            && self.is_model_loaded()
        {
            info!("Immediately unloading model after {}", context);
            self.request_unload();
        }
    }

    pub fn load_model(&self, model_id: &str) -> Result<()> {
        self.load_model_with_device(model_id, None)
    }

    /// Like [`load_model`](Self::load_model), but lets a caller hard-select the
    /// compute device for this one load by its transcribe-cpp device
    /// registry index (the index shown by `--list-devices`). `None` keeps the
    /// persisted accelerator setting (which may be Auto). Only affects
    /// transcribe-cpp (whisper-family) models; the selection is not persisted.
    pub fn load_model_with_device(
        &self,
        model_id: &str,
        device_index: Option<usize>,
    ) -> Result<()> {
        self.load_model_with_device_internal(model_id, device_index, true)
    }

    /// The real load path. `allow_fallback` gates the RAM auto-fallback: the
    /// top-level (user-initiated) load may fall back to a smaller
    /// already-downloaded model when the memory gate refuses the selection;
    /// the fallback load itself may not cascade (the resolver already picked
    /// the best fit - if even that no longer fits, refusing is correct).
    fn load_model_with_device_internal(
        &self,
        model_id: &str,
        device_index: Option<usize>,
        allow_fallback: bool,
    ) -> Result<()> {
        apply_accelerator_settings(&self.app_handle);

        let load_start = std::time::Instant::now();
        debug!("Starting to load model: {}", model_id);

        // Emit loading started event
        let _ = self.app_handle.emit(
            "model-state-changed",
            ModelStateEvent {
                event_type: "loading_started".to_string(),
                model_id: Some(model_id.to_string()),
                model_name: None,
                error: None,
            },
        );

        let model_info = match self.model_manager.get_model_info(model_id) {
            Some(model_info) => model_info,
            None => {
                let error_msg = format!("Model not found: {}", model_id);
                let _ = self.app_handle.emit(
                    "model-state-changed",
                    ModelStateEvent {
                        event_type: "loading_failed".to_string(),
                        model_id: Some(model_id.to_string()),
                        model_name: None,
                        error: Some(error_msg.clone()),
                    },
                );
                return Err(anyhow::anyhow!(error_msg));
            }
        };

        // Every failure after loading starts must emit a terminal event so the
        // frontend can never remain in its loading state.
        let emit_loading_failed = |error_msg: &str| {
            let _ = self.app_handle.emit(
                "model-state-changed",
                ModelStateEvent {
                    event_type: "loading_failed".to_string(),
                    model_id: Some(model_id.to_string()),
                    model_name: Some(model_info.name.clone()),
                    error: Some(error_msg.to_string()),
                },
            );
        };

        if !model_info.is_downloaded {
            let error_msg = "Model not downloaded";
            emit_loading_failed(error_msg);
            return Err(anyhow::anyhow!(error_msg));
        }

        // Memory-pressure gate (spec F3): refuse loads whose forecast
        // footprint cannot fit, BEFORE the current engine is dropped below -
        // a refusal leaves the resident model loaded and transcribing. The
        // outgoing model's footprint is credited back to the free reading
        // because its pages are freed before the new model's peak. A probe
        // failure fails open (gate returns false for `None`).
        if get_settings(&self.app_handle).memory_pressure_guard {
            let forecast = model_info.size_mb.saturating_mul(1024 * 1024);
            let credit = self.resident_model_footprint_bytes();
            let free = memory::available_memory_bytes().map(|f| f.saturating_add(credit));
            if free.is_none() {
                // Spec F3: the fail-open path must log a WARNING so an inert
                // probe is visible on the console (default filter Info), not
                // silently skipped.
                warn!(
                    "memory gate: available-memory probe unavailable, failing open for {}",
                    model_info.name
                );
            }
            if memory::gate_should_refuse(free, forecast, memory::DEFAULT_HEADROOM_BYTES) {
                // RAM auto-fallback: when enabled, load the best
                // ALREADY-DOWNLOADED model that fits instead of failing the
                // dictation. The tray/indicator state follows the model that
                // is actually resident (current_model_id is set to the
                // fallback below), and the frontend gets a one-shot
                // "model-fallback" event to toast the switch. Toggle off (or
                // nothing fitting) keeps the exact refuse-with-toast path.
                if allow_fallback && get_settings(&self.app_handle).auto_fallback {
                    let candidates = self.fallback_candidates(model_id);
                    if let Some(fallback) =
                        memory::resolve_fallback_model(free, &candidates, model_id)
                    {
                        let fallback_name = self
                            .model_manager
                            .get_model_info(&fallback.id)
                            .map(|info| info.name)
                            .unwrap_or_else(|| fallback.id.clone());
                        warn!(
                            "memory gate refused '{}', falling back to '{}' for this load",
                            model_info.name, fallback_name
                        );
                        let _ = self.app_handle.emit(
                            "model-fallback",
                            ModelFallbackEvent {
                                requested_model_name: model_info.name.clone(),
                                fallback_model_name: fallback_name,
                            },
                        );
                        return self.load_model_with_device_internal(
                            &fallback.id,
                            device_index,
                            false,
                        );
                    }
                }
                let gib = 1024.0 * 1024.0 * 1024.0;
                let error_msg = format!(
                    "Not enough free memory for {}: needs ~{:.1} GB, ~{:.1} GB free (guard can be disabled in Settings)",
                    model_info.name,
                    forecast as f64 / gib,
                    free.unwrap_or(0) as f64 / gib
                );
                warn!("memory gate refused a load: {}", error_msg);
                emit_loading_failed(&error_msg);
                return Err(anyhow::anyhow!(error_msg));
            }
        }

        let model_path = self
            .model_manager
            .get_model_path(model_id)
            .inspect_err(|error| emit_loading_failed(&error.to_string()))?;

        // Drop the current engine BEFORE building the new one so the previous
        // model is freed first - avoids holding two models at once (peak memory
        // on large GGUFs). A transcribe-cpp load replaces its worker the same
        // way. Clear the id too: if the new load fails, status should read "no
        // loaded model", not the dropped engine.
        *self.lock_onnx() = None;
        if !matches!(model_info.engine_type, EngineType::TranscribeCpp) {
            self.engine.unload().wait();
        }
        {
            let mut current_model = self.current_model_id.lock().unwrap();
            *current_model = None;
        }

        // Create appropriate engine based on model type

        let loaded_onnx = match model_info.engine_type {
            EngineType::TranscribeCpp => {
                // The backend is chosen at load time. With an explicit
                // `device_index` (the --device-index flag) hard-select that
                // registered device; otherwise re-read the persisted
                // accelerator preference (so an accelerator change marked for
                // reload takes effect here). The worker resolves the selector
                // to a device handle, and rejects an index that isn't a
                // loadable device (a host without GPU access lists none).
                let (backend, device) = match device_index {
                    Some(index) => (Backend::Auto, DeviceSelector::Index(index)),
                    None => {
                        let settings = get_settings(&self.app_handle);
                        let accelerator = settings.transcribe_accelerator;
                        match resolve_gpu_device(
                            accelerator,
                            settings.transcribe_gpu_device.as_deref(),
                        ) {
                            // Backend::Auto accepts an exact GPU device.
                            Some(key) => (Backend::Auto, DeviceSelector::Key(key)),
                            // Without a valid exact device, backend selection
                            // handles the retired generic GPU state and host
                            // CPU guard.
                            None => (select_transcribe_backend(accelerator), DeviceSelector::Auto),
                        }
                    }
                };
                let requested_device = format!("{:?}", device);
                let info = self
                    .engine
                    .load(LoadSpec {
                        path: model_path.clone(),
                        backend,
                        device,
                    })
                    .map_err(|e| {
                        let error_msg = format!("Failed to load model {}: {}", model_id, e);
                        emit_loading_failed(&error_msg);
                        anyhow::anyhow!(error_msg)
                    })?;
                // Reconcile the registry's advertised capabilities with the
                // loaded model's real ones (GGUF metadata) so badges/gating
                // reflect runtime truth, not the pre-download probe. The
                // load-completed event below triggers the frontend refresh.
                let caps = &info.capabilities;
                self.model_manager.set_runtime_capabilities(
                    model_id,
                    caps.supports_streaming,
                    caps.supports_translate,
                    caps.supports_language_detect,
                    caps.languages.clone(),
                );
                // The bound backend may differ from the request (e.g. CPU
                // fallback under Auto or after a GPU crash); log what loaded.
                info!(
                    "Loaded transcribe-cpp model '{}' (requested {:?}, requested device {}, \
                     bound backend '{}', bound device '{}', supports_streaming={}, \
                     supports_translate={}, supports_language_detect={})",
                    model_id,
                    backend,
                    requested_device,
                    info.backend,
                    info.device,
                    caps.supports_streaming,
                    caps.supports_translate,
                    caps.supports_language_detect
                );
                None
            }
            EngineType::Parakeet => {
                let engine =
                    ParakeetModel::load(&model_path, &Quantization::Int8).map_err(|e| {
                        let error_msg =
                            format!("Failed to load parakeet model {}: {}", model_id, e);
                        emit_loading_failed(&error_msg);
                        anyhow::anyhow!(error_msg)
                    })?;
                Some(OnnxEngine::Parakeet(engine))
            }
            EngineType::Moonshine => {
                let engine = MoonshineModel::load(
                    &model_path,
                    MoonshineVariant::Base,
                    &Quantization::default(),
                )
                .map_err(|e| {
                    let error_msg = format!("Failed to load moonshine model {}: {}", model_id, e);
                    emit_loading_failed(&error_msg);
                    anyhow::anyhow!(error_msg)
                })?;
                Some(OnnxEngine::Moonshine(engine))
            }
            EngineType::MoonshineStreaming => {
                let engine = StreamingModel::load(&model_path, 0, &Quantization::default())
                    .map_err(|e| {
                        let error_msg = format!(
                            "Failed to load moonshine streaming model {}: {}",
                            model_id, e
                        );
                        emit_loading_failed(&error_msg);
                        anyhow::anyhow!(error_msg)
                    })?;
                Some(OnnxEngine::MoonshineStreaming(engine))
            }
            EngineType::SenseVoice => {
                let engine =
                    SenseVoiceModel::load(&model_path, &Quantization::Int8).map_err(|e| {
                        let error_msg =
                            format!("Failed to load SenseVoice model {}: {}", model_id, e);
                        emit_loading_failed(&error_msg);
                        anyhow::anyhow!(error_msg)
                    })?;
                Some(OnnxEngine::SenseVoice(engine))
            }
            EngineType::GigaAM => {
                let engine = GigaAMModel::load(&model_path, &Quantization::Int8).map_err(|e| {
                    let error_msg = format!("Failed to load gigaam model {}: {}", model_id, e);
                    emit_loading_failed(&error_msg);
                    anyhow::anyhow!(error_msg)
                })?;
                Some(OnnxEngine::GigaAM(engine))
            }
            EngineType::Canary => {
                let engine = CanaryModel::load(&model_path, &Quantization::Int8).map_err(|e| {
                    let error_msg = format!("Failed to load canary model {}: {}", model_id, e);
                    emit_loading_failed(&error_msg);
                    anyhow::anyhow!(error_msg)
                })?;
                Some(OnnxEngine::Canary(engine))
            }
            EngineType::Cohere => {
                let engine = CohereModel::load(&model_path, &Quantization::Int8).map_err(|e| {
                    let error_msg = format!("Failed to load cohere model {}: {}", model_id, e);
                    emit_loading_failed(&error_msg);
                    anyhow::anyhow!(error_msg)
                })?;
                Some(OnnxEngine::Cohere(engine))
            }
        };

        // Update the current engine and model ID
        *self.lock_onnx() = loaded_onnx;
        {
            let mut current_model = self.current_model_id.lock().unwrap();
            *current_model = Some(model_id.to_string());
        }

        // Reset idle timer so the watcher doesn't immediately unload a just-loaded model
        self.touch_activity();

        // Emit loading completed event
        let _ = self.app_handle.emit(
            "model-state-changed",
            ModelStateEvent {
                event_type: "loading_completed".to_string(),
                model_id: Some(model_id.to_string()),
                model_name: Some(model_info.name.clone()),
                error: None,
            },
        );

        let load_duration = load_start.elapsed();
        debug!(
            "Successfully loaded transcription model: {} (took {}ms)",
            model_id,
            load_duration.as_millis()
        );
        Ok(())
    }

    /// Kicks off the model loading in a background thread if it's not already loaded
    pub fn initiate_model_load(&self) {
        let mut is_loading = self.is_loading.lock().unwrap();
        if *is_loading {
            return;
        }

        let reload_pending = self.reload_model_on_next_use.load(Ordering::Acquire);
        if !reload_pending && self.is_model_loaded() {
            return;
        }

        *is_loading = true;
        let self_clone = self.clone();
        thread::spawn(move || {
            if reload_pending {
                self_clone
                    .reload_model_on_next_use
                    .store(false, Ordering::Release);
            }
            let settings = get_settings(&self_clone.app_handle);
            if let Err(e) = self_clone.load_model(&settings.selected_model) {
                error!("Failed to load model: {}", e);
            }
            let mut is_loading = self_clone.is_loading.lock().unwrap();
            *is_loading = false;
            self_clone.loading_condvar.notify_all();
        });
    }

    pub fn get_current_model(&self) -> Option<String> {
        let current_model = self.current_model_id.lock().unwrap();
        current_model.clone()
    }

    /// The compute backend the currently-loaded engine is bound to, for
    /// diagnostics (e.g. confirming `--device-index` actually bound a GPU rather
    /// than falling back to CPU/auto). transcribe-cpp (whisper-family) reports
    /// its real backend string; ONNX engines report "onnx"; `None` when no
    /// model is loaded.
    pub fn current_backend(&self) -> Option<String> {
        if let Some(info) = self.engine.loaded() {
            return Some(info.backend);
        }
        self.lock_onnx().as_ref().map(|_| "onnx".to_string())
    }

    /// Whether a live streaming run is currently in flight.
    pub fn is_streaming(&self) -> bool {
        self.stream_active.load(Ordering::Acquire)
    }

    /// Shared handle to the stream router, used by the audio recorder to feed
    /// real-time frames without going through Tauri state on every frame.
    pub fn stream_router(&self) -> Arc<StreamRouter> {
        Arc::clone(&self.router)
    }

    /// Begin a live streaming transcription on the held engine's session.
    /// Audio frames pushed via [`StreamRouter::feed`] (captured directly by the
    /// audio recorder) are decoded incrementally and emitted to the overlay as
    /// [`StreamTextEvent`].
    ///
    /// Non-blocking: spawns a worker that waits for any in-progress model load,
    /// verifies the model supports streaming, then begins the stream. If the
    /// model can't stream, the worker idles until finalize/cancel and reports
    /// `None` so the caller falls back to batch transcription. Frames sent
    /// before the stream begins queue on the channel and are not lost.
    pub fn start_stream(&self) {
        if self.router.is_open() || self.active_stream_worker.load(Ordering::Acquire) != 0 {
            warn!("start_stream called while a stream worker is already active");
            return;
        }
        // A previous worker that exited without the finalize/cancel handshake
        // (channel dropped underneath it) must not leave a stale live buffer
        // behind for hotkey edits to write into.
        self.session_buffer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .end();
        let worker_id = self.next_stream_worker_id.fetch_add(1, Ordering::Relaxed);
        if self
            .active_stream_worker
            .compare_exchange(0, worker_id, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            warn!("start_stream lost a race with another stream worker");
            return;
        }
        let rx = self.router.open();
        self.stream_active.store(false, Ordering::Release);

        let manager = self.clone();
        thread::spawn(move || manager.run_stream_worker(rx, worker_id));
    }

    fn run_stream_worker(&self, rx: mpsc::Receiver<StreamCmd>, worker_id: u64) {
        let _worker = StreamWorkerGuard {
            worker_id,
            active_stream_worker: Arc::clone(&self.active_stream_worker),
            stream_active: Arc::clone(&self.stream_active),
        };

        // Wait for any in-progress model load to finish (start_stream races the
        // background load kicked off when recording starts).
        {
            let mut is_loading = self.is_loading.lock().unwrap();
            while *is_loading {
                is_loading = self.loading_condvar.wait(is_loading).unwrap();
            }
        }

        let model_id = self.get_current_model().unwrap_or_default();

        // Only transcribe-cpp models expose streaming; ONNX engines fall back to
        // batch. The loaded model (not the ModelManager copy) is the source of
        // truth for run-path capabilities.
        let Some(info) = self.engine.loaded() else {
            info!(
                "Live preview: model '{}' is not a loaded transcribe-cpp model; \
                 streaming is unavailable, using batch transcription",
                model_id
            );
            self.router.clear();
            drain_until_finalize(rx);
            return;
        };
        let caps = &info.capabilities;
        info!(
            "Live preview: model '{}' arch='{}' variant='{}' supports_streaming={} \
             supports_translate={} languages={:?}",
            model_id,
            info.arch,
            info.variant,
            caps.supports_streaming,
            caps.supports_translate,
            caps.languages,
        );
        if !caps.supports_streaming {
            self.router.clear();
            drain_until_finalize(rx);
            return;
        }
        let languages = caps.languages.clone();

        // Build run options mirroring the offline transcribe-cpp path: task +
        // language gated against what the model actually advertises.
        let settings = get_settings(&self.app_handle);
        let effective_language =
            effective_language_for_model(&settings, self.model_manager.as_ref(), &model_id);
        let run_plan = transcribe_cpp_run_plan(
            settings.translate_to_english,
            &effective_language,
            &languages,
            caps.supports_translate,
        );
        let output_language = resolve_output_language_evidence(
            &settings,
            run_plan.language.as_deref(),
            &languages,
            run_plan.target_language.as_deref() == Some("en"),
        );
        let run_options = RunOptions {
            task: run_plan.task,
            language: run_plan.language,
            target_language: run_plan.target_language,
            ..Default::default()
        };

        // Feed results arrive on the engine's thread; this callback records
        // the snapshot into the session buffer and emits the rendered
        // interim text.
        let preview_script = PreviewScript::new(settings.chinese_script, &output_language);
        let perf = Arc::new(Mutex::new(StreamPerf::new()));
        let session_buffer_for_progress = Arc::clone(&self.session_buffer);
        let on_progress = {
            let perf = Arc::clone(&perf);
            let app_handle = self.app_handle.clone();
            move |progress: StreamProgress| {
                let mut perf = lock_perf(&perf);
                perf.record_compute(progress.elapsed);
                perf.record_update(&progress.update);
                if let Some(text) = progress.text {
                    perf.record_emit();
                    let display = {
                        let mut session = session_buffer_for_progress
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        session.render(&text.committed, &text.tentative)
                    };
                    // The whole displayed text is emitted as the committed
                    // part: the interim transform runs over the full raw
                    // buffer, so the committed/tentative visual split cannot
                    // be preserved exactly across punctuation joins and
                    // deletions. The model's own rewrites still surface
                    // because the display is recomputed from every snapshot.
                    emit_stream_text(&app_handle, &display, "");
                }
                perf.maybe_log();
            }
        };

        // The session buffer goes live before the engine stream starts, so
        // no interim callback can race past `begin`. Toggles are captured
        // here (once per session), matching `PreviewScript`.
        self.session_buffer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .begin(
                preview_script,
                settings.spoken_punctuation,
                settings.voice_deletion_commands,
                &languages,
            );

        // Run the stream in the engine's worker process. Feeds are queued
        // without waiting; a crashed or hung worker makes finalize report no
        // result, and the caller falls back to batch transcription. StreamOptions::default()
        // uses CommitPolicy::Auto and lets the family pick its own streaming
        // strategy (no family-specific ext).
        let stream =
            match self
                .engine
                .start_stream(run_options, StreamOptions::default(), on_progress)
            {
                Ok(stream) => stream,
                Err(e) => {
                    error!("Failed to begin stream: {}", e);
                    self.session_buffer
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .end();
                    self.forget_model_if_engine_dropped(&model_id, &e.to_string());
                    drain_until_finalize(rx);
                    return;
                }
            };

        self.stream_active.store(true, Ordering::Release);
        self.touch_activity();
        info!(
            "Live streaming transcription started (model '{}', backend '{}')",
            model_id, info.backend
        );

        while let Ok(cmd) = rx.recv() {
            match cmd {
                StreamCmd::Feed(pcm) => {
                    self.touch_activity();
                    lock_perf(&perf).record_feed(pcm.len());
                    stream.feed(pcm);
                }
                StreamCmd::Finalize(reply) => {
                    // In auto mode the model's own LID is the best remaining
                    // evidence; the snapshot is only materialized when it can
                    // change the outcome.
                    let want_language = matches!(output_language, OutputLanguageEvidence::Unknown);
                    let result = match stream.finalize(want_language) {
                        // After finalize the committed prefix holds the full
                        // text; display() = committed + tentative is the safe read.
                        Ok(Some(finalized)) => {
                            let mut perf = lock_perf(&perf);
                            perf.record_compute(finalized.elapsed);
                            perf.record_update(&finalized.update);
                            let output_language = match &output_language {
                                OutputLanguageEvidence::Unknown => with_model_detected_language(
                                    OutputLanguageEvidence::Unknown,
                                    finalized.language,
                                ),
                                resolved => resolved.clone(),
                            };
                            Ok(Some(FinalizedStreamText {
                                text: finalized.text.full,
                                output_language,
                                supported_languages: languages.clone(),
                            }))
                        }
                        Ok(None) => {
                            error!(
                                "Live transcription could not be finalized; \
                                 falling back to batch transcription"
                            );
                            self.forget_model_if_engine_dropped(
                                &model_id,
                                "live transcription failed and the model could not be restarted",
                            );
                            Ok(None)
                        }
                        Err(e) => {
                            info!("Live transcription finalize stopped: {}", e);
                            Err(e)
                        }
                    };
                    let chars = match &result {
                        Ok(Some(finalized)) => finalized.text.len(),
                        _ => 0,
                    };
                    lock_perf(&perf).log_finalized(chars);
                    let _ = reply.send(result);
                    return;
                }
                // Dropping the stream handle resets the worker's stream.
                StreamCmd::Cancel => return,
            }
        }
        // The channel closed without finalize/cancel; dropping the stream
        // handle resets the worker's stream. `_worker` drops after, clearing
        // this worker's flags.
    }

    /// Return the taken ONNX engine to the mutex, unless the model was switched
    /// or unloaded during transcription (in which case the stale engine is dropped).
    fn return_engine(&self, engine: OnnxEngine, expected_model_id: &str) {
        let still_current =
            self.current_model_id.lock().unwrap().as_deref() == Some(expected_model_id);
        if still_current {
            *self.lock_onnx() = Some(engine);
        } else {
            info!(
                "Model changed/unloaded during transcription; dropping stale engine (was '{}')",
                expected_model_id
            );
            // `engine` drops here, freeing its resources.
        }
    }

    /// Flush the active stream and return its final, post-filtered text.
    ///
    /// `Ok(None)` means no usable stream result exists and the caller should
    /// fall back to batch transcription. `Err` means the transcription was
    /// cancelled. There is no timeout here: waiting behind a model load is a
    /// queue position, not a failure (#1841), and the engine bounds the
    /// finalize itself by the audio it still has to decode.
    pub fn finalize_stream(&self) -> Result<Option<String>> {
        self.finalize_stream_with_passes(false)
    }

    /// Command-mode variant of [`Self::finalize_stream`]: the finalized text
    /// is returned raw. None of the normal text passes (spoken punctuation,
    /// voice deletion, terminal fallback, custom words, fillers) may touch a
    /// command transcript, because every word is a command token and the
    /// parser owns all interpretation.
    pub fn finalize_stream_for_commands(&self) -> Result<Option<String>> {
        self.finalize_stream_with_passes(true)
    }

    fn finalize_stream_with_passes(&self, command_mode: bool) -> Result<Option<String>> {
        let Some(tx) = self.router.take() else {
            return Ok(None);
        };
        let (reply_tx, reply_rx) = mpsc::channel();
        if tx.send(StreamCmd::Finalize(reply_tx)).is_err() {
            return Ok(None);
        }
        let finalized = match reply_rx.recv() {
            Ok(Ok(Some(finalized))) => finalized,
            Ok(Ok(None)) | Err(_) => return Ok(None),
            Ok(Err(e)) => return Err(e.into()),
        };

        // Fold the engine's final raw text together with any manual
        // session-buffer edits (delete-last-word hotkey). The finalize path
        // then transforms this raw transcript exactly once through the
        // canonical pipeline; it is never fed the interim-displayed text.
        // Idempotence argument: the interim display transform is recomputed
        // from the raw buffer on every tick and never writes back into it
        // (the engine accumulator and `base` stay raw-domain), so no text
        // pass can run twice over the same words. The interim transform is
        // additionally idempotent on its own output (tested), but the
        // architecture does not rely on that.
        let final_raw = self
            .session_buffer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .combine_final(finalized.text);

        let filtered = if command_mode {
            final_raw
        } else {
            let settings = get_settings(&self.app_handle);
            // Streaming models do not receive a decode prompt, so custom words
            // always go through the shared fuzzy post-correction path.
            post_process_transcription_text(
                final_raw,
                &settings,
                false,
                &finalized.output_language,
                &finalized.supported_languages,
            )
        };

        self.maybe_unload_immediately("streaming transcription");
        Ok(Some(filtered))
    }

    /// Apply the assignable delete-last-word hotkey to the live session
    /// buffer of an active streaming dictation. Returns `true` when a live
    /// buffer was edited and the overlay refreshed through the regular
    /// interim-update event; `false` when there is no live buffer to edit
    /// (batch model, stream not begun, or the session already finalized).
    pub fn apply_session_buffer_word_deletion(&self) -> bool {
        let mut session = self
            .session_buffer
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match session.delete_last_word() {
            Some(display) => {
                let _ = StreamTextEvent {
                    committed: display,
                    tentative: String::new(),
                }
                .emit(&self.app_handle);
                true
            }
            None => false,
        }
    }

    /// Abandon any active stream without producing text (e.g. on cancel).
    pub fn cancel_stream(&self) {
        if let Some(tx) = self.router.take() {
            let _ = tx.send(StreamCmd::Cancel);
        }
        self.stream_active.store(false, Ordering::Release);
        self.session_buffer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .end();
    }

    /// Stop the transcription in progress (a batch run or stream finalize),
    /// and any queued behind it, for an explicit user cancel. The worker doing
    /// it is stopped; the model stays loaded and the next use starts a fresh
    /// worker for it.
    pub fn cancel_transcription(&self) {
        self.engine.cancel();
    }

    /// Emit a working-phase event to the streaming overlay (spinner + label).
    pub fn emit_stream_working(&self, kind: StreamWorkKind) {
        let _ = StreamPhaseEvent {
            phase: StreamPhase::Working,
            kind: Some(kind),
        }
        .emit(&self.app_handle);
    }

    pub fn transcribe(&self, audio: Vec<f32>) -> Result<String> {
        self.transcribe_with_model(audio).map(|(text, _)| text)
    }

    /// Transcribe and report which model id actually produced the text: the
    /// resident model at run time - after a RAM auto-fallback this is the
    /// fallback, not the persisted selection. History entries record it so a
    /// mid-dictation model switch stays auditable. Returns `(text, model_id)`;
    /// the model id is empty when unknown.
    pub fn transcribe_with_model(&self, audio: Vec<f32>) -> Result<(String, String)> {
        self.transcribe_audio(audio, false)
    }

    /// Command-mode variant of [`Self::transcribe_with_model`]: the raw
    /// transcript is returned with every text pass skipped, because the
    /// command parser owns all interpretation of the words.
    pub fn transcribe_with_model_for_commands(&self, audio: Vec<f32>) -> Result<(String, String)> {
        self.transcribe_audio(audio, true)
    }

    fn transcribe_audio(&self, audio: Vec<f32>, command_mode: bool) -> Result<(String, String)> {
        #[cfg(debug_assertions)]
        if std::env::var("HANDY_FORCE_TRANSCRIPTION_FAILURE").is_ok() {
            return Err(anyhow::anyhow!(
                "Simulated transcription failure (HANDY_FORCE_TRANSCRIPTION_FAILURE)"
            ));
        }

        // Update last activity timestamp
        self.touch_activity();

        let st = std::time::Instant::now();
        let audio_len = audio.len();

        debug!("Audio vector length: {}", audio_len);

        if audio.is_empty() {
            debug!("Empty audio vector");
            self.maybe_unload_immediately("empty audio");
            return Ok((String::new(), self.get_current_model().unwrap_or_default()));
        }

        // Check if model is loaded, if not try to load it
        {
            // If the model is loading, wait for it to complete.
            let mut is_loading = self.is_loading.lock().unwrap();
            while *is_loading {
                is_loading = self.loading_condvar.wait(is_loading).unwrap();
            }

            if !self.is_model_loaded() {
                return Err(anyhow::anyhow!("Model is not loaded for transcription."));
            }
        }

        // Get current settings for configuration
        let settings = get_settings(&self.app_handle);

        // Validate selected language against the model's supported languages.
        // If the language isn't supported, fall back to "auto" to prevent errors.
        // Validate against the model that's actually loaded (which can differ
        // from settings.selected_model when a caller loaded a specific model -
        // e.g. the --transcribe-file path's --model), not the persisted
        // selection.
        let active_model = self
            .get_current_model()
            .unwrap_or_else(|| settings.selected_model.clone());
        // Resolve the persisted language *intent* into the language this model
        // will actually use. The coercion is capability-aware (a must-pick model
        // never receives "auto") and computed fresh here - it is never written
        // back to settings, so the intent survives switching models and back.
        let validated_language =
            effective_language_for_model(&settings, self.model_manager.as_ref(), &active_model);
        if validated_language != settings.selected_language {
            debug!(
                "Language intent '{}' resolved to '{}' for model '{}'",
                settings.selected_language, validated_language, active_model
            );
        }

        // Perform transcription with the appropriate engine.
        let run = match self.engine.loaded() {
            Some(info) => {
                self.transcribe_cpp(info, audio, &settings, &validated_language, &active_model)?
            }
            None => self.transcribe_onnx(&audio, &settings, &validated_language, &active_model)?,
        };

        let output_language = with_model_detected_language(
            resolve_output_language_evidence(
                &settings,
                run.applied_language_hint.as_deref(),
                &run.languages,
                run.output_was_translated,
            ),
            run.model_detected_language,
        );
        debug!("Output language evidence: {:?}", output_language);

        // Command mode skips every text pass: the transcript is a command
        // program, not prose, and the parser must see the raw ASR words.
        let filtered_result = if command_mode {
            run.text
        } else {
            // Apply fuzzy word correction if custom words are configured, UNLESS the
            // words were already handed to the model as an initial prompt (whisper
            // family). We don't pass a prompt to non-whisper models (it requires
            // the whisper-kind run extension), so they still get fuzzy correction here,
            // same as the ONNX engines.
            post_process_transcription_text(
                run.text,
                &settings,
                run.model_is_whisper,
                &output_language,
                &run.languages,
            )
        };

        let et = std::time::Instant::now();
        let translation_note = if settings.translate_to_english {
            " (translated)"
        } else {
            ""
        };
        // Real-time factor. Input PCM is 16 kHz mono, so audio length in seconds
        // is samples / 16000. `speedup` is audio_secs / elapsed_secs - e.g. 4.00x
        // means transcribed 4x faster than real time
        let elapsed_secs = (et - st).as_secs_f64();
        let audio_secs = audio_len as f64 / 16_000.0;
        let speedup = real_time_factor(audio_secs, elapsed_secs);
        info!(
            "Transcription completed in {:.2}s for {:.2}s of audio ({:.2}x real-time){}",
            elapsed_secs, audio_secs, speedup, translation_note
        );

        let final_result = filtered_result;

        if final_result.is_empty() {
            info!("Transcription result is empty");
        } else {
            info!(
                "Transcription result: {}",
                crate::utils::redact_text(&final_result)
            );
        }

        self.maybe_unload_immediately("transcription");

        Ok((final_result, active_model))
    }

    /// transcribe-cpp: the model runs in the engine's worker process, which
    /// recovers from crashes and hangs itself. The loaded model's live
    /// capabilities (cheap GGUF-metadata reads) are the source of truth, not
    /// the ModelManager copy. The whisper run extension is kind-tagged, so
    /// non-whisper archs (parakeet, voxtral, …) reject it with INVALID_ARG;
    /// attach it - and translate - only where supported.
    fn transcribe_cpp(
        &self,
        info: LoadedInfo,
        audio: Vec<f32>,
        settings: &AppSettings,
        validated_language: &str,
        active_model: &str,
    ) -> Result<RunOutcome> {
        // Whether the model advertises Feature::InitialPrompt. Informational
        // (logged below); the whisper run extension and the fuzzy-correction
        // skip are gated on `model_is_whisper` instead, since non-whisper
        // archs can advertise the feature while rejecting the whisper-kind
        // extension.
        let model_takes_initial_prompt = info.supports_initial_prompt;
        let model_is_whisper = info.arch == "whisper";
        let model_supports_translate = info.capabilities.supports_translate;
        let languages = info.capabilities.languages;
        debug!(
            "transcribe-cpp model '{}' on '{}': initial_prompt={}, translate={}, languages={:?}",
            settings.selected_model,
            info.backend,
            model_takes_initial_prompt,
            model_supports_translate,
            languages
        );

        // Custom words become the initial prompt ONLY for models that accept
        // one (whisper family). Attaching the whisper run extension to a
        // non-whisper arch is rejected with INVALID_ARG, so skip it there and
        // let the fuzzy post-correction handle custom words instead.
        let family = if settings.custom_words.is_empty() || !model_is_whisper {
            None
        } else {
            Some(RunExtension::Whisper(WhisperRunOptions {
                initial_prompt: Some(settings.custom_words.join(", ")),
                ..Default::default()
            }))
        };

        let run_plan = transcribe_cpp_run_plan(
            settings.translate_to_english,
            validated_language,
            &languages,
            model_supports_translate,
        );
        let output_was_translated = run_plan.target_language.as_deref() == Some("en");
        let applied_language_hint = run_plan.language.clone();

        let run_options = RunOptions {
            task: run_plan.task,
            language: run_plan.language,
            target_language: run_plan.target_language,
            family,
            ..Default::default()
        };

        debug!(
            "transcribe-cpp run: task={:?}, language={:?}, initial_prompt={}",
            run_options.task,
            run_options.language,
            run_options.family.is_some()
        );

        let transcript = match self.engine.transcribe(audio, run_options) {
            Ok(transcript) => transcript,
            Err(EngineError::Cancelled) => return Err(EngineError::Cancelled.into()),
            Err(e) => {
                self.forget_model_if_engine_dropped(active_model, &e.to_string());
                return Err(anyhow::anyhow!(
                    "transcribe-cpp transcription failed: {}",
                    e
                ));
            }
        };
        Ok(RunOutcome {
            text: transcript.text,
            languages,
            applied_language_hint,
            output_was_translated,
            // Whisper's audio-based LID (auto mode only; `None` when a
            // language hint was passed).
            model_detected_language: transcript.language,
            model_is_whisper,
        })
    }

    /// In-process ONNX engines.
    fn transcribe_onnx(
        &self,
        audio: &[f32],
        settings: &AppSettings,
        validated_language: &str,
        active_model: &str,
    ) -> Result<RunOutcome> {
        let languages = self
            .model_manager
            .get_model_info(active_model)
            .map(|info| info.supported_languages)
            .unwrap_or_default();
        let mut output_was_translated = false;
        let mut applied_language_hint: Option<String> = None;

        // Take the engine out so we own it during transcription. If the
        // engine panics, we simply don't put it back (effectively unloading
        // it) instead of poisoning the mutex. No lock is held during the
        // engine call.
        let mut engine = match self.lock_onnx().take() {
            Some(e) => e,
            None => {
                return Err(anyhow::anyhow!(
                    "Model failed to load after auto-load attempt. Please check your model settings."
                ));
            }
        };

        // We use catch_unwind to prevent engine panics from poisoning the
        // mutex, which would make the app hang indefinitely on subsequent
        // operations.
        let transcribe_result = catch_unwind(AssertUnwindSafe(|| -> Result<String> {
            match &mut engine {
                OnnxEngine::Parakeet(parakeet_engine) => {
                    let params = ParakeetParams {
                        timestamp_granularity: Some(TimestampGranularity::Segment),
                        ..Default::default()
                    };
                    parakeet_engine
                        .transcribe_with(audio, &params)
                        .map(|r| r.text)
                        .map_err(|e| anyhow::anyhow!("Parakeet transcription failed: {}", e))
                }
                OnnxEngine::Moonshine(moonshine_engine) => moonshine_engine
                    .transcribe(audio, &TranscribeOptions::default())
                    .map(|r| r.text)
                    .map_err(|e| anyhow::anyhow!("Moonshine transcription failed: {}", e)),
                OnnxEngine::MoonshineStreaming(streaming_engine) => streaming_engine
                    .transcribe(audio, &TranscribeOptions::default())
                    .map(|r| r.text)
                    .map_err(|e| {
                        anyhow::anyhow!("Moonshine streaming transcription failed: {}", e)
                    }),
                OnnxEngine::SenseVoice(sense_voice_engine) => {
                    let language = match validated_language {
                        "zh" => Some("zh".to_string()),
                        "en" => Some("en".to_string()),
                        "ja" => Some("ja".to_string()),
                        "ko" => Some("ko".to_string()),
                        "yue" => Some("yue".to_string()),
                        _ => None,
                    };
                    applied_language_hint = language.clone();
                    let params = SenseVoiceParams {
                        language,
                        use_itn: Some(true),
                    };
                    sense_voice_engine
                        .transcribe_with(audio, &params)
                        .map(|r| r.text)
                        .map_err(|e| anyhow::anyhow!("SenseVoice transcription failed: {}", e))
                }
                OnnxEngine::GigaAM(gigaam_engine) => gigaam_engine
                    .transcribe(audio, &TranscribeOptions::default())
                    .map(|r| r.text)
                    .map_err(|e| anyhow::anyhow!("GigaAM transcription failed: {}", e)),
                OnnxEngine::Canary(canary_engine) => {
                    output_was_translated = settings.translate_to_english;
                    let lang = if validated_language == "auto" {
                        None
                    } else {
                        Some(validated_language.to_string())
                    };
                    applied_language_hint = lang.clone();
                    let options = TranscribeOptions {
                        language: lang,
                        translate: settings.translate_to_english,
                        ..Default::default()
                    };
                    canary_engine
                        .transcribe(audio, &options)
                        .map(|r| r.text)
                        .map_err(|e| anyhow::anyhow!("Canary transcription failed: {}", e))
                }
                OnnxEngine::Cohere(cohere_engine) => {
                    let lang = if validated_language == "auto" {
                        None
                    } else {
                        Some(validated_language.to_string())
                    };
                    applied_language_hint = lang.clone();
                    let options = TranscribeOptions {
                        language: lang,
                        ..Default::default()
                    };
                    cohere_engine
                        .transcribe(audio, &options)
                        .map(|r| r.text)
                        .map_err(|e| anyhow::anyhow!("Cohere transcription failed: {}", e))
                }
            }
        }));

        let text = match transcribe_result {
            Ok(inner_result) => {
                // Success or normal error: return the engine unless a model
                // switch/unload invalidated it while it was in use.
                self.return_engine(engine, active_model);
                inner_result?
            }
            Err(panic_payload) => {
                // Engine panicked - do NOT put it back (it's in an unknown state).
                // The engine is dropped here, effectively unloading it.
                let panic_msg = panic_payload_message(panic_payload.as_ref());
                error!(
                    "Transcription engine panicked: {}. Model has been unloaded.",
                    panic_msg
                );

                // Clear the model ID so it will be reloaded on next attempt
                {
                    let mut current_model = self
                        .current_model_id
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    *current_model = None;
                }

                let _ = self.app_handle.emit(
                    "model-state-changed",
                    ModelStateEvent {
                        event_type: "unloaded".to_string(),
                        model_id: None,
                        model_name: None,
                        error: Some(format!("Engine panicked: {}", panic_msg)),
                    },
                );

                return Err(anyhow::anyhow!(
                    "Transcription engine panicked: {}. The model has been unloaded and will reload on next attempt.",
                    panic_msg
                ));
            }
        };
        Ok(RunOutcome {
            text,
            languages,
            applied_language_hint,
            output_was_translated,
            model_detected_language: None,
            model_is_whisper: false,
        })
    }
}

/// What a batch transcription produced, plus what post-processing needs to
/// know about how it was produced.
struct RunOutcome {
    text: String,
    /// The model's supported languages.
    languages: Vec<String>,
    applied_language_hint: Option<String>,
    output_was_translated: bool,
    /// The language the model itself detected, if it reported one.
    model_detected_language: Option<String>,
    /// Whether the loaded model is actually whisper-family (arch string).
    /// Non-whisper archs (e.g. Voxtral Small) can advertise
    /// Feature::InitialPrompt yet reject the whisper-kind run extension with
    /// INVALID_ARG, so the whisper extension must be gated on the arch, not
    /// on the feature (see #1601).
    model_is_whisper: bool,
}

fn emit_stream_text(app_handle: &AppHandle, committed: &str, tentative: &str) {
    let _ = StreamTextEvent {
        committed: committed.to_string(),
        tentative: tentative.to_string(),
    }
    .emit(app_handle);
}

fn lock_perf(perf: &Mutex<StreamPerf>) -> MutexGuard<'_, StreamPerf> {
    perf.lock().unwrap_or_else(|e| e.into_inner())
}

struct StreamPerf {
    feed_count: u64,
    emit_count: u64,
    streamed_samples: u64,
    stream_compute_elapsed: Duration,
    last_log: Instant,
    latest_revision: i32,
    latest_input_received_ms: i64,
    latest_audio_committed_ms: i64,
    latest_buffered_ms: i64,
}

impl StreamPerf {
    fn new() -> Self {
        Self {
            feed_count: 0,
            emit_count: 0,
            streamed_samples: 0,
            stream_compute_elapsed: Duration::ZERO,
            last_log: Instant::now(),
            latest_revision: 0,
            latest_input_received_ms: 0,
            latest_audio_committed_ms: 0,
            latest_buffered_ms: 0,
        }
    }

    fn record_feed(&mut self, samples: usize) {
        self.feed_count += 1;
        self.streamed_samples += samples as u64;
    }

    fn record_compute(&mut self, elapsed: Duration) {
        self.stream_compute_elapsed += elapsed;
    }

    fn record_update(&mut self, update: &transcribe_cpp::StreamUpdate) {
        self.latest_revision = update.revision;
        self.latest_input_received_ms = update.input_received_ms;
        self.latest_audio_committed_ms = update.audio_committed_ms;
        self.latest_buffered_ms = update.buffered_ms;
    }

    fn record_emit(&mut self) {
        self.emit_count += 1;
    }

    fn maybe_log(&mut self) {
        if self.last_log.elapsed() < STREAM_PERF_LOG_INTERVAL {
            return;
        }

        let audio_secs = self.audio_secs();
        let compute_secs = self.compute_secs();
        debug!(
            "Live preview perf: {:.2}s streamed audio, {:.2}s model compute ({:.2}x real-time), \
             input_received={:.2}s, committed_audio={:.2}s, buffered={}ms, revision={}, \
             {} frames fed, {} updates emitted",
            audio_secs,
            compute_secs,
            real_time_factor(audio_secs, compute_secs),
            self.latest_input_received_ms as f64 / 1000.0,
            self.latest_audio_committed_ms as f64 / 1000.0,
            self.latest_buffered_ms,
            self.latest_revision,
            self.feed_count,
            self.emit_count,
        );
        self.last_log = Instant::now();
    }

    fn log_finalized(&self, chars: usize) {
        let audio_secs = self.audio_secs();
        let compute_secs = self.compute_secs();
        info!(
            "Live preview finalized in {:.2}s model compute for {:.2}s streamed audio ({:.2}x real-time): \
             input_received={:.2}s, committed_audio={:.2}s, buffered={}ms, revision={}, \
             {} frames fed, {} updates emitted, {} chars",
            compute_secs,
            audio_secs,
            real_time_factor(audio_secs, compute_secs),
            self.latest_input_received_ms as f64 / 1000.0,
            self.latest_audio_committed_ms as f64 / 1000.0,
            self.latest_buffered_ms,
            self.latest_revision,
            self.feed_count,
            self.emit_count,
            chars
        );
    }

    fn audio_secs(&self) -> f64 {
        self.streamed_samples as f64 / 16_000.0
    }

    fn compute_secs(&self) -> f64 {
        self.stream_compute_elapsed.as_secs_f64()
    }
}

fn real_time_factor(audio_secs: f64, compute_secs: f64) -> f64 {
    if compute_secs > 0.0 {
        audio_secs / compute_secs
    } else {
        0.0
    }
}

/// Resolve the persisted language intent into the language a specific model can
/// use without writing the coerced value back to settings.
fn effective_language_for_model(
    settings: &AppSettings,
    model_manager: &ModelManager,
    model_id: &str,
) -> String {
    match model_manager.get_model_info(model_id) {
        Some(info) => crate::managers::model::effective_language(
            &settings.selected_language,
            &info.supported_languages,
            info.supports_language_detection,
        ),
        None => settings.selected_language.clone(),
    }
}

/// Resolve how confidently Handy knows the language of the text produced by a
/// transcription run. The UI language is deliberately not part of this
/// decision.
fn resolve_output_language_evidence(
    settings: &AppSettings,
    applied_language_hint: Option<&str>,
    supported_languages: &[String],
    translated_to_english: bool,
) -> OutputLanguageEvidence {
    if translated_to_english {
        return OutputLanguageEvidence::TranslatedToEnglish;
    }

    // Stored language intent is only evidence when this specific engine run
    // actually received the hint. Some multilingual engines (notably Parakeet
    // V3) always auto-detect and ignore Handy's selection; transcribe-cpp also
    // drops a requested hint when the loaded model does not advertise it.
    if let Some(language) = applied_language_hint.filter(|lang| !lang.is_empty() && *lang != "auto")
    {
        if settings.selected_language != "auto"
            && crate::managers::model::canonical_language_code(&settings.selected_language)
                == crate::managers::model::canonical_language_code(language)
        {
            return OutputLanguageEvidence::UserSelected(language.to_string());
        }

        // The engine may have required a concrete fallback even though the
        // user's persisted language was auto or unsupported.
        return OutputLanguageEvidence::ModelConstrained(language.to_string());
    }

    // A single-language model has a known output language without needing a
    // selectable language hint.
    if let [language] = supported_languages {
        return OutputLanguageEvidence::ModelConstrained(language.clone());
    }

    OutputLanguageEvidence::Unknown
}

/// Upgrade [`OutputLanguageEvidence::Unknown`] with the language the model
/// itself detected during the run (audio-based LID, e.g. Whisper in auto
/// mode). Stronger evidence resolved before the run is never overridden.
fn with_model_detected_language(
    evidence: OutputLanguageEvidence,
    detected: Option<String>,
) -> OutputLanguageEvidence {
    match (evidence, detected) {
        (OutputLanguageEvidence::Unknown, Some(language))
            if !language.is_empty() && language != "auto" =>
        {
            OutputLanguageEvidence::ModelDetected(language)
        }
        (evidence, _) => evidence,
    }
}

struct TranscribeCppRunPlan {
    task: Task,
    language: Option<String>,
    target_language: Option<String>,
}

/// Build the transcribe-cpp language/task options shared by batch and live
/// streaming paths.
fn transcribe_cpp_run_plan(
    translate_to_english: bool,
    effective_language: &str,
    model_languages: &[String],
    model_supports_translate: bool,
) -> TranscribeCppRunPlan {
    let requested_language = match effective_language {
        "auto" => None,
        other => Some(other.to_string()),
    };
    // Only pass a language the loaded model actually advertises (per
    // capabilities().languages); otherwise auto-detect rather than failing with
    // UNSUPPORTED_LANGUAGE. Language-agnostic models report an empty list, so
    // they always stay on auto.
    let language = requested_language.filter(|lang| model_languages.iter().any(|l| l == lang));
    let (task, target_language) = cpp_translation_task(
        translate_to_english,
        model_supports_translate,
        language.as_deref(),
    );

    TranscribeCppRunPlan {
        task,
        language,
        target_language,
    }
}

fn post_process_transcription_text(
    raw: String,
    settings: &AppSettings,
    custom_words_already_prompted: bool,
    output_language: &OutputLanguageEvidence,
    supported_languages: &[String],
) -> String {
    let converts_script = settings.chinese_script != ChineseScript::AsTranscribed;
    fail_open_text_transform(raw, |raw| {
        // Last-resort language evidence: confidence-gated detection from the
        // transcribed text itself, constrained to the model's languages. Only
        // consulted when it can change the outcome (built-in gated fillers or
        // Chinese script conversion).
        let output_language = match output_language {
            OutputLanguageEvidence::Unknown
                if converts_script
                    || (settings.filler_word_removal_enabled
                        && settings.custom_filler_words.is_none()) =>
            {
                match detect_output_language(&raw, supported_languages) {
                    Some(language) => {
                        debug!("Text-based language detection resolved '{}'", language);
                        OutputLanguageEvidence::TextDetected(language)
                    }
                    None => OutputLanguageEvidence::Unknown,
                }
            }
            other => other.clone(),
        };

        // Convert the script before custom words so they match in the script
        // the user writes in. Only output known to be Chinese is touched, so
        // e.g. Japanese kanji are never rewritten.
        let variety = output_language
            .language()
            .and_then(ChineseVariety::from_language);
        let raw = match variety {
            Some(variety) if converts_script => {
                convert_chinese_script(&raw, variety, settings.chinese_script)
            }
            _ => raw,
        };

        // Spoken punctuation first, then voice deletion, then the terminal
        // fallback, so the custom-word pass and every later stage see final
        // wording and punctuation. Each pass is independently toggleable;
        // off reproduces today's behavior.
        let punctuated = if settings.spoken_punctuation {
            normalize_spoken_punctuation(&raw)
        } else {
            raw
        };

        // Voice deletion runs after punctuation insertion (so "hello comma
        // scratch that" deletes the punctuated word "hello,") and before the
        // terminal fallback. A "delete everything" command short-circuits
        // every later pass: the dictation pastes nothing (the paste site
        // already skips empty text).
        let deleted = if settings.voice_deletion_commands {
            apply_voice_deletion(&punctuated)
        } else {
            VoiceDeletionOutcome {
                text: punctuated,
                cleared: false,
            }
        };
        if deleted.cleared {
            info!("Voice deletion command cleared the transcription; skipping the remaining text passes");
            return String::new();
        }

        let punctuated = if settings.terminal_punctuation {
            apply_terminal_punctuation(&deleted.text)
        } else {
            deleted.text
        };

        let corrected = if !settings.custom_words.is_empty() && !custom_words_already_prompted {
            apply_custom_words(
                &punctuated,
                &settings.custom_words,
                settings.word_correction_threshold,
            )
        } else {
            punctuated
        };

        let without_fillers = remove_filler_words(
            &corrected,
            &output_language,
            &settings.custom_filler_words,
            settings.filler_word_removal_enabled,
        );

        normalize_transcription_output(&without_fillers)
    })
}

/// Characters to wait for before detecting the preview's language. A lone
/// Chinese character reads as Mandarin, but Japanese often opens with kanji
/// before any kana; waiting a few characters keeps those from locking in.
const PREVIEW_DETECTION_MIN_CHARS: usize = 6;

/// Converts live-preview text into the configured Chinese script as it streams.
///
/// The preview is cosmetic: the final paste re-resolves the language and
/// converts on its own, so this only has to look right while speaking.
struct PreviewScript {
    script: ChineseScript,
    variety: Option<ChineseVariety>,
    /// The language wasn't known when the stream started (auto-detect), so it
    /// is detected from the streamed text until it turns out to be Chinese.
    detect: bool,
}

impl PreviewScript {
    fn new(script: ChineseScript, output_language: &OutputLanguageEvidence) -> Self {
        let enabled = script != ChineseScript::AsTranscribed;
        Self {
            script,
            variety: output_language
                .language()
                .and_then(ChineseVariety::from_language)
                .filter(|_| enabled),
            detect: enabled && *output_language == OutputLanguageEvidence::Unknown,
        }
    }

    /// Converts the model's raw committed/tentative text. Always fed the raw
    /// text, never a previous conversion, so it stays consistent with the
    /// final conversion. Once Chinese is detected it sticks for the rest of
    /// the stream so the preview doesn't flip back and forth.
    fn convert(
        &mut self,
        committed: &str,
        tentative: &str,
        supported_languages: &[String],
    ) -> (String, String) {
        if self.variety.is_none() && self.detect {
            let text = format!("{committed}{tentative}");
            if text.chars().count() >= PREVIEW_DETECTION_MIN_CHARS {
                self.variety = detect_output_language(&text, supported_languages)
                    .as_deref()
                    .and_then(ChineseVariety::from_language);
            }
        }

        match self.variety {
            Some(variety) => (
                convert_chinese_script(committed, variety, self.script),
                convert_chinese_script(tentative, variety, self.script),
            ),
            None => (committed.to_string(), tentative.to_string()),
        }
    }
}

/// Optional text cleanup must never discard a successful model result. The
/// transform is pure and owns its input, so recovering the untouched text is
/// safe even if a bug in custom-word or filler filtering unwinds.
fn fail_open_text_transform<F>(raw: String, transform: F) -> String
where
    F: FnOnce(String) -> String,
{
    let fallback = raw.clone();
    match catch_unwind(AssertUnwindSafe(|| transform(raw))) {
        Ok(processed) => processed,
        Err(payload) => {
            error!(
                "Optional transcription text post-processing panicked: {}; using the raw transcription",
                panic_payload_message(payload.as_ref())
            );
            fallback
        }
    }
}

/// Decide a transcribe-cpp run's task + translation target from settings.
///
/// "Translate to English" only fires where the model advertises translation.
/// Unlike transcribe-rs (which forces the target to English itself when its
/// `translate` flag is set), transcribe-cpp requires an explicit
/// `target_language`: a null target defaults to the *source*, so a non-English
/// source silently becomes e.g. es→es and Canary rejects the unadvertised pair.
/// An English source is skipped entirely - en→en is not a real translation, and
/// it's reachable by default since auto-detect-less models coerce intent to "en".
///
/// Returns `(task, target_language)` ready to drop into `RunOptions`.
fn cpp_translation_task(
    translate_to_english: bool,
    model_supports_translate: bool,
    source_language: Option<&str>,
) -> (Task, Option<String>) {
    let translate_to_en =
        translate_to_english && model_supports_translate && source_language != Some("en");
    if translate_to_en {
        (Task::Translate, Some("en".to_string()))
    } else {
        (Task::Transcribe, None)
    }
}

/// Drain a stream command channel, ignoring fed audio, until the caller
/// finalizes or cancels. Used when streaming can't actually run (model not
/// loaded / not streaming-capable) so the finalize handshake still completes
/// and the caller falls back to batch transcription.
fn drain_until_finalize(rx: mpsc::Receiver<StreamCmd>) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            StreamCmd::Feed(_) => {}
            StreamCmd::Finalize(reply) => {
                let _ = reply.send(Ok(None));
                break;
            }
            StreamCmd::Cancel => break,
        }
    }
}

/// Log the compute devices transcribe-cpp registers.
///
/// Devices are listed by a transcription worker process (see
/// [`crate::engine_supervisor`]), so GPU driver initialization never runs in
/// the app process. Listing opens the GPU, which on macOS loads ggml's Metal
/// library and compiles it when the system shader cache does not hold it yet
/// (the first launch after an install or update), so the app calls this from
/// a background thread instead of its startup path.
pub fn report_compute_devices(tm: &TranscriptionManager) {
    if transcribe_gpu_disabled_for_host() {
        warn!(
            "Windows x64 build is running under emulation on an ARM64 host; \
             disabling transcribe.cpp GPU acceleration and using CPU"
        );
    }
    match transcribe_compute_devices(&tm.engine) {
        Ok(devices) => info!(
            "transcribe-cpp initialized with {} compute device(s): [{}]",
            devices.len(),
            devices
                .iter()
                .map(|d| format!("{} ({})", d.name, d.kind))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Err(e) => warn!("transcribe-cpp compute devices are unavailable: {}", e),
    }
}

/// Human-readable list of the transcribe-cpp compute devices, for the
/// `--list-devices` flag. The reported `index` is the value to pass to
/// `--device-index`. Errors if the devices could not be listed.
pub fn describe_compute_devices(tm: &TranscriptionManager) -> Result<Vec<String>> {
    Ok(transcribe_compute_devices(&tm.engine)?
        .into_iter()
        .map(|d| {
            let idx = d
                .index
                .map(|i| i.to_string())
                .unwrap_or_else(|| "-".to_string());
            let vram_mb = d.memory_total / (1024 * 1024);
            format!(
                "index={} kind={} name={} vram={}MB",
                idx,
                d.kind,
                d.label(),
                vram_mb
            )
        })
        .collect())
}

/// Map Handy's whisper accelerator setting to a transcribe-cpp [`Backend`].
///
/// `Auto` lets the library pick the best device (with CPU fallback), while
/// `Cpu` forces strict CPU. `Gpu` only remains as the companion setting for an
/// exact device; without a valid exact device it has the retired generic GPU
/// state's new Auto semantics. An emulated x64 process on Windows ARM64 forces
/// strict CPU for every setting.
fn select_transcribe_backend(setting: TranscribeAcceleratorSetting) -> Backend {
    select_transcribe_backend_for_host(setting, transcribe_gpu_disabled_for_host())
}

fn select_transcribe_backend_for_host(
    setting: TranscribeAcceleratorSetting,
    gpu_disabled: bool,
) -> Backend {
    match effective_transcribe_accelerator(setting, gpu_disabled) {
        TranscribeAcceleratorSetting::Cpu => Backend::Cpu,
        TranscribeAcceleratorSetting::Auto | TranscribeAcceleratorSetting::Gpu => Backend::Auto,
    }
}

/// Resolve the user's persisted GPU choice to the device key the worker
/// should load on. Registry indices and handles are process-local, so
/// settings store a key based on the backend's stable `device_id` (falling
/// back to name for backends such as Metal that do not report one). The
/// worker falls back to automatic selection if that device is gone.
fn resolve_gpu_device(
    setting: TranscribeAcceleratorSetting,
    gpu_device: Option<&str>,
) -> Option<String> {
    if transcribe_gpu_disabled_for_host() || setting != TranscribeAcceleratorSetting::Gpu {
        return None;
    }
    gpu_device.map(str::to_string)
}

/// Apply the user's ORT accelerator preference to the transcribe-rs global.
/// Called on startup and before loading a model.
///
/// The transcribe.cpp (whisper-family) backend is no longer set here: it is
/// chosen at model-load time from [`select_transcribe_backend`], so changing the
/// accelerator only needs a model reload (see `reload_model_on_next_use`).
pub fn apply_accelerator_settings(app: &tauri::AppHandle) {
    use transcribe_rs::accel;

    let settings = get_settings(app);

    info!(
        "transcribe.cpp accelerator preference: {:?} (applied on next model load)",
        settings.transcribe_accelerator
    );

    let ort_pref = match settings.ort_accelerator {
        OrtAcceleratorSetting::Auto => accel::OrtAccelerator::Auto,
        OrtAcceleratorSetting::Cpu => accel::OrtAccelerator::CpuOnly,
        OrtAcceleratorSetting::Cuda => accel::OrtAccelerator::Cuda,
        OrtAcceleratorSetting::DirectMl => accel::OrtAccelerator::DirectMl,
        OrtAcceleratorSetting::Rocm => accel::OrtAccelerator::Rocm,
    };
    accel::set_ort_accelerator(ort_pref);
    info!("ORT accelerator set to: {}", ort_pref);
}

#[derive(Serialize, Clone, Debug, Type)]
pub struct GpuDeviceOption {
    pub id: String,
    pub name: String,
    pub total_vram_mb: usize,
}

fn transcribe_gpu_disabled_for_host() -> bool {
    crate::utils::is_windows_x64_emulated_on_arm64()
}

fn effective_transcribe_accelerator(
    setting: TranscribeAcceleratorSetting,
    gpu_disabled: bool,
) -> TranscribeAcceleratorSetting {
    if gpu_disabled {
        TranscribeAcceleratorSetting::Cpu
    } else {
        setting
    }
}

/// The compute devices the latest worker listed. On a host without GPU
/// access every worker is CPU-only, so it lists no GPU.
fn transcribe_compute_devices(engine: &EngineSupervisor) -> Result<Vec<DeviceInfo>> {
    engine
        .devices()
        .ok_or_else(|| anyhow::anyhow!("compute devices could not be listed (see the log for why)"))
}

fn available_transcribe_accelerators(gpu_disabled: bool) -> Vec<String> {
    if gpu_disabled {
        vec!["cpu".to_string()]
    } else {
        vec!["auto".to_string(), "cpu".to_string(), "gpu".to_string()]
    }
}

fn gpu_device_options(engine: &EngineSupervisor) -> Vec<GpuDeviceOption> {
    // GPU compute devices as the latest worker listed them, so a GPU that
    // comes back is picked up. `id` is a persistent identity key, never the
    // process-local registry index. It uses the backend's device_id where
    // available and its name otherwise (Metal). `total_vram_mb` is 0 when the
    // backend does not report capacity. Empty if listing failed.
    transcribe_compute_devices(engine)
        .unwrap_or_default()
        .into_iter()
        .filter(DeviceInfo::is_gpu)
        .map(|d| GpuDeviceOption {
            id: d.key.clone(),
            name: d.label().to_string(),
            total_vram_mb: (d.memory_total / (1024 * 1024)) as usize,
        })
        .collect()
}

#[derive(Serialize, Clone, Debug, Type)]
pub struct AvailableAccelerators {
    pub transcribe: Vec<String>,
    pub ort: Vec<String>,
    pub gpu_devices: Vec<GpuDeviceOption>,
}

/// Return the accelerators available to this process on its current host.
pub fn get_available_accelerators(tm: &TranscriptionManager) -> AvailableAccelerators {
    use transcribe_rs::accel::OrtAccelerator;

    let ort_options: Vec<String> = OrtAccelerator::available()
        .into_iter()
        .map(|a| a.to_string())
        .collect();

    let transcribe_options = available_transcribe_accelerators(transcribe_gpu_disabled_for_host());

    AvailableAccelerators {
        transcribe: transcribe_options,
        ort: ort_options,
        gpu_devices: gpu_device_options(&tm.engine),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn languages(codes: &[&str]) -> Vec<String> {
        codes.iter().map(|code| (*code).to_string()).collect()
    }

    #[test]
    fn normal_hosts_preserve_every_transcribe_accelerator_setting() {
        for setting in [
            TranscribeAcceleratorSetting::Auto,
            TranscribeAcceleratorSetting::Cpu,
            TranscribeAcceleratorSetting::Gpu,
        ] {
            assert_eq!(effective_transcribe_accelerator(setting, false), setting);
        }
        assert_eq!(
            available_transcribe_accelerators(false),
            ["auto", "cpu", "gpu"]
        );
        assert_eq!(
            select_transcribe_backend_for_host(TranscribeAcceleratorSetting::Auto, false),
            Backend::Auto
        );
        assert_eq!(
            select_transcribe_backend_for_host(TranscribeAcceleratorSetting::Cpu, false),
            Backend::Cpu
        );
        assert_eq!(
            select_transcribe_backend_for_host(TranscribeAcceleratorSetting::Gpu, false),
            Backend::Auto
        );
    }

    #[test]
    fn emulated_x64_on_arm64_forces_every_transcribe_setting_to_cpu() {
        for setting in [
            TranscribeAcceleratorSetting::Auto,
            TranscribeAcceleratorSetting::Cpu,
            TranscribeAcceleratorSetting::Gpu,
        ] {
            assert_eq!(
                effective_transcribe_accelerator(setting, true),
                TranscribeAcceleratorSetting::Cpu
            );
            assert_eq!(
                select_transcribe_backend_for_host(setting, true),
                Backend::Cpu
            );
        }
        assert_eq!(available_transcribe_accelerators(true), ["cpu"]);
    }

    #[test]
    fn optional_text_transform_falls_back_to_raw_text_after_panic() {
        let raw = "原始轉錄。".to_string();
        let result = fail_open_text_transform(raw.clone(), |_| {
            panic!("simulated optional cleanup failure")
        });

        assert_eq!(result, raw);
    }

    fn session_buffer() -> StreamSessionBuffer {
        let mut buffer = StreamSessionBuffer::default();
        buffer.begin(
            PreviewScript::new(
                ChineseScript::AsTranscribed,
                &OutputLanguageEvidence::Unknown,
            ),
            true,
            true,
            &languages(&["en"]),
        );
        buffer
    }

    #[test]
    fn session_buffer_without_edits_passes_snapshots_through() {
        let mut session = session_buffer();
        assert_eq!(session.render("Hello ", "wor"), "Hello wor");
        assert_eq!(session.render("Hello world ", ""), "Hello world ");

        // No manual edits: finalize gets the engine text byte-for-byte.
        assert_eq!(
            session.combine_final("Hello world".to_string()),
            "Hello world"
        );
        assert!(!session.live);
    }

    #[test]
    fn session_buffer_delete_word_while_tentative_stays_deleted() {
        let mut session = session_buffer();
        session.render("hello ", "world");
        assert_eq!(session.delete_last_word(), Some("hello ".to_string()));

        // The engine commits the (already deleted) tentative word verbatim
        // and the user keeps talking: the word must not resurrect.
        assert_eq!(session.render("hello world and more", ""), "hello and more");
        assert_eq!(
            session.combine_final("hello world and more".to_string()),
            "hello and more"
        );
    }

    #[test]
    fn session_buffer_delete_committed_word_joins_cleanly() {
        let mut session = session_buffer();
        session.render("hello world", "");
        assert_eq!(session.delete_last_word(), Some("hello ".to_string()));

        // New speech arrives after the committed prefix that was already
        // consumed: exactly one separating space survives the join.
        assert_eq!(session.render("hello world next", ""), "hello next");
    }

    #[test]
    fn session_buffer_delete_word_repeated_and_on_empty() {
        let mut session = session_buffer();
        session.render("one two three", "");
        assert_eq!(session.delete_last_word(), Some("one two ".to_string()));
        assert_eq!(session.delete_last_word(), Some("one ".to_string()));
        assert_eq!(session.delete_last_word(), Some("".to_string()));
        // Deleting from an empty buffer is a no-op that still refreshes.
        assert_eq!(session.delete_last_word(), Some("".to_string()));

        // The interim display transform trims a leading space; the raw
        // buffer keeps it, so speech after a full deletion joins normally.
        assert_eq!(session.render(" fresh", ""), "fresh");
        assert_eq!(session.render(" fresh words", ""), "fresh words");
    }

    #[test]
    fn session_buffer_delete_word_requires_live_session() {
        let mut session = StreamSessionBuffer::default();
        assert_eq!(session.delete_last_word(), None);

        // After finalize the buffer is no longer live.
        let mut live = session_buffer();
        live.render("hello", "");
        live.combine_final("hello".to_string());
        assert_eq!(live.delete_last_word(), None);
    }

    #[test]
    fn session_buffer_phrase_split_across_snapshot_regions_converts() {
        // The interim transform runs over the FULL combined raw buffer, so a
        // spoken phrase split across the committed/tentative boundary (or
        // across a manual-edit seam) still converts; converting the parts
        // separately would miss it.
        let mut session = session_buffer();
        assert_eq!(session.render("say full", " stop now"), "say. Now");

        // After a manual edit the same holds across the base/new-material
        // seam: "full" lands in base, "stop" in the new material.
        let mut edited = session_buffer();
        edited.render("alpha ", "beta");
        edited.delete_last_word(); // removes "beta", freezes "alpha "
        assert_eq!(edited.render("alpha beta full", " stop now"), "alpha. Now");
    }

    #[test]
    fn session_buffer_display_grows_no_terminal_punctuation() {
        let mut session = session_buffer();
        // Mid-sentence, mid-question: no period or question mark may appear
        // on any tick (the terminal fallback runs only at finalize).
        assert_eq!(session.render("hello world", ""), "hello world");
        assert_eq!(session.render("what is this", ""), "what is this");

        // The finalize pipeline is where terminal punctuation may land.
        let settings = AppSettings {
            spoken_punctuation: true,
            voice_deletion_commands: true,
            terminal_punctuation: true,
            ..Default::default()
        };
        let final_text = post_process_transcription_text(
            session.combine_final("what is this".to_string()),
            &settings,
            false,
            &OutputLanguageEvidence::UserSelected("en".to_string()),
            &languages(&["en"]),
        );
        assert_eq!(final_text, "what is this?");
    }

    #[test]
    fn session_buffer_voice_deletion_command_eats_into_edited_base() {
        // A spoken "scratch that" after a manual edit still deletes the word
        // before it, even when that word was streamed after the edit: the
        // transform recomputes over the whole combined buffer every tick.
        // Here "two" was removed by the hotkey, "three" by the voice command.
        let mut session = session_buffer();
        session.render("one two", "");
        session.delete_last_word(); // buffer is now "one "
        session.render("one two three", "");
        assert_eq!(
            session.render("one two three scratch that", ""),
            "one".to_string()
        );
        // And the finalize fold agrees: raw buffer passed onward is
        // "one three scratch that more" (the voice command is consumed by
        // the finalize pipeline's own deletion pass, not by the buffer).
        assert_eq!(
            session.combine_final("one two three scratch that more".to_string()),
            "one three scratch that more"
        );
    }

    #[test]
    fn join_raw_collapses_a_single_duplicated_separator() {
        assert_eq!(join_raw("hello ", " world"), "hello world");
        assert_eq!(join_raw("hello", " world"), "hello world");
        assert_eq!(join_raw("hello ", "world"), "hello world");
        // Newlines are never collapsed away by the space rule.
        assert_eq!(join_raw("hello ", "\nworld"), "hello \nworld");
        assert_eq!(join_raw("line\n", "next"), "line\nnext");
    }

    #[test]
    fn common_prefix_stops_on_char_boundaries() {
        assert_eq!(common_prefix_len("你好世界", "你好啊"), "你好".len());
        assert_eq!(common_prefix_len("abc", "abc"), 3);
        assert_eq!(common_prefix_len("", "abc"), 0);
    }

    #[test]
    fn non_chinese_output_is_never_converted() {
        let settings = AppSettings {
            chinese_script: ChineseScript::Traditional,
            terminal_punctuation: false,
            ..Default::default()
        };
        for evidence in [
            OutputLanguageEvidence::ModelDetected("ja".to_string()),
            OutputLanguageEvidence::UserSelected("en".to_string()),
            OutputLanguageEvidence::TranslatedToEnglish,
        ] {
            let result = post_process_transcription_text(
                "学校に行きます".to_string(),
                &settings,
                false,
                &evidence,
                &languages(&["zh", "ja", "en"]),
            );

            assert_eq!(result, "学校に行きます", "{evidence:?}");
        }
    }

    #[test]
    fn auto_detected_chinese_text_is_converted() {
        let settings = AppSettings {
            chinese_script: ChineseScript::Simplified,
            filler_word_removal_enabled: false,
            ..Default::default()
        };
        let result = post_process_transcription_text(
            "我們今天下午一起去學校圖書館看書，然後再去吃晚飯。".to_string(),
            &settings,
            false,
            &OutputLanguageEvidence::Unknown,
            &languages(&["zh", "en", "ja"]),
        );

        assert_eq!(result, "我们今天下午一起去学校图书馆看书，然后再去吃晚饭。");
    }

    #[test]
    fn portuguese_transcription_does_not_use_english_ui_filler_words() {
        let settings = AppSettings {
            app_language: "en".to_string(),
            selected_language: "pt-BR".to_string(),
            terminal_punctuation: false,
            ..Default::default()
        };
        let supported = languages(&["en", "pt"]);
        let evidence = resolve_output_language_evidence(&settings, Some("pt"), &supported, false);

        let result = post_process_transcription_text(
            "eu vi um carro".to_string(),
            &settings,
            false,
            &evidence,
            &supported,
        );

        assert_eq!(
            evidence,
            OutputLanguageEvidence::UserSelected("pt".to_string())
        );
        assert_eq!(result, "eu vi um carro");
    }

    #[test]
    fn norwegian_alias_is_recorded_as_user_selected_evidence() {
        let settings = AppSettings {
            selected_language: "no".to_string(),
            ..Default::default()
        };

        let evidence =
            resolve_output_language_evidence(&settings, Some("nb"), &languages(&["nb"]), false);

        assert_eq!(
            evidence,
            OutputLanguageEvidence::UserSelected("nb".to_string())
        );
    }

    #[test]
    fn auto_language_without_detection_skips_gated_filler_removal() {
        let settings = AppSettings {
            selected_language: "auto".to_string(),
            terminal_punctuation: false,
            ..Default::default()
        };
        let evidence =
            resolve_output_language_evidence(&settings, None, &languages(&["en", "pt"]), false);

        // Too short for a reliable text detection, so the gated "um" must
        // survive; the universal "uhm" is removed regardless.
        let result = post_process_transcription_text(
            "um uhm ok".to_string(),
            &settings,
            false,
            &evidence,
            &languages(&["en", "pt"]),
        );

        assert_eq!(evidence, OutputLanguageEvidence::Unknown);
        assert_eq!(result, "um ok");
    }

    #[test]
    fn unknown_evidence_with_confident_text_detection_removes_gated_fillers() {
        let settings = AppSettings {
            selected_language: "auto".to_string(),
            terminal_punctuation: false,
            ..Default::default()
        };

        let result = post_process_transcription_text(
            "um so the weather forecast said it would probably rain throughout the whole weekend"
                .to_string(),
            &settings,
            false,
            &OutputLanguageEvidence::Unknown,
            &languages(&["en", "pt", "es", "de"]),
        );

        assert_eq!(
            result,
            "so the weather forecast said it would probably rain throughout the whole weekend"
        );
    }

    #[test]
    fn unknown_evidence_with_portuguese_text_preserves_um() {
        let settings = AppSettings {
            selected_language: "auto".to_string(),
            terminal_punctuation: false,
            ..Default::default()
        };

        let result = post_process_transcription_text(
            "eu vi um carro na rua ontem de manhã quando fui ao mercado".to_string(),
            &settings,
            false,
            &OutputLanguageEvidence::Unknown,
            &languages(&["en", "pt", "es", "de"]),
        );

        assert_eq!(
            result,
            "eu vi um carro na rua ontem de manhã quando fui ao mercado"
        );
    }

    #[test]
    fn model_detected_language_upgrades_unknown_evidence_only() {
        assert_eq!(
            with_model_detected_language(OutputLanguageEvidence::Unknown, Some("en".to_string())),
            OutputLanguageEvidence::ModelDetected("en".to_string())
        );
        assert_eq!(
            with_model_detected_language(OutputLanguageEvidence::Unknown, Some("auto".to_string())),
            OutputLanguageEvidence::Unknown
        );
        assert_eq!(
            with_model_detected_language(OutputLanguageEvidence::Unknown, None),
            OutputLanguageEvidence::Unknown
        );
        assert_eq!(
            with_model_detected_language(
                OutputLanguageEvidence::UserSelected("pt".to_string()),
                Some("en".to_string())
            ),
            OutputLanguageEvidence::UserSelected("pt".to_string())
        );
    }

    #[test]
    fn auto_language_uses_single_language_model_as_evidence() {
        let settings = AppSettings {
            selected_language: "auto".to_string(),
            ..Default::default()
        };

        let evidence =
            resolve_output_language_evidence(&settings, None, &languages(&["en"]), false);

        assert_eq!(
            evidence,
            OutputLanguageEvidence::ModelConstrained("en".to_string())
        );
    }

    #[test]
    fn unsupported_explicit_language_uses_model_fallback_as_evidence() {
        let settings = AppSettings {
            selected_language: "pt".to_string(),
            ..Default::default()
        };

        let evidence = resolve_output_language_evidence(
            &settings,
            Some("en"),
            &languages(&["en", "de"]),
            false,
        );

        assert_eq!(
            evidence,
            OutputLanguageEvidence::ModelConstrained("en".to_string())
        );
    }

    #[test]
    fn ignored_user_language_is_not_output_evidence() {
        let settings = AppSettings {
            // Parakeet V3 ignores language hints and auto-detects even when a
            // selection from the previously active model remains persisted.
            selected_language: "en".to_string(),
            terminal_punctuation: false,
            ..Default::default()
        };
        let supported = languages(&["en", "de", "pt"]);

        let evidence = resolve_output_language_evidence(&settings, None, &supported, false);
        assert_eq!(evidence, OutputLanguageEvidence::Unknown);

        let result = post_process_transcription_text(
            "eu vi um carro".to_string(),
            &settings,
            false,
            &evidence,
            &supported,
        );
        assert_eq!(result, "eu vi um carro");
    }

    fn text_pipeline_settings(spoken: bool, terminal: bool, deletion: bool) -> AppSettings {
        AppSettings {
            chinese_script: ChineseScript::AsTranscribed,
            custom_words: vec!["ChargeBee".to_string()],
            word_correction_threshold: 0.5,
            spoken_punctuation: spoken,
            terminal_punctuation: terminal,
            voice_deletion_commands: deletion,
            ..Default::default()
        }
    }

    /// The dictionary can only match "charge b" as clean n-gram words after
    /// the spoken-punctuation pass inserted the period and handed the capital
    /// over, and the trailing period proves the terminal fallback ran ahead
    /// of the custom-word correction: normalizer -> terminal fallback ->
    /// custom words -> filler/normalize cleanup.
    #[test]
    fn punctuation_passes_compose_in_the_specified_order() {
        let settings = text_pipeline_settings(true, true, true);

        let result = post_process_transcription_text(
            "hello full stop charge b".to_string(),
            &settings,
            false,
            &OutputLanguageEvidence::UserSelected("en".to_string()),
            &languages(&["en"]),
        );

        assert_eq!(result, "hello. ChargeBee.");

        let question = post_process_transcription_text(
            "what time is it".to_string(),
            &settings,
            false,
            &OutputLanguageEvidence::UserSelected("en".to_string()),
            &languages(&["en"]),
        );
        assert_eq!(question, "what time is it?");
    }

    #[test]
    fn punctuation_passes_respect_their_toggles_independently() {
        let en = OutputLanguageEvidence::UserSelected("en".to_string());
        let supported = languages(&["en"]);
        let raw = "hello comma charge b".to_string();

        // Spoken punctuation only: comma inserted, no terminal period.
        let spoken_only = text_pipeline_settings(true, false, true);
        assert_eq!(
            post_process_transcription_text(raw.clone(), &spoken_only, false, &en, &supported),
            "hello, ChargeBee"
        );

        // Terminal fallback only: period appended, spoken token untouched.
        let terminal_only = text_pipeline_settings(false, true, true);
        assert_eq!(
            post_process_transcription_text(raw.clone(), &terminal_only, false, &en, &supported),
            "hello comma ChargeBee."
        );

        // Both on.
        let both = text_pipeline_settings(true, true, true);
        assert_eq!(
            post_process_transcription_text(raw.clone(), &both, false, &en, &supported),
            "hello, ChargeBee."
        );

        // Both off: byte-for-byte the pre-punctuation pipeline (dictionary,
        // filler removal, normalization, and nothing else).
        let neither = text_pipeline_settings(false, false, false);
        assert_eq!(
            post_process_transcription_text(raw, &neither, false, &en, &supported),
            "hello comma ChargeBee"
        );
    }

    /// Voice deletion sits between the punctuation passes and the dictionary:
    /// it sees punctuated word tokens ("hello,"), the terminal fallback's
    /// interrogative check sees the post-deletion wording, and the dictionary
    /// sees the post-deletion words (a pre-deletion "charge b" would have
    /// fuzzy-matched the custom word).
    #[test]
    fn voice_deletion_sits_between_punctuation_and_dictionary() {
        let settings = AppSettings {
            chinese_script: ChineseScript::AsTranscribed,
            custom_words: vec!["ChargeBee".to_string()],
            ..Default::default()
        };
        let en = OutputLanguageEvidence::UserSelected("en".to_string());
        let supported = languages(&["en"]);

        // "hello," is deleted as one word, leaving nothing for the terminal
        // fallback to punctuate.
        let emptied = post_process_transcription_text(
            "hello comma scratch that".to_string(),
            &settings,
            false,
            &en,
            &supported,
        );
        assert_eq!(emptied, "");

        // The question mark proves the interrogative check ran after the
        // deletion removed "this".
        let question = post_process_transcription_text(
            "what is this scratch that".to_string(),
            &settings,
            false,
            &en,
            &supported,
        );
        assert_eq!(question, "what is?");

        // "charge b" never reaches the dictionary intact: the deletion
        // removes "b" first and "charge" alone stays below the default
        // correction threshold.
        let partial = post_process_transcription_text(
            "hello comma charge b scratch that".to_string(),
            &settings,
            false,
            &en,
            &supported,
        );
        assert_eq!(partial, "hello, charge.");
    }

    /// A "delete everything" command short-circuits the terminal fallback,
    /// the dictionary, and cleanup: the result is exactly empty, with no
    /// appended punctuation and no custom-word substitution.
    #[test]
    fn voice_deletion_cleared_short_circuits_later_passes() {
        let settings = AppSettings {
            chinese_script: ChineseScript::AsTranscribed,
            custom_words: vec!["ChargeBee".to_string()],
            ..Default::default()
        };
        let en = OutputLanguageEvidence::UserSelected("en".to_string());
        let supported = languages(&["en"]);

        let cleared = post_process_transcription_text(
            "what time is it delete everything".to_string(),
            &settings,
            false,
            &en,
            &supported,
        );
        assert_eq!(cleared, "");

        // Everything after the command is discarded too.
        let restarted = post_process_transcription_text(
            "one two three start over four five".to_string(),
            &settings,
            false,
            &en,
            &supported,
        );
        assert_eq!(restarted, "");
    }

    /// Toggling voice deletion off leaves the command words in the text,
    /// byte-for-byte the pre-deletion pipeline.
    #[test]
    fn voice_deletion_toggle_off_preserves_command_words() {
        let settings = text_pipeline_settings(true, true, false);
        let en = OutputLanguageEvidence::UserSelected("en".to_string());
        let supported = languages(&["en"]);

        let result = post_process_transcription_text(
            "hello scratch that world".to_string(),
            &settings,
            false,
            &en,
            &supported,
        );
        assert_eq!(result, "hello scratch that world.");
    }

    #[test]
    fn unapplied_transcribe_cpp_language_is_not_output_evidence() {
        let settings = AppSettings {
            selected_language: "en".to_string(),
            ..Default::default()
        };
        let supported = languages(&[]);
        let plan = transcribe_cpp_run_plan(false, "en", &supported, false);

        assert_eq!(plan.language, None);
        assert_eq!(
            resolve_output_language_evidence(
                &settings,
                plan.language.as_deref(),
                &supported,
                false,
            ),
            OutputLanguageEvidence::Unknown
        );
    }

    #[test]
    fn translated_output_is_treated_as_english() {
        let settings = AppSettings {
            selected_language: "pt".to_string(),
            ..Default::default()
        };

        let evidence = resolve_output_language_evidence(
            &settings,
            Some("pt"),
            &languages(&["en", "pt"]),
            true,
        );

        assert_eq!(evidence, OutputLanguageEvidence::TranslatedToEnglish);
    }

    #[test]
    fn transcribe_cpp_run_plan_skips_english_translation() {
        let plan = transcribe_cpp_run_plan(true, "en", &languages(&["en", "es"]), true);

        assert!(matches!(plan.task, Task::Transcribe));
        assert_eq!(plan.language.as_deref(), Some("en"));
        assert_eq!(plan.target_language, None);
    }

    #[test]
    fn transcribe_cpp_run_plan_translates_supported_non_english() {
        let plan = transcribe_cpp_run_plan(true, "es", &languages(&["en", "es"]), true);

        assert!(matches!(plan.task, Task::Translate));
        assert_eq!(plan.language.as_deref(), Some("es"));
        assert_eq!(plan.target_language.as_deref(), Some("en"));
    }

    #[test]
    fn transcribe_cpp_run_plan_requires_model_translation_support() {
        let plan = transcribe_cpp_run_plan(true, "es", &languages(&["en", "es"]), false);

        assert!(matches!(plan.task, Task::Transcribe));
        assert_eq!(plan.language.as_deref(), Some("es"));
        assert_eq!(plan.target_language, None);
    }

    // --- RAM auto-fallback inventory --------------------------------------

    fn model_info_for(id: &str, size_mb: u64, downloaded: bool) -> ModelInfo {
        ModelInfo {
            id: id.to_string(),
            name: id.to_string(),
            description: String::new(),
            filename: format!("{}.gguf", id),
            source: ModelSource::Local,
            size_mb,
            is_downloaded: downloaded,
            is_downloading: false,
            partial_size: 0,
            is_directory: false,
            engine_type: crate::managers::model::EngineType::TranscribeCpp,
            accuracy_score: 0.0,
            speed_score: 0.0,
            supports_translation: false,
            is_recommended: false,
            supported_languages: vec![],
            supports_language_selection: false,
            is_custom: true,
            supports_streaming: false,
            supports_language_detection: false,
        }
    }

    fn hf_added_model(id: &str, repo_id: &str, filename: &str, size_mb: u64) -> ModelInfo {
        let mut info = model_info_for(id, size_mb, true);
        info.filename = filename.to_string();
        info.source = ModelSource::HuggingFace {
            repo_id: repo_id.to_string(),
            revision: "main".to_string(),
        };
        info.is_custom = false;
        info
    }

    /// A real ranked catalog entry, for rank comparison.
    fn ranked_catalog_model() -> ModelInfo {
        let desc = crate::catalog::CATALOG
            .iter()
            .find(|d| d.recommended_rank.is_some())
            .expect("catalog has ranked models");
        let mut info = desc.to_model_info(&super::super::model::DiskStatus::default());
        info.is_downloaded = true;
        info
    }

    #[test]
    fn fallback_rank_places_unranked_models_after_the_catalog() {
        // A ranked catalog entry resolves to its real (small) rank...
        let catalog = ranked_catalog_model();
        assert!(fallback_rank(&catalog) != u32::MAX);

        // ...while a user-added Hugging Face repo and a local custom model
        // both rank last (u32::MAX): eligible, but only chosen when no
        // cataloged model fits.
        let added = hf_added_model(
            "org/custom-asr/model-Q8_0.gguf",
            "org/custom-asr",
            "model-Q8_0.gguf",
            700,
        );
        assert_eq!(fallback_rank(&added), u32::MAX);
        let custom = model_info_for("my-dropped-model", 100, true);
        assert_eq!(fallback_rank(&custom), u32::MAX);
    }

    #[test]
    fn fallback_inventory_includes_user_added_and_custom_models() {
        let catalog = ranked_catalog_model();
        let added = hf_added_model(
            "org/custom-asr/model-Q8_0.gguf",
            "org/custom-asr",
            "model-Q8_0.gguf",
            700,
        );
        let custom = model_info_for("my-dropped-model", 100, true);
        let not_downloaded = model_info_for("pending-model", 50, false);
        let models = vec![
            catalog.clone(),
            added.clone(),
            custom.clone(),
            not_downloaded,
        ];

        let candidates = fallback_candidate_list(&models, "selected");

        // Exactly the downloaded models other than the failed one, custom and
        // user-added included.
        let mut ids: Vec<&str> = candidates.iter().map(|c| c.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec![
                catalog.id.as_str(),
                "my-dropped-model",
                "org/custom-asr/model-Q8_0.gguf",
            ]
        );
        let added_candidate = candidates
            .iter()
            .find(|c| c.id == "org/custom-asr/model-Q8_0.gguf")
            .unwrap();
        assert_eq!(added_candidate.rank, u32::MAX);
        assert_eq!(
            added_candidate.footprint_bytes,
            700u64 * 1024 * 1024,
            "footprint must stay the size-derived estimate the gate compares"
        );
    }

    #[test]
    fn fallback_resolver_selects_user_added_model_when_it_fits() {
        const GIB: u64 = 1024 * 1024 * 1024;
        let added = hf_added_model(
            "org/custom-asr/model-Q8_0.gguf",
            "org/custom-asr",
            "model-Q8_0.gguf",
            700,
        );
        let candidates = fallback_candidate_list(&[added], "selected");

        // Plenty free: the user-added model is selected.
        assert_eq!(
            memory::resolve_fallback_model(Some(8 * GIB), &candidates, "selected")
                .map(|c| c.id.as_str()),
            Some("org/custom-asr/model-Q8_0.gguf")
        );
        // Too tight for it (700 MiB + 1.5 GiB headroom > 2 GiB): nothing.
        assert_eq!(
            memory::resolve_fallback_model(Some(2 * GIB), &candidates, "selected"),
            None
        );
    }

    #[test]
    fn fallback_resolver_prefers_catalog_over_user_added_when_both_fit() {
        const GIB: u64 = 1024 * 1024 * 1024;
        let catalog = ranked_catalog_model();
        let added = hf_added_model(
            "org/custom-asr/model-Q8_0.gguf",
            "org/custom-asr",
            "model-Q8_0.gguf",
            700,
        );
        let candidates = fallback_candidate_list(&[added.clone(), catalog.clone()], "selected");

        // Both fit, the ranked catalog entry wins...
        assert_eq!(
            memory::resolve_fallback_model(Some(16 * GIB), &candidates, "selected")
                .map(|c| c.id.as_str()),
            Some(catalog.id.as_str())
        );

        // ...and the user-added model takes over once the cataloged one does
        // not fit (a huge forecast here forces that without needing a big
        // catalog model).
        let mut huge = added.clone();
        huge.size_mb = 1; // small: always fits
        let mut huge_catalog = catalog.clone();
        huge_catalog.size_mb = 16 * 1024; // 16 GiB: never fits below
        let forced = fallback_candidate_list(&[huge_catalog, huge], "selected");
        assert_eq!(
            memory::resolve_fallback_model(Some(8 * GIB), &forced, "selected")
                .map(|c| c.id.as_str()),
            Some("org/custom-asr/model-Q8_0.gguf")
        );
    }
}

impl Drop for TranscriptionManager {
    fn drop(&mut self) {
        // Skip shutdown unless this is the very last clone. TranscriptionManager
        // is cloned by initiate_model_load() and the watcher thread - those
        // clones dropping must not kill the watcher. The watcher thread holds
        // its own clone, so onnx's strong_count is always >= 2 while the
        // watcher is alive. When it reaches 1, only this instance remains
        // and we can safely shut down.
        if Arc::strong_count(&self.onnx) > 1 {
            return;
        }

        // Signal the watcher thread to shutdown
        self.shutdown_signal.store(true, Ordering::Relaxed);

        // Wait for the thread to finish gracefully.
        // Use match instead of unwrap to avoid panicking if the mutex is
        // poisoned - a panic inside Drop calls abort().
        let mut guard = match self.watcher_handle.lock() {
            Ok(g) => g,
            Err(e) => {
                warn!("Recovered poisoned watcher_handle mutex during TranscriptionManager drop - a panic occurred earlier this session");
                e.into_inner()
            }
        };
        if let Some(handle) = guard.take() {
            if let Err(e) = handle.join() {
                warn!("Failed to join idle watcher thread: {:?}", e);
            } else {
                debug!("Idle watcher thread joined successfully");
            }
        }
    }
}
