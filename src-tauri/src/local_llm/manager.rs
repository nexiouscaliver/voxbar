//! The exclusive swap runtime (spec sections 2, 3.3, 3.4, 4).
//!
//! [`LlmManager::run_swap`] spawns a DETACHED runner thread that owns the
//! swap lease, the TranscriptionManager loading slot, the worker child
//! (kill-on-drop), and the restore handoff. The caller gets ONLY a
//! oneshot receiver: dropping it is harmless because the runner sends the
//! outcome and ignores a closed channel (requirement L6 - the stop path's
//! `complete_unless_cancelled` can drop the caller's future at any
//! instant, and nothing the caller does can abandon the state machine).
//!
//! The runner executes the pure planner's actions against two injectable
//! seams so every failure path is hermetically testable (T15/T16/T26):
//! [`SwapHost`] (everything manager-shaped: slot, unload, restore, abort
//! signals, gate inputs, events) and [`SwapEngine`] (the worker process).
//! The runner performs no unbounded blocking wait: every phase is a 25ms
//! poll loop that checks abort signals (user cancel, pending press,
//! recording started) and deadlines (per-phase and the 45s total).

use super::forecast;
use super::planner::{
    effective_budget, AbortReason, Action, Phase, Signal, SwapPlanner, SwapState,
};
use super::protocol::{self, WorkerRequest, WorkerResponse};
use super::SkipReason;
use crate::actions::{strip_invisible_chars, strip_think_block, TRANSCRIPTION_FIELD};
use crate::managers::audio::AudioRecordingManager;
use crate::managers::model::ModelManager;
use crate::managers::transcription::{
    decide_memory_gate, memory_gate_refusal_message, LoadingGuard, MemoryGateDecision,
    MemoryGateRefusalPayload, TranscriptionManager,
};
use crate::memory;
use crate::settings::{get_settings, ModelUnloadTimeout};
use crate::TranscriptionCoordinator;
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};
use tauri_specta::Event as _;

/// Emitted when a local post-process pass fell back to the raw transcript
/// (spec 7.3). The frontend toasts it at most once per reason per app
/// session; the memory_gate variant carries the formatted refusal numbers
/// in `detail`.
#[derive(Clone, Debug, Serialize, Deserialize, Type, tauri_specta::Event)]
pub struct PostProcessSkipEvent {
    pub reason: SkipReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Log severity of a skip: engine failures, timeouts, the memory gate, and
/// the fidelity guard are warnings (something broke); the expected,
/// recoverable skips (model not downloaded, transcript over the token cap)
/// are info. Pure, and unit-pinned alongside the line formatters below.
pub(crate) fn skip_log_level(reason: SkipReason) -> log::Level {
    match reason {
        SkipReason::MemoryGate
        | SkipReason::EngineFailed
        | SkipReason::Timeout
        | SkipReason::LengthGuard => log::Level::Warn,
        SkipReason::DownloadMissing | SkipReason::TooLong => log::Level::Info,
    }
}

/// The one structured line every skip writes to voxbar.log. Grep-able on
/// "local post-process skipped", always carries the snake_case reason and
/// the detail when there is one; before this existed a skip left no trace,
/// which is why a raw-fallback dictation was undiagnosable after the fact.
pub(crate) fn skip_log_line(reason: SkipReason, detail: Option<&str>) -> String {
    format!(
        "local post-process skipped: reason={} detail={}",
        skip_reason_str(reason),
        detail.unwrap_or("-")
    )
}

/// The snake_case name matching the SkipReason serde representation, so log
/// lines and event payloads always agree on spelling.
pub(crate) fn skip_reason_str(reason: SkipReason) -> &'static str {
    match reason {
        SkipReason::MemoryGate => "memory_gate",
        SkipReason::DownloadMissing => "download_missing",
        SkipReason::EngineFailed => "engine_failed",
        SkipReason::Timeout => "timeout",
        SkipReason::LengthGuard => "length_guard",
        SkipReason::TooLong => "too_long",
    }
}

/// The terminal success line (matches the cloud path's success shape:
/// outcome plus output length in chars).
pub(crate) fn processed_outcome_log_line(output_chars: usize) -> String {
    format!(
        "local post-process outcome: processed (output {} chars)",
        output_chars
    )
}

/// The terminal raw-fallback line: why the raw transcript won, carried by
/// the planner state at loop exit (which aborted path ended the swap).
pub(crate) fn raw_outcome_log_line(planner_state: super::planner::SwapState) -> String {
    format!(
        "local post-process outcome: raw transcript used (planner state at exit: {:?})",
        planner_state
    )
}

/// The single skip sink: one structured log line, the PostProcessSkipEvent
/// the main window already dedupes into toasts, and the Wave-1 notice
/// channel (in-card row while the card is visible, error sound for the
/// error-toned reasons) so the person mid-dictation learns why raw text
/// landed after the polishing wait.
pub(crate) fn emit_post_process_skip(app: &AppHandle, reason: SkipReason, detail: Option<String>) {
    let line = skip_log_line(reason, detail.as_deref());
    match skip_log_level(reason) {
        log::Level::Warn => warn!("{line}"),
        _ => info!("{line}"),
    }
    let _ = PostProcessSkipEvent {
        reason,
        detail: detail.clone(),
    }
    .emit(app);
    crate::managers::transcription::emit_overlay_notice(
        app,
        crate::managers::transcription::NoticeCode::from_skip_reason(reason),
        detail,
    );
}

/// The result handed back to the caller when the swap finishes. `Raw`
/// means the raw transcript must be used; any skip event explaining why
/// was already emitted by the runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwapOutcome {
    Processed(String),
    Raw,
}

/// Everything one swap needs from the caller. The grammar is the
/// pre-rendered GBNF string (json_schema_to_grammar of the structured
/// output schema); `is_cancelled` is the stop path's cancel-generation
/// closure so user cancellation reaches the runner with zero new plumbing
/// (it is polled, never awaited).
#[derive(Clone)]
pub struct SwapRequest {
    pub transcript: String,
    pub system_prompt: String,
    pub grammar: Option<String>,
    pub is_cancelled: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

/// The bounds the runner enforces. Defaults are the planner's constant
/// table (T32 pins them); tests inject tiny values to exercise the same
/// code paths quickly.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SwapTiming {
    pub voice_unload: Duration,
    pub llm_load: Duration,
    pub generate: Duration,
    pub graceful_exit: Duration,
    pub kill_wait: Duration,
    pub total: Duration,
    pub poll: Duration,
    pub acquire_tick: Duration,
    pub lease_deadline: Duration,
    pub slot_deadline: Duration,
}

impl Default for SwapTiming {
    fn default() -> Self {
        use super::planner::*;
        Self {
            voice_unload: VOICE_UNLOAD_TIMEOUT,
            llm_load: LLM_LOAD_TIMEOUT,
            generate: GENERATE_TIMEOUT,
            graceful_exit: WORKER_GRACEFUL_EXIT_TIMEOUT,
            kill_wait: WORKER_KILL_WAIT_TIMEOUT,
            total: TOTAL_SWAP_DEADLINE,
            poll: ABORT_POLL_INTERVAL,
            acquire_tick: ACQUIRE_RETRY_INTERVAL,
            lease_deadline: LEASE_ACQUIRE_TIMEOUT,
            slot_deadline: SLOT_ACQUIRE_TIMEOUT,
        }
    }
}

/// The gate's inputs for the LLM load (spec section 4 / L4). `free` is the
/// resident-credited reading (probe + outgoing voice credit); the runner
/// refuses on `None`-probe only by failing open, exactly like a voice
/// load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmGateInputs {
    pub guard_enabled: bool,
    pub free: Option<u64>,
    pub headroom: u64,
    pub forecast: u64,
    pub model_name: String,
}

/// The LLM gate wiring, pure (T14): the SAME decide_memory_gate as voice
/// loads, with auto_fallback and allow_fallback BOTH false (L4: no RAM
/// auto-fallback for the LLM ever - a refusal is a refusal; post-process
/// is advisory and silently swapping model variants underneath the user
/// is out of character with the feature).
pub(crate) fn gate_llm_load(inputs: &LlmGateInputs) -> MemoryGateDecision {
    decide_memory_gate(
        inputs.guard_enabled,
        false,
        false,
        inputs.free,
        inputs.forecast,
        inputs.headroom,
        Vec::new,
        super::LOCAL_LLM_MODEL_ID,
    )
}

/// The structured refusal payload and the human-readable message for a
/// refused LLM gate. Reuses the voice-gate formatting so the numbers are
/// real and unit-consistent with every other refusal the app shows.
pub(crate) fn gate_refusal(inputs: &LlmGateInputs) -> (MemoryGateRefusalPayload, String) {
    let free = inputs.free.unwrap_or(0);
    (
        MemoryGateRefusalPayload {
            forecast_bytes: inputs.forecast,
            free_bytes: free,
            headroom_bytes: inputs.headroom,
        },
        memory_gate_refusal_message(&inputs.model_name, inputs.forecast, free, inputs.headroom),
    )
}

/// Validate the worker's completion against the transcript: strip the
/// think belt and invisibles, parse the JSON, extract the transcription
/// field, then apply the CJK-aware fidelity guard. Every failure folds to
/// the raw transcript with its skip reason; the unload path is identical
/// either way.
fn validate_output(
    transcript: &str,
    generated: &str,
) -> Result<String, (SkipReason, Option<String>)> {
    let content = strip_invisible_chars(strip_think_block(generated));
    let extracted = match serde_json::from_str::<serde_json::Value>(&content) {
        Ok(json) => match json.get(TRANSCRIPTION_FIELD).and_then(|t| t.as_str()) {
            Some(field) => strip_invisible_chars(strip_think_block(field)),
            None => {
                return Err((
                    SkipReason::EngineFailed,
                    Some("the local model output had no transcription field".to_string()),
                ))
            }
        },
        Err(_) => {
            return Err((
                SkipReason::EngineFailed,
                Some("the local model output was not valid JSON".to_string()),
            ))
        }
    };
    if forecast::fails_fidelity_guard(transcript, &extracted) {
        return Err((
            SkipReason::LengthGuard,
            Some("the cleaned text collapsed below the fidelity threshold".to_string()),
        ));
    }
    Ok(extracted)
}

/// Everything the runner does to the app, injectable for tests. Production
/// is [`AppSwapHost`]; tests script a fake to pin failure paths without a
/// Tauri app, a model file, or a worker process.
pub(crate) trait SwapHost: Send + Sync {
    /// The loading slot: same claim `initiate_model_load` would make.
    fn try_acquire_slot(&self) -> Option<LoadingGuard>;
    /// Unload the voice model, BLOCKING until the worker exited (called on
    /// the unload helper thread; the runner only polls its completion).
    fn unload_voice(&self);
    /// The restore handoff: transfer the slot into a voice reload.
    fn restore_under_guard(&self, guard: LoadingGuard);
    /// Whether a voice model is resident right now (swap start).
    fn voice_model_is_loaded(&self) -> bool;
    /// Whether the model_unload_timeout setting is Immediately.
    fn unload_timeout_is_immediately(&self) -> bool;
    /// A transcribe press remembered while the pipeline is busy.
    fn has_pending_press(&self) -> bool;
    /// A recording is live right now (the history-retry racer).
    fn is_recording(&self) -> bool;
    /// The pinned model's resolved file path ("" when unresolvable).
    fn model_path(&self) -> String;
    /// Emit the post-process skip event to the frontend.
    fn emit_skip(&self, reason: SkipReason, detail: Option<String>);
    /// The memory gate's inputs for this swap (L4).
    fn gate_inputs(&self) -> LlmGateInputs;
}

/// The worker seam, injectable for tests (T26). Production is
/// [`ProcessEngine`] (the `--llm-worker` child); fakes record effects.
pub(crate) trait SwapEngine: Send {
    /// Spawn the worker (if needed) and send Load; the receiver yields the
    /// load result exactly once.
    fn begin_load(&mut self, path: String, n_ctx: u32) -> Receiver<Result<(), String>>;
    /// Send Generate; the receiver yields the completion text exactly once.
    fn begin_generate(
        &mut self,
        system: String,
        user: String,
        grammar: Option<String>,
        max_gen_tokens: u32,
    ) -> Receiver<Result<String, String>>;
    /// Kill the worker child immediately. Idempotent.
    fn kill(&mut self);
    /// Ask the worker to exit gracefully. Idempotent; safe when dead.
    fn begin_exit(&mut self);
    /// Non-blocking: has the child process exited? Vacuously true when no
    /// child was ever spawned (nothing to wait for).
    fn has_exited(&mut self) -> bool;
    /// The child pid (for the measured-RSS refinement); None before spawn.
    fn worker_pid(&self) -> Option<u32> {
        None
    }
    /// Log the worker's last NATIVE stderr lines (crash diagnostics), once
    /// teardown observes the exit. Default: nothing to log.
    fn log_native_tail(&mut self) {}
}

/// Kill-on-drop child wrapper: if the runner is ever lost (panic), the
/// unwind drops this and the worker dies, so a cancelled swap can never
/// leak a 610 MB worker beside the next voice load. The final belt behind
/// L6, not the primary mechanism.
struct KillOnDropChild(Option<Child>);

impl KillOnDropChild {
    fn kill(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
        }
    }

    fn try_wait(&mut self) -> bool {
        self.0
            .as_mut()
            .is_some_and(|child| child.try_wait().ok().flatten().is_some())
    }
}

impl Drop for KillOnDropChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The production engine: the same executable relaunched with
/// `--llm-worker`, speaking protocol frames on piped stdio. A dedicated
/// reader thread turns response lines into channel messages so the runner
/// can poll them with timeouts.
struct ProcessEngine {
    child: Option<KillOnDropChild>,
    stdin: Option<ChildStdin>,
    responses: Option<Arc<Mutex<Receiver<WorkerResponse>>>>,
    pid: Option<u32>,
    /// The worker's drained stderr crash tail (kept by the shared tail
    /// thread) and the drain-done signal, for teardown diagnostics.
    stderr_tail: Option<Arc<Mutex<VecDeque<String>>>>,
    stderr_done: Option<Receiver<()>>,
}

impl ProcessEngine {
    fn new() -> Self {
        Self {
            child: None,
            stdin: None,
            responses: None,
            pid: None,
            stderr_tail: None,
            stderr_done: None,
        }
    }

    fn ensure_spawned(&mut self) -> Result<(), String> {
        if self.child.is_some() {
            return Ok(());
        }
        // The shared worker spawn contract: the same exe-identity check the
        // transcribe-cpp worker passes, so a self-update applied mid-run
        // can never pair a new-version llm worker with an old parent.
        let exe = crate::engine_supervisor::worker_exe()
            .map_err(|e| format!("cannot resolve the worker executable: {}", e))?;
        let mut command = Command::new(exe);
        command
            .arg(super::worker::WORKER_FLAG)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Piped and drained (never inherited): the shared tail thread
            // forwards every line into voxbar.log, keeps a bounded crash
            // tail, and a full pipe can never block the worker.
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|e| format!("failed to spawn the llm worker: {}", e))?;
        self.pid = Some(child.id());
        let stdin = child.stdin.take();
        if let Some(stderr) = child.stderr.take() {
            let (tail, done) = crate::engine_supervisor::spawn_stderr_tail(
                stderr,
                crate::engine_supervisor::STDERR_TAIL_LINES,
                "llm_worker",
            );
            self.stderr_tail = Some(tail);
            self.stderr_done = Some(done);
        }
        if let Some(stdout) = child.stdout.take() {
            let (tx, rx) = mpsc::channel();
            thread::Builder::new()
                .name("llm-worker-reader".into())
                .spawn(move || {
                    let reader = BufReader::new(stdout);
                    for line in reader.lines() {
                        let Ok(line) = line else { break };
                        if line.trim().is_empty() {
                            continue;
                        }
                        // A malformed line becomes a Failed the awaiting
                        // phase can act on; the reader keeps serving.
                        let response = parse_response_line(&line).unwrap_or_else(|reason| {
                            WorkerResponse::Failed {
                                reason: format!("unparseable worker response: {}", reason),
                            }
                        });
                        if tx.send(response).is_err() {
                            break;
                        }
                    }
                })
                .map_err(|e| format!("failed to start the llm worker reader: {}", e))?;
            self.responses = Some(Arc::new(Mutex::new(rx)));
        }
        self.child = Some(KillOnDropChild(Some(child)));
        self.stdin = stdin;
        Ok(())
    }

    fn send(&mut self, request: &WorkerRequest) -> Result<(), String> {
        let line = protocol::to_line(request);
        let Some(stdin) = self.stdin.as_mut() else {
            return Err("the llm worker is not running".to_string());
        };
        stdin
            .write_all(line.as_bytes())
            .and_then(|_| stdin.flush())
            .map_err(|e| format!("failed to write to the llm worker: {}", e))
    }

    /// Bridge the next worker response into the caller's one-shot channel
    /// on a forwarder thread (the shared reader channel stays owned here).
    fn forward_next_response<T>(
        &self,
        map: impl FnOnce(WorkerResponse) -> T + Send + 'static,
    ) -> Receiver<T>
    where
        T: Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        if let Some(responses) = self.responses.clone() {
            thread::spawn(move || {
                // Blocking recv on this forwarder thread only; the runner
                // polls rx with timeouts.
                let response = match responses.lock() {
                    Ok(responses) => responses.recv().ok(),
                    Err(_) => None,
                };
                if let Some(response) = response {
                    let _ = tx.send(map(response));
                }
            });
        }
        rx
    }
}

fn parse_response_line(line: &str) -> Result<WorkerResponse, String> {
    protocol::parse_response_line(line)
}

impl SwapEngine for ProcessEngine {
    fn begin_load(&mut self, path: String, n_ctx: u32) -> Receiver<Result<(), String>> {
        if let Err(reason) = self.ensure_spawned() {
            let (tx, rx) = mpsc::channel();
            let _ = tx.send(Err(reason));
            return rx;
        }
        if let Err(reason) = self.send(&WorkerRequest::Load { path, n_ctx }) {
            let (tx, rx) = mpsc::channel();
            let _ = tx.send(Err(reason));
            return rx;
        }
        self.forward_next_response(|response| match response {
            WorkerResponse::Loaded => Ok(()),
            WorkerResponse::Failed { reason } => Err(reason),
            WorkerResponse::Generated { .. } => {
                Err("unexpected response for a load request".to_string())
            }
        })
    }

    fn begin_generate(
        &mut self,
        system: String,
        user: String,
        grammar: Option<String>,
        max_gen_tokens: u32,
    ) -> Receiver<Result<String, String>> {
        if let Err(reason) = self.send(&WorkerRequest::Generate {
            system,
            user,
            grammar,
            max_gen_tokens,
        }) {
            let (tx, rx) = mpsc::channel();
            let _ = tx.send(Err(reason));
            return rx;
        }
        self.forward_next_response(|response| match response {
            WorkerResponse::Generated { text } => Ok(text),
            WorkerResponse::Failed { reason } => Err(reason),
            WorkerResponse::Loaded => Err("unexpected response for a generate request".to_string()),
        })
    }

    fn kill(&mut self) {
        if let Some(child) = self.child.as_mut() {
            child.kill();
        }
    }

    fn begin_exit(&mut self) {
        if self.child.is_some() {
            // Best effort: a dead worker's stdin write fails silently.
            let _ = self.send(&WorkerRequest::Exit);
        }
    }

    fn has_exited(&mut self) -> bool {
        // A worker that was never spawned (worker spawn failure is the
        // canonical low-RAM case) has nothing to wait for: reporting
        // false here makes WaitExit burn the full graceful+kill window
        // and then log a false "worker ignored SIGKILL" error.
        match self.child.as_mut() {
            None => true,
            Some(child) => child.try_wait(),
        }
    }

    fn worker_pid(&self) -> Option<u32> {
        self.pid
    }

    fn log_native_tail(&mut self) {
        // Let the drain thread catch the worker's final lines, then report
        // only the RAW native output (structured log lines were already
        // forwarded by the tail thread; re-logging them would be noise).
        if let Some(done) = self.stderr_done.take() {
            let _ = done.recv_timeout(Duration::from_secs(1));
        }
        let Some(tail) = self.stderr_tail.take() else {
            return;
        };
        let native: Vec<String> = tail
            .lock()
            .map(|tail| {
                tail.iter()
                    .filter(|line| !line.starts_with('\u{1}'))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if !native.is_empty() {
            warn!("llm worker native stderr tail:\n{}", native.join("\n"));
        }
    }
}

/// The production host: everything the runner does to the app.
struct AppSwapHost {
    app: AppHandle,
    measured_rss: Arc<OnceLock<u64>>,
}

impl AppSwapHost {
    fn new(app: AppHandle, measured_rss: Arc<OnceLock<u64>>) -> Self {
        Self { app, measured_rss }
    }

    fn tm(&self) -> Arc<TranscriptionManager> {
        self.app
            .state::<Arc<TranscriptionManager>>()
            .inner()
            .clone()
    }
}

impl SwapHost for AppSwapHost {
    fn try_acquire_slot(&self) -> Option<LoadingGuard> {
        self.tm().try_start_loading()
    }

    fn unload_voice(&self) {
        if let Err(e) = self.tm().unload_model() {
            // unload_model always returns Ok today; log defensively if
            // that ever changes so the timeout path stays diagnosable.
            warn!("voice model unload reported an error: {}", e);
        }
    }

    fn restore_under_guard(&self, guard: LoadingGuard) {
        self.tm().restore_model_under_guard(guard);
    }

    fn voice_model_is_loaded(&self) -> bool {
        self.tm().is_model_loaded()
    }

    fn unload_timeout_is_immediately(&self) -> bool {
        get_settings(&self.app).model_unload_timeout == ModelUnloadTimeout::Immediately
    }

    fn has_pending_press(&self) -> bool {
        self.app
            .try_state::<TranscriptionCoordinator>()
            .is_some_and(|c| c.has_pending_press())
    }

    fn is_recording(&self) -> bool {
        self.app
            .try_state::<Arc<AudioRecordingManager>>()
            .is_some_and(|a| a.is_recording())
    }

    fn model_path(&self) -> String {
        self.app
            .state::<Arc<ModelManager>>()
            .get_model_path(super::LOCAL_LLM_MODEL_ID)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    fn emit_skip(&self, reason: SkipReason, detail: Option<String>) {
        emit_post_process_skip(&self.app, reason, detail);
    }

    fn gate_inputs(&self) -> LlmGateInputs {
        let settings = get_settings(&self.app);
        let file_size_bytes = self
            .app
            .state::<Arc<ModelManager>>()
            .get_model_info(super::LOCAL_LLM_MODEL_ID)
            .map(|info| info.size_mb.saturating_mul(1024 * 1024))
            .unwrap_or_else(|| {
                warn!("the local LLM model entry is missing; using the pinned size");
                super::LOCAL_LLM_MODEL_SIZE_MB.saturating_mul(1024 * 1024)
            });
        // L5: runtime-inclusive forecast; the measured RSS (captured after
        // the first successful generation this launch) corrects the 3/2
        // file-size floor once available. The same shared helper the voice
        // gate composes through, so both gates forecast identically.
        let forecast_bytes =
            forecast::runtime_inclusive_bytes(file_size_bytes, self.measured_rss.get().copied());
        // The voice model is still resident at gate time and its pages are
        // freed before the LLM's peak: credit its footprint back to free,
        // exactly like the voice loader's drop-old-first credit.
        let credit = self.tm().resident_model_footprint_bytes();
        let probe = memory::probe_availability();
        if probe.available_bytes.is_none() {
            warn!(
                "memory gate: available-memory probe unavailable for the local LLM load; \
                 failing open"
            );
        }
        LlmGateInputs {
            guard_enabled: settings.memory_pressure_guard,
            free: probe
                .available_bytes
                .map(|free| free.saturating_add(credit)),
            headroom: settings.memory_gate_headroom_mb.saturating_mul(1024 * 1024),
            forecast: forecast_bytes,
            model_name: "Qwen3 0.6B (post-process)".to_string(),
        }
    }
}

/// The exclusive swap manager: one swap at a time (`swap_running` is the
/// L3 lease probe the delete paths consult), the lease mutex the runner
/// actually holds, and this launch's measured worker RSS (never
/// persisted).
#[derive(Clone)]
pub struct LlmManager {
    lease: Arc<Mutex<()>>,
    swap_running: Arc<AtomicBool>,
    measured_rss: Arc<OnceLock<u64>>,
}

impl Default for LlmManager {
    fn default() -> Self {
        Self::new()
    }
}

impl LlmManager {
    pub fn new() -> Self {
        Self {
            lease: Arc::new(Mutex::new(())),
            swap_running: Arc::new(AtomicBool::new(false)),
            measured_rss: Arc::new(OnceLock::new()),
        }
    }

    /// Whether a swap is running RIGHT NOW. A cheap, non-blocking probe
    /// for the model-delete paths (L3): they refuse transiently instead of
    /// deleting a voice model file the swap may be about to restore.
    pub fn swap_in_progress(&self) -> bool {
        self.swap_running.load(Ordering::Acquire)
    }

    /// Run one exclusive swap and return ONLY the outcome receiver. The
    /// runner thread is detached: dropping the receiver never abandons the
    /// state machine, kills the worker, or strands the loading slot (L6).
    pub fn run_swap(
        &self,
        app: &AppHandle,
        request: SwapRequest,
    ) -> tokio::sync::oneshot::Receiver<SwapOutcome> {
        let (rx, _handle) = self.spawn_runner(
            request,
            RunnerConfig {
                timing: SwapTiming::default(),
                host: Arc::new(AppSwapHost::new(
                    app.clone(),
                    Arc::clone(&self.measured_rss),
                )),
                engine_factory: Box::new(|| Box::new(ProcessEngine::new())),
            },
        );
        // Detach: the runner owns everything and always terminates.
        rx
    }

    fn spawn_runner(
        &self,
        request: SwapRequest,
        cfg: RunnerConfig,
    ) -> (
        tokio::sync::oneshot::Receiver<SwapOutcome>,
        Option<thread::JoinHandle<()>>,
    ) {
        // Set the lease probe BEFORE the thread starts so a delete racing
        // this call can never slip between spawn and acquisition.
        self.swap_running.store(true, Ordering::Release);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let llm = self.clone();
        let spawned = thread::Builder::new()
            .name("llm-swap".into())
            .spawn(move || {
                // The panic belt: panic=unwind even in release builds, so
                // the unwind drops the engine (kill-on-drop), the slot
                // guard, and the lease; the probe clears here regardless.
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    swap_runner(&llm, &request, cfg)
                }))
                .unwrap_or_else(|panic| {
                    error!(
                        "local post-process swap runner panicked: {}; recovering via the \
                             ordinary lazy-load path",
                        panic_payload(&panic)
                    );
                    SwapOutcome::Raw
                });
                llm.swap_running.store(false, Ordering::Release);
                // A dropped receiver is harmless: the runner is done either
                // way and never blocks on the send.
                let _ = tx.send(outcome);
            });
        let handle = self.recover_from_failed_spawn(spawned);
        (rx, handle)
    }

    /// The runner thread itself can fail to start (memory exhaustion is
    /// the canonical hostile case). That failure must neither panic the
    /// awaiting stop task nor wedge the swap probe: clear the probe HERE
    /// (the closure that owns the other clear never ran). The failed
    /// spawn also drops the closure, which drops the outcome sender, so
    /// the awaiting stop task's receiver resolves Err immediately and its
    /// match falls back to the raw transcript, exactly the recovery the
    /// panic belt uses. Without the probe clear, every model delete
    /// would refuse forever ("post-processing is in progress") until an
    /// app restart.
    fn recover_from_failed_spawn(
        &self,
        spawned: std::io::Result<thread::JoinHandle<()>>,
    ) -> Option<thread::JoinHandle<()>> {
        match spawned {
            Ok(handle) => Some(handle),
            Err(e) => {
                self.swap_running.store(false, Ordering::Release);
                error!(
                    "failed to spawn the llm swap runner thread: {}; the swap did not run and \
                     the caller falls back to the raw transcript",
                    e
                );
                None
            }
        }
    }
}

fn panic_payload(panic: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

struct RunnerConfig {
    timing: SwapTiming,
    host: Arc<dyn SwapHost>,
    engine_factory: Box<dyn FnOnce() -> Box<dyn SwapEngine> + Send>,
}

/// The executor. Synchronous and test-callable: every wait is a bounded
/// poll loop; every terminal path hands the slot off or drops it; the
/// lease is held for the whole swap and released at the end.
fn swap_runner(llm: &LlmManager, request: &SwapRequest, cfg: RunnerConfig) -> SwapOutcome {
    let timing = cfg.timing;

    // The input cap (L10) skips the whole swap: no unload, no worker, raw
    // transcript out. This is the tiny-model quality cliff.
    if forecast::input_exceeds_cap(&request.transcript) {
        cfg.host.emit_skip(
            SkipReason::TooLong,
            Some(format!(
                "transcript exceeds the {}-token local budget",
                forecast::INPUT_TOKEN_CAP
            )),
        );
        return SwapOutcome::Raw;
    }

    let mut planner = SwapPlanner::new(
        cfg.host.voice_model_is_loaded(),
        cfg.host.unload_timeout_is_immediately(),
    );
    let total_start = Instant::now();
    let abort = |host: &Arc<dyn SwapHost>| -> Option<AbortReason> {
        if let Some(is_cancelled) = &request.is_cancelled {
            if is_cancelled() {
                return Some(AbortReason::UserCancel);
            }
        }
        if host.has_pending_press() {
            return Some(AbortReason::PressPending);
        }
        if host.is_recording() {
            return Some(AbortReason::RecordingStarted);
        }
        if total_start.elapsed() >= timing.total {
            return Some(AbortReason::TotalDeadline);
        }
        None
    };

    let mut engine = (cfg.engine_factory)();
    let mut slot: Option<LoadingGuard> = None;
    let lease = Arc::clone(&llm.lease);
    let mut lease_guard: Option<MutexGuard<'_, ()>> = None;
    let mut processed: Option<String> = None;
    let mut signal: Option<Signal> = Some(Signal::Start);

    debug!("local post-process swap starting (context: {:?})", planner);

    while !planner.is_done() {
        if signal.is_none() {
            // RestoringVoice: the runner computes the restore decision
            // (should_restore + the terminal handoff rule) and feeds it.
            debug_assert_eq!(
                planner.state,
                SwapState::RestoringVoice,
                "only the restore decision produces no signal of its own"
            );
            let restore = planner.restore_decision(cfg.host.is_recording());
            signal = Some(if restore {
                Signal::RestoreHandedOff
            } else {
                Signal::RestoreSkipped
            });
        }
        let step_signal = signal
            .take()
            .expect("signal set above or by the prior step");
        let actions = planner.step(step_signal, cfg.host.is_recording());

        'actions: for action in actions {
            match action {
                Action::TakeLease => {
                    let deadline = Instant::now() + timing.lease_deadline;
                    loop {
                        if let Some(reason) = abort(&cfg.host) {
                            signal = Some(Signal::Abort(reason));
                            break 'actions;
                        }
                        if let Ok(guard) = lease.try_lock() {
                            lease_guard = Some(guard);
                            planner.mark_lease_acquired();
                            break;
                        }
                        if Instant::now() >= deadline {
                            info!("local post-process skipped: another swap holds the lease");
                            signal = Some(Signal::LeaseDenied);
                            break 'actions;
                        }
                        thread::sleep(timing.acquire_tick);
                    }
                }
                Action::TakeSlot => {
                    let deadline = Instant::now() + timing.slot_deadline;
                    loop {
                        if let Some(reason) = abort(&cfg.host) {
                            signal = Some(Signal::Abort(reason));
                            break 'actions;
                        }
                        if let Some(guard) = cfg.host.try_acquire_slot() {
                            slot = Some(guard);
                            planner.mark_slot_acquired();
                            break;
                        }
                        if Instant::now() >= deadline {
                            info!(
                                "local post-process skipped: a model load holds the loading slot"
                            );
                            signal = Some(Signal::SlotDenied);
                            break 'actions;
                        }
                        thread::sleep(timing.acquire_tick);
                    }
                }
                Action::Gate => {
                    let inputs = cfg.host.gate_inputs();
                    match gate_llm_load(&inputs) {
                        MemoryGateDecision::Allow => {
                            signal = Some(Signal::GateAllowed);
                        }
                        MemoryGateDecision::Refuse => {
                            let (payload, message) = gate_refusal(&inputs);
                            info!(
                                "local post-process refused by the memory gate: {} \
                                 (forecast_bytes={} free_bytes={} headroom_bytes={})",
                                message,
                                payload.forecast_bytes,
                                payload.free_bytes,
                                payload.headroom_bytes
                            );
                            signal = Some(Signal::GateRefused { detail: message });
                        }
                        // Unreachable: the LLM gate runs with both fallback
                        // flags false and no candidates.
                        MemoryGateDecision::Fallback(id) => {
                            error!("local post-process gate produced a fallback ({})", id);
                            signal = Some(Signal::GateRefused {
                                detail: "internal error: the LLM gate cannot fall back".to_string(),
                            });
                        }
                    }
                }
                Action::UnloadVoice => {
                    // The restore context must describe the model this
                    // swap is ABOUT to unload, not the one resident when
                    // the runner started: the lease/slot waits above can
                    // span another load completing (or the resident model
                    // timing out). Refresh under the held slot so no load
                    // can slip between this read and the unload below.
                    planner.refresh_restore_context(
                        cfg.host.voice_model_is_loaded(),
                        cfg.host.unload_timeout_is_immediately(),
                    );
                    // The unload helper thread: tm.unload_model() blocks
                    // inside Unloading::wait with no timeout variant, so
                    // the runner polls this flag instead of joining.
                    let done = Arc::new(AtomicBool::new(false));
                    {
                        let done = Arc::clone(&done);
                        let host = Arc::clone(&cfg.host);
                        let _ = thread::Builder::new()
                            .name("llm-swap-unload-helper".into())
                            .spawn(move || {
                                host.unload_voice();
                                done.store(true, Ordering::Release);
                            });
                    }
                    let deadline = Instant::now() + timing.voice_unload;
                    loop {
                        if done.load(Ordering::Acquire) {
                            signal = Some(Signal::VoiceUnloaded);
                            break;
                        }
                        if let Some(reason) = abort(&cfg.host) {
                            signal = Some(Signal::Abort(reason));
                            break 'actions;
                        }
                        if Instant::now() >= deadline {
                            warn!(
                                "voice model unload timed out after {:?}; the local LLM will \
                                 NOT load (voice RAM not provably freed); the queued unload \
                                 completes on its own",
                                timing.voice_unload
                            );
                            signal = Some(Signal::VoiceUnloadTimeout);
                            break 'actions;
                        }
                        thread::sleep(timing.poll);
                    }
                }
                // The worker process is created lazily by begin_load.
                Action::SpawnWorker => {}
                Action::LoadLlm => {
                    let budget = effective_budget(
                        timing.llm_load,
                        timing.total.saturating_sub(total_start.elapsed()),
                    );
                    let path = cfg.host.model_path();
                    let rx = engine.begin_load(path, protocol::WORKER_N_CTX);
                    let deadline = Instant::now() + budget;
                    loop {
                        match rx.recv_timeout(timing.poll) {
                            Ok(Ok(())) => {
                                signal = Some(Signal::LlmLoaded);
                                break;
                            }
                            Ok(Err(reason)) => {
                                signal = Some(Signal::LlmLoadFailed { reason });
                                break;
                            }
                            Err(RecvTimeoutError::Timeout) => {
                                if total_start.elapsed() >= timing.total {
                                    signal = Some(Signal::Timeout(Phase::LoadingLlm));
                                    break 'actions;
                                }
                                if let Some(reason) = abort(&cfg.host) {
                                    signal = Some(Signal::Abort(reason));
                                    break 'actions;
                                }
                                if Instant::now() >= deadline {
                                    signal = Some(Signal::Timeout(Phase::LoadingLlm));
                                    break 'actions;
                                }
                            }
                            Err(RecvTimeoutError::Disconnected) => {
                                signal = Some(Signal::LlmLoadFailed {
                                    reason: "the llm worker closed its response channel"
                                        .to_string(),
                                });
                                break;
                            }
                        }
                    }
                }
                Action::Generate => {
                    // /no_think appended to the user message is the ONLY
                    // thinking-off mechanism this engine version supports
                    // (R4); strip_think_block and the fidelity guard are
                    // the belts.
                    let user = format!("{}\n/no_think", request.transcript);
                    let budget = effective_budget(
                        timing.generate,
                        timing.total.saturating_sub(total_start.elapsed()),
                    );
                    let rx = engine.begin_generate(
                        request.system_prompt.clone(),
                        user,
                        request.grammar.clone(),
                        forecast::max_gen_tokens(&request.transcript),
                    );
                    let deadline = Instant::now() + budget;
                    loop {
                        match rx.recv_timeout(timing.poll) {
                            Ok(Ok(text)) => {
                                // L5: capture the worker's RSS after the
                                // first successful generation this launch
                                // so later gates forecast from measurement.
                                if let (None, Some(pid)) =
                                    (llm.measured_rss.get(), engine.worker_pid())
                                {
                                    if let Some(rss) = memory::rss_bytes_for_pid(pid) {
                                        let _ = llm.measured_rss.set(rss);
                                        debug!(
                                            "measured llm worker RSS: {} bytes (forecast \
                                             refinement for later swaps)",
                                            rss
                                        );
                                    }
                                }
                                match validate_output(&request.transcript, &text) {
                                    Ok(clean) => {
                                        processed = Some(clean);
                                    }
                                    Err((reason, detail)) => {
                                        // Invalid output folds to raw; the
                                        // unload path below is identical.
                                        cfg.host.emit_skip(reason, detail);
                                    }
                                }
                                signal = Some(Signal::LlmGenerated { text });
                                break;
                            }
                            Ok(Err(reason)) => {
                                signal = Some(Signal::LlmGenFailed { reason });
                                break;
                            }
                            Err(RecvTimeoutError::Timeout) => {
                                if total_start.elapsed() >= timing.total {
                                    signal = Some(Signal::Timeout(Phase::Generating));
                                    break 'actions;
                                }
                                if let Some(reason) = abort(&cfg.host) {
                                    signal = Some(Signal::Abort(reason));
                                    break 'actions;
                                }
                                if Instant::now() >= deadline {
                                    signal = Some(Signal::Timeout(Phase::Generating));
                                    break 'actions;
                                }
                            }
                            Err(RecvTimeoutError::Disconnected) => {
                                signal = Some(Signal::LlmGenFailed {
                                    reason: "the llm worker closed its response channel"
                                        .to_string(),
                                });
                                break;
                            }
                        }
                    }
                }
                Action::KillWorker => engine.kill(),
                Action::WaitExit => {
                    engine.begin_exit();
                    let start = Instant::now();
                    let graceful_deadline = start + timing.graceful_exit;
                    let hard_deadline = start + timing.graceful_exit + timing.kill_wait;
                    loop {
                        if engine.has_exited() {
                            engine.log_native_tail();
                            signal = Some(Signal::LlmUnloaded);
                            break;
                        }
                        let now = Instant::now();
                        if now >= hard_deadline {
                            error!(
                                "llm worker ignored SIGKILL and the {:?} wait bound; marking \
                                 the worker slot poisoned and restoring the voice model",
                                timing.kill_wait
                            );
                            engine.log_native_tail();
                            signal = Some(Signal::LlmKillTimedOut);
                            break;
                        }
                        // Teardown is never abandoned: an abort here only
                        // accelerates it (straight to kill).
                        if abort(&cfg.host).is_some() || now >= graceful_deadline {
                            engine.kill();
                        }
                        thread::sleep(timing.poll);
                    }
                }
                Action::RestoreHandoff => match slot.take() {
                    Some(guard) => {
                        cfg.host.restore_under_guard(guard);
                        info!("voice model restore handed off under the transferred slot");
                    }
                    None => {
                        error!("restore handoff requested but the runner holds no slot");
                    }
                },
                Action::DropSlotGuard => {
                    if let Some(guard) = slot.take() {
                        drop(guard);
                    }
                }
                Action::ReleaseLease => {
                    drop(lease_guard.take());
                }
                Action::EmitSkip { reason, detail } => {
                    cfg.host.emit_skip(reason, detail);
                }
            }
        }
    }

    // Final belt: nothing may leak past the loop whatever happened above.
    drop(slot);
    drop(lease_guard);

    match (&processed, planner.is_done()) {
        (Some(clean), true) => {
            info!("{}", processed_outcome_log_line(clean.len()));
            SwapOutcome::Processed(clean.clone())
        }
        _ => {
            info!("{}", raw_outcome_log_line(planner.state));
            SwapOutcome::Raw
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managers::transcription::LoadingGuard as RealLoadingGuard;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Condvar;

    /// Every SkipReason logs one grep-able line at the right severity: the
    /// broke-something reasons warn, the expected/recoverable ones stay at
    /// info. The line always carries the snake_case reason so log analysis
    /// and the event payload agree on spelling.
    #[test]
    fn skip_log_lines_cover_every_reason_at_the_right_level() {
        let cases = [
            (SkipReason::MemoryGate, log::Level::Warn),
            (SkipReason::EngineFailed, log::Level::Warn),
            (SkipReason::Timeout, log::Level::Warn),
            (SkipReason::LengthGuard, log::Level::Warn),
            (SkipReason::DownloadMissing, log::Level::Info),
            (SkipReason::TooLong, log::Level::Info),
        ];
        for (reason, level) in cases {
            assert_eq!(skip_log_level(reason), level, "{reason:?} severity");
            let line = skip_log_line(reason, Some("because"));
            assert!(
                line.starts_with("local post-process skipped: reason="),
                "line shape: {line}"
            );
            assert!(
                line.contains(skip_reason_str(reason)),
                "reason in line: {line}"
            );
            assert!(line.ends_with("detail=because"), "detail in line: {line}");
            // No detail still produces a complete line.
            assert!(skip_log_line(reason, None).ends_with("detail=-"));
        }
    }

    /// Both terminal outcomes leave a line: Processed reports the output
    /// length (the cloud path's success shape), Raw reports the planner
    /// state at loop exit so the raw fallback is diagnosable from the log
    /// alone (history id 97's raw fallback left no trace before this).
    #[test]
    fn terminal_outcome_log_lines_name_the_outcome() {
        let processed = processed_outcome_log_line(1234);
        assert!(processed.starts_with("local post-process outcome: processed"));
        assert!(processed.contains("1234 chars"), "{processed}");

        let raw = raw_outcome_log_line(super::super::planner::SwapState::Done);
        assert!(raw.starts_with("local post-process outcome: raw transcript used"));
        assert!(raw.contains("Done"), "planner state rides the line: {raw}");
    }

    fn mib(mb: u64) -> u64 {
        mb * 1024 * 1024
    }

    /// The scripted host: every knob a failure path needs, with the same
    /// flag/guard mechanics the real TranscriptionManager uses.
    struct FakeHost {
        slot_flag: Arc<Mutex<bool>>,
        slot_condvar: Arc<Condvar>,
        slot_denied: Arc<AtomicBool>,
        block_unload: Arc<AtomicBool>,
        unload_started: Arc<AtomicBool>,
        restored: Arc<AtomicUsize>,
        pending_press: Arc<AtomicBool>,
        recording: Arc<AtomicBool>,
        skips: Arc<Mutex<Vec<(SkipReason, Option<String>)>>>,
        voice_loaded: Arc<AtomicBool>,
        immediately: Arc<AtomicBool>,
        gate_refuse: Arc<AtomicBool>,
        panic_at_restore: Arc<AtomicBool>,
    }

    impl FakeHost {
        fn new() -> Self {
            Self {
                slot_flag: Arc::new(Mutex::new(false)),
                slot_condvar: Arc::new(Condvar::new()),
                slot_denied: Arc::new(AtomicBool::new(false)),
                block_unload: Arc::new(AtomicBool::new(false)),
                unload_started: Arc::new(AtomicBool::new(false)),
                restored: Arc::new(AtomicUsize::new(0)),
                pending_press: Arc::new(AtomicBool::new(false)),
                recording: Arc::new(AtomicBool::new(false)),
                skips: Arc::new(Mutex::new(Vec::new())),
                voice_loaded: Arc::new(AtomicBool::new(true)),
                immediately: Arc::new(AtomicBool::new(false)),
                gate_refuse: Arc::new(AtomicBool::new(false)),
                panic_at_restore: Arc::new(AtomicBool::new(false)),
            }
        }

        fn slot_is_free(&self) -> bool {
            !*self.slot_flag.lock().unwrap()
        }

        fn skips_contain(&self, reason: SkipReason) -> bool {
            self.skips.lock().unwrap().iter().any(|(r, _)| *r == reason)
        }
    }

    impl SwapHost for FakeHost {
        fn try_acquire_slot(&self) -> Option<RealLoadingGuard> {
            if self.slot_denied.load(Ordering::Acquire) {
                return None;
            }
            let mut flag = self.slot_flag.lock().unwrap();
            if *flag {
                return None;
            }
            *flag = true;
            Some(RealLoadingGuard::new(
                Arc::clone(&self.slot_flag),
                Arc::clone(&self.slot_condvar),
            ))
        }

        fn unload_voice(&self) {
            self.unload_started.store(true, Ordering::Release);
            if self.block_unload.load(Ordering::Acquire) {
                // Never returns: the runner must hit its bounded timeout
                // while this helper stays blocked holding no locks.
                loop {
                    thread::park();
                }
            }
            // A fast "unload": nothing else to simulate.
        }

        fn restore_under_guard(&self, guard: RealLoadingGuard) {
            if self.panic_at_restore.load(Ordering::Acquire) {
                panic!("injected restore panic for the belt test");
            }
            self.restored.fetch_add(1, Ordering::Release);
            // The restore load "completes" immediately: the guard drop
            // clears the slot exactly like the real restore loader's end.
            drop(guard);
        }

        fn voice_model_is_loaded(&self) -> bool {
            self.voice_loaded.load(Ordering::Acquire)
        }

        fn unload_timeout_is_immediately(&self) -> bool {
            self.immediately.load(Ordering::Acquire)
        }

        fn has_pending_press(&self) -> bool {
            self.pending_press.load(Ordering::Acquire)
        }

        fn is_recording(&self) -> bool {
            self.recording.load(Ordering::Acquire)
        }

        fn model_path(&self) -> String {
            "/nonexistent/qwen-smoke.gguf".to_string()
        }

        fn emit_skip(&self, reason: SkipReason, detail: Option<String>) {
            self.skips.lock().unwrap().push((reason, detail));
        }

        fn gate_inputs(&self) -> LlmGateInputs {
            let refuse = self.gate_refuse.load(Ordering::Acquire);
            LlmGateInputs {
                guard_enabled: true,
                free: Some(if refuse { mib(700) } else { mib(4096) }),
                headroom: 0,
                forecast: mib(610) * 3 / 2,
                model_name: "Qwen3 0.6B (post-process)".to_string(),
            }
        }
    }

    /// The recorded state shared between the fake engine and the test.
    #[derive(Default)]
    struct FakeEngineState {
        killed: AtomicBool,
        exited: AtomicBool,
        exit_sent: AtomicBool,
        generate_users: Mutex<Vec<String>>,
    }

    struct FakeEngine {
        state: Arc<FakeEngineState>,
        load_result: Result<(), String>,
        load_delay: Duration,
        generate_result: Result<String, String>,
        generate_delay: Duration,
        /// Keep never-fired senders alive so recv blocks forever (T27).
        never_senders: Vec<mpsc::Sender<Result<String, String>>>,
        never_generate: bool,
    }

    impl FakeEngine {
        fn happy(generate: Result<String, String>) -> Self {
            Self {
                state: Arc::new(FakeEngineState::default()),
                load_result: Ok(()),
                load_delay: Duration::from_millis(5),
                generate_result: generate,
                generate_delay: Duration::from_millis(5),
                never_senders: Vec::new(),
                never_generate: false,
            }
        }
    }

    impl Drop for FakeEngine {
        fn drop(&mut self) {
            // Kill-on-drop belt semantics, like the real wrapper.
            self.state.killed.store(true, Ordering::Release);
        }
    }

    impl SwapEngine for FakeEngine {
        fn begin_load(&mut self, _path: String, _n_ctx: u32) -> Receiver<Result<(), String>> {
            let (tx, rx) = mpsc::channel();
            let result = self.load_result.clone();
            let delay = self.load_delay;
            thread::spawn(move || {
                thread::sleep(delay);
                let _ = tx.send(result);
            });
            rx
        }

        fn begin_generate(
            &mut self,
            _system: String,
            user: String,
            _grammar: Option<String>,
            _max_gen_tokens: u32,
        ) -> Receiver<Result<String, String>> {
            self.state.generate_users.lock().unwrap().push(user);
            let (tx, rx) = mpsc::channel();
            if self.never_generate {
                // Keep the sender alive so the runner's recv_timeout
                // blocks until a deadline fires, never erroring instantly.
                self.never_senders.push(tx);
                return rx;
            }
            let result = self.generate_result.clone();
            let delay = self.generate_delay;
            thread::spawn(move || {
                thread::sleep(delay);
                let _ = tx.send(result);
            });
            rx
        }

        fn kill(&mut self) {
            self.state.killed.store(true, Ordering::Release);
            self.state.exited.store(true, Ordering::Release);
        }

        fn begin_exit(&mut self) {
            self.state.exit_sent.store(true, Ordering::Release);
            // The fake worker exits promptly on a graceful Exit.
            self.state.exited.store(true, Ordering::Release);
        }

        fn has_exited(&mut self) -> bool {
            self.state.exited.load(Ordering::Acquire)
        }
    }

    fn tiny_timing() -> SwapTiming {
        SwapTiming {
            voice_unload: Duration::from_millis(120),
            llm_load: Duration::from_millis(200),
            generate: Duration::from_millis(200),
            graceful_exit: Duration::from_millis(100),
            kill_wait: Duration::from_millis(200),
            total: Duration::from_millis(2_000),
            poll: Duration::from_millis(5),
            acquire_tick: Duration::from_millis(5),
            lease_deadline: Duration::from_millis(100),
            slot_deadline: Duration::from_millis(100),
        }
    }

    fn runner_cfg(host: FakeHost, engine: FakeEngine) -> (RunnerConfig, Arc<FakeEngineState>) {
        let engine_state = Arc::clone(&engine.state);
        let cfg = RunnerConfig {
            timing: tiny_timing(),
            host: Arc::new(host),
            engine_factory: Box::new(move || Box::new(engine)),
        };
        (cfg, engine_state)
    }

    fn sample_request() -> SwapRequest {
        SwapRequest {
            transcript: "um hello world this is a test transcript".to_string(),
            system_prompt: "clean the transcript".to_string(),
            grammar: Some("root ::= ...".to_string()),
            is_cancelled: None,
        }
    }

    /// T26 shape: the happy path through the real runner with the fakes,
    /// proving the engine/host seams cover the full machine.
    #[test]
    fn happy_path_processes_and_restores() {
        let host = FakeHost::new();
        let restored = Arc::clone(&host.restored);
        let (cfg, engine_state) = runner_cfg(
            host,
            FakeEngine::happy(Ok("{\"transcription\": \"Hello, world.\"}".to_string())),
        );
        let llm = LlmManager::new();
        let outcome = swap_runner(&llm, &sample_request(), cfg);
        assert_eq!(outcome, SwapOutcome::Processed("Hello, world.".to_string()));
        assert_eq!(
            restored.load(Ordering::Acquire),
            1,
            "the dictation context restores the voice model"
        );
        assert!(engine_state.exit_sent.load(Ordering::Acquire));
    }

    /// T29: the Generate frame's user content ends with /no_think (the
    /// only thinking-off mechanism, R4).
    #[test]
    fn generate_user_content_ends_with_no_think() {
        let host = FakeHost::new();
        let (cfg, engine_state) = runner_cfg(
            host,
            FakeEngine::happy(Ok("{\"transcription\": \"Clean.\"}".to_string())),
        );
        let llm = LlmManager::new();
        let outcome = swap_runner(&llm, &sample_request(), cfg);
        assert!(matches!(outcome, SwapOutcome::Processed(_)));
        let users = engine_state.generate_users.lock().unwrap().clone();
        assert_eq!(users.len(), 1);
        assert!(
            users[0].ends_with("/no_think"),
            "user content must end with /no_think, got: {:?}",
            users[0]
        );
        assert!(users[0].starts_with("um hello world"));
    }

    /// T14: the gate wiring refuses with real numbers at the boundary and
    /// can never fall back (both fallback flags false by construction).
    #[test]
    fn llm_gate_wiring_refuses_with_real_numbers() {
        let refuse = LlmGateInputs {
            guard_enabled: true,
            free: Some(mib(700)),
            headroom: 0,
            forecast: mib(610) * 3 / 2,
            model_name: "Qwen3 0.6B (post-process)".to_string(),
        };
        assert_eq!(gate_llm_load(&refuse), MemoryGateDecision::Refuse);

        // Contrast: the same numbers WITH fallback enabled would fall
        // back on a voice load; the LLM path structurally cannot.
        assert_ne!(
            decide_memory_gate(
                true,
                true,
                true,
                refuse.free,
                refuse.forecast,
                refuse.headroom,
                || vec![memory::FallbackCandidate {
                    rank: 0,
                    footprint_bytes: mib(100),
                    id: "small".to_string(),
                }],
                "failed"
            ),
            MemoryGateDecision::Refuse
        );

        // Plenty free: allowed.
        let allow = LlmGateInputs {
            free: Some(mib(4096)),
            ..refuse.clone()
        };
        assert_eq!(gate_llm_load(&allow), MemoryGateDecision::Allow);

        // The guard toggle bypasses entirely.
        let bypass = LlmGateInputs {
            guard_enabled: false,
            ..refuse
        };
        assert_eq!(gate_llm_load(&bypass), MemoryGateDecision::Allow);

        // The refusal message carries the real numbers (915 MB forecast
        // against 700 MB free), unit-consistent with voice refusals.
        let inputs = LlmGateInputs {
            guard_enabled: true,
            free: Some(mib(700)),
            headroom: mib(256),
            forecast: mib(610) * 3 / 2,
            model_name: "Qwen3 0.6B (post-process)".to_string(),
        };
        let (payload, message) = gate_refusal(&inputs);
        assert_eq!(payload.forecast_bytes, mib(915));
        assert_eq!(payload.free_bytes, mib(700));
        assert_eq!(payload.headroom_bytes, mib(256));
        assert!(message.contains("915 MB"), "message: {}", message);
        assert!(message.contains("700 MB"), "message: {}", message);
        assert!(message.contains("256 MB"), "message: {}", message);
    }

    /// T27: a Generate that never finishes trips the total deadline; the
    /// runner kills the worker, still restores (dictation context), and
    /// reports the timeout skip. Asserted on effects, with a generous real
    /// wall-clock bound.
    #[test]
    fn never_finishing_generate_trips_the_total_deadline() {
        let started = Instant::now();
        let host = FakeHost::new();
        let mut engine = FakeEngine::happy(Ok(String::new()));
        engine.never_generate = true;
        let (mut cfg, engine_state) = runner_cfg(host, engine);
        cfg.timing.total = Duration::from_millis(150);
        let llm = LlmManager::new();
        let outcome = swap_runner(&llm, &sample_request(), cfg);
        assert_eq!(outcome, SwapOutcome::Raw);
        assert!(
            engine_state.killed.load(Ordering::Acquire),
            "the never-finishing worker must be killed"
        );
        // The dictation context restores the voice model after teardown.
        // (restored is a host counter; reach it through a second run below.)
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// T27 companion: with the same never-finishing engine the skip event
    /// carries reason=timeout and the restore handoff still happens.
    #[test]
    fn total_deadline_emits_timeout_skip_and_restores() {
        let host = FakeHost::new();
        let host_skips = Arc::clone(&host.skips);
        let host_restored = Arc::clone(&host.restored);
        let mut engine = FakeEngine::happy(Ok(String::new()));
        engine.never_generate = true;
        let (mut cfg, _engine_state) = runner_cfg(host, engine);
        cfg.timing.total = Duration::from_millis(150);
        let llm = LlmManager::new();
        let outcome = swap_runner(&llm, &sample_request(), cfg);
        assert_eq!(outcome, SwapOutcome::Raw);
        let skips = host_skips.lock().unwrap();
        assert!(
            skips.iter().any(|(r, _)| *r == SkipReason::Timeout),
            "expected a timeout skip, got {:?}",
            *skips
        );
        assert_eq!(host_restored.load(Ordering::Acquire), 1);
    }

    /// T34: a voice unload that never returns still yields the bounded
    /// VoiceUnloadTimeout path; the LLM never loads; the abandoned helper
    /// thread holds no locks (the runner is free to finish).
    #[test]
    fn blocked_unload_hits_the_bounded_timeout() {
        let started = Instant::now();
        let host = FakeHost::new();
        host.block_unload.store(true, Ordering::Release);
        let host_started = Arc::clone(&host.unload_started);
        let host_skips = Arc::clone(&host.skips);
        let (mut cfg, engine_state) = runner_cfg(host, FakeEngine::happy(Ok(String::new())));
        cfg.timing.voice_unload = Duration::from_millis(80);
        let llm = LlmManager::new();
        let outcome = swap_runner(&llm, &sample_request(), cfg);
        assert_eq!(outcome, SwapOutcome::Raw);
        assert!(host_started.load(Ordering::Acquire));
        assert!(
            host_skips
                .lock()
                .unwrap()
                .iter()
                .any(|(r, _)| *r == SkipReason::EngineFailed),
            "the unload timeout must surface as engine_failed"
        );
        assert!(
            engine_state.generate_users.lock().unwrap().is_empty(),
            "the LLM must never be asked to generate when voice RAM is not freed"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// T15: dropping the caller's receiver at every phase cannot abandon
    /// the runner. For each phase, the detached runner still reaches a
    /// terminal state with the worker killed, the slot handed off or
    /// released, and the lease probe cleared.
    #[test]
    fn dropping_the_receiver_never_abandons_the_runner() {
        let llm = LlmManager::new();

        // Phase: gate refusal (terminal before any LLM work).
        {
            let mut host = FakeHost::new();
            host.gate_refuse.store(true, Ordering::Release);
            let (cfg, _engine_state) = runner_cfg(host, FakeEngine::happy(Ok(String::new())));
            let (rx, handle) = llm.spawn_runner(sample_request(), cfg);
            drop(rx);
            handle
                .expect("runner thread must spawn in tests")
                .join()
                .expect("runner must terminate");
            assert!(!llm.swap_in_progress(), "lease probe cleared");
        }

        // Phase: blocked voice unload (times out while the caller is gone).
        {
            let mut host = FakeHost::new();
            host.block_unload.store(true, Ordering::Release);
            let (mut cfg, _engine_state) = runner_cfg(host, FakeEngine::happy(Ok(String::new())));
            cfg.timing.voice_unload = Duration::from_millis(80);
            let (rx, handle) = llm.spawn_runner(sample_request(), cfg);
            drop(rx);
            handle
                .expect("runner thread must spawn in tests")
                .join()
                .expect("runner must terminate");
            assert!(!llm.swap_in_progress());
        }

        // Phase: never-finishing generation (total deadline while the
        // caller is gone).
        {
            let mut engine = FakeEngine::happy(Ok(String::new()));
            engine.never_generate = true;
            let (mut cfg, _engine_state) = runner_cfg(FakeHost::new(), engine);
            cfg.timing.total = Duration::from_millis(120);
            let (rx, handle) = llm.spawn_runner(sample_request(), cfg);
            drop(rx);
            handle
                .expect("runner thread must spawn in tests")
                .join()
                .expect("runner must terminate");
            assert!(!llm.swap_in_progress());
        }
    }

    /// T15 companion, at the observable level: after a dropped-receiver
    /// swap the slot is released or handed off and the outcome channel's
    /// death changed nothing.
    #[test]
    fn dropped_receiver_swap_leaves_the_slot_released() {
        let llm = LlmManager::new();
        let host = FakeHost::new();
        host.gate_refuse.store(true, Ordering::Release);
        let slot_flag = Arc::clone(&host.slot_flag);
        let (cfg, _engine_state) = runner_cfg(host, FakeEngine::happy(Ok(String::new())));
        let (rx, handle) = llm.spawn_runner(sample_request(), cfg);
        drop(rx);
        handle
            .expect("runner thread must spawn in tests")
            .join()
            .expect("runner must terminate");
        assert!(
            !*slot_flag.lock().unwrap(),
            "the loading slot must be released (guard dropped) after a gate-refused swap"
        );
        assert!(!llm.swap_in_progress());
    }

    /// T16: a runner panic triggers the drop belts. The engine's
    /// kill-on-drop killed the worker, the loading slot reads false
    /// afterwards, and the lease probe cleared (nothing stuck).
    #[test]
    fn runner_panic_fires_the_drop_belts() {
        let llm = LlmManager::new();
        let host = FakeHost::new();
        host.panic_at_restore.store(true, Ordering::Release);
        let slot_flag = Arc::clone(&host.slot_flag);
        let (cfg, engine_state) = runner_cfg(
            host,
            FakeEngine::happy(Ok("{\"transcription\": \"Clean.\"}".to_string())),
        );
        let (rx, handle) = llm.spawn_runner(sample_request(), cfg);
        // The receiver is dropped too: nothing must rescue the runner
        // except its own belts.
        drop(rx);
        // catch_unwind swallows the panic; the join is clean.
        handle
            .expect("runner thread must spawn in tests")
            .join()
            .expect("the wrapper catches the panic");
        assert!(
            engine_state.killed.load(Ordering::Acquire),
            "the engine's kill-on-drop must have fired during the unwind"
        );
        assert!(
            !*slot_flag.lock().unwrap(),
            "the slot guard must have dropped during the unwind"
        );
        assert!(!llm.swap_in_progress(), "the lease probe must clear");
    }

    /// The fidelity and format guards fold to raw outcomes with their skip
    /// reasons while the teardown stays identical (Processed only on a
    /// valid, faithful output).
    #[test]
    fn invalid_outputs_fold_to_raw_with_skip_events() {
        let llm = LlmManager::new();

        // Not JSON at all.
        let host = FakeHost::new();
        let skips = Arc::clone(&host.skips);
        let (cfg, _) = runner_cfg(host, FakeEngine::happy(Ok("the cleaned text".to_string())));
        assert_eq!(swap_runner(&llm, &sample_request(), cfg), SwapOutcome::Raw);
        assert!(skips
            .lock()
            .unwrap()
            .iter()
            .any(|(r, _)| *r == SkipReason::EngineFailed));

        // Valid JSON whose content collapsed (fidelity guard).
        let input = SwapRequest {
            transcript: "word ".repeat(40).trim_end().to_string(),
            system_prompt: String::new(),
            grammar: None,
            is_cancelled: None,
        };
        let host = FakeHost::new();
        let skips = Arc::clone(&host.skips);
        let (cfg, _) = runner_cfg(
            host,
            FakeEngine::happy(Ok("{\"transcription\": \"gone\"}".to_string())),
        );
        assert_eq!(swap_runner(&llm, &input, cfg), SwapOutcome::Raw);
        assert!(skips
            .lock()
            .unwrap()
            .iter()
            .any(|(r, _)| *r == SkipReason::LengthGuard));
    }

    /// The input cap skips the entire swap before anything is touched.
    #[test]
    fn over_cap_input_skips_without_touching_anything() {
        let llm = LlmManager::new();
        let host = FakeHost::new();
        let skips = Arc::clone(&host.skips);
        let unload_started = Arc::clone(&host.unload_started);
        let (cfg, engine_state) = runner_cfg(host, FakeEngine::happy(Ok(String::new())));
        let request = SwapRequest {
            transcript: "word".repeat(4801),
            system_prompt: String::new(),
            grammar: None,
            is_cancelled: None,
        };
        assert_eq!(swap_runner(&llm, &request, cfg), SwapOutcome::Raw);
        assert!(skips
            .lock()
            .unwrap()
            .iter()
            .any(|(r, _)| *r == SkipReason::TooLong));
        assert!(
            !unload_started.load(Ordering::Acquire),
            "the voice model is never touched"
        );
        assert!(engine_state.generate_users.lock().unwrap().is_empty());
    }

    /// Dictation wins: a pending press during generation aborts the swap,
    /// kills the worker, restores the voice model, and returns raw.
    #[test]
    fn pending_press_during_generation_aborts_and_restores() {
        let llm = LlmManager::new();
        let host = FakeHost::new();
        let pending = Arc::clone(&host.pending_press);
        let restored = Arc::clone(&host.restored);
        let mut engine = FakeEngine::happy(Ok("{\"transcription\": \"Clean.\"}".to_string()));
        engine.generate_delay = Duration::from_millis(150);
        let (cfg, engine_state) = runner_cfg(host, engine);
        // Arm the press shortly after the swap starts.
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(40));
            pending.store(true, Ordering::Release);
        });
        let outcome = swap_runner(&llm, &sample_request(), cfg);
        assert_eq!(outcome, SwapOutcome::Raw);
        assert!(engine_state.killed.load(Ordering::Acquire));
        assert_eq!(restored.load(Ordering::Acquire), 1);
    }

    /// A runner-thread spawn failure (memory exhaustion is the canonical
    /// hostile case) must clear the swap probe instead of wedging every
    /// later model delete on "post-processing is in progress", and must
    /// not panic the caller: the recovery helper is the whole contract.
    #[test]
    fn failed_runner_spawn_clears_the_probe_without_panicking() {
        let llm = LlmManager::new();
        // The probe is set BEFORE the spawn attempt; simulate the spawn
        // having failed.
        llm.swap_running.store(true, Ordering::Release);
        assert!(llm.swap_in_progress());

        let handle = llm.recover_from_failed_spawn(Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "test: no thread available",
        )));

        assert!(handle.is_none(), "no join handle exists for a failed spawn");
        assert!(
            !llm.swap_in_progress(),
            "the probe must clear so model deletes are not refused forever"
        );
    }

    /// A worker that was never spawned has nothing to wait for: WaitExit
    /// must observe it as exited instead of burning the full
    /// graceful+kill window and logging a false "worker ignored SIGKILL".
    #[test]
    fn unspawned_worker_counts_as_exited() {
        let mut engine = ProcessEngine::new();
        assert!(
            engine.has_exited(),
            "no child means nothing to wait for; false here burns the 15s teardown bound"
        );
    }

    /// A voice-model load that completes while this swap waits for the
    /// loading slot (the other load's guard holds it) must still be
    /// restored: the planner refreshes its restore context at the unload,
    /// over the model resident at THAT moment, not at runner start.
    #[test]
    fn voice_loaded_during_slot_wait_is_still_restored() {
        let llm = LlmManager::new();
        let host = FakeHost::new();
        // Nothing resident when the swap starts...
        host.voice_loaded.store(false, Ordering::Release);
        // ...and the slot is held by the other load, which completes (and
        // leaves a voice model resident) while this swap waits.
        host.slot_denied.store(true, Ordering::Release);
        let voice_loaded = Arc::clone(&host.voice_loaded);
        let slot_denied = Arc::clone(&host.slot_denied);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(25));
            voice_loaded.store(true, Ordering::Release);
            slot_denied.store(false, Ordering::Release);
        });
        let restored = Arc::clone(&host.restored);
        let (cfg, _engine_state) = runner_cfg(
            host,
            FakeEngine::happy(Ok("{\"transcription\": \"Clean.\"}".to_string())),
        );
        let outcome = swap_runner(&llm, &sample_request(), cfg);
        assert!(matches!(outcome, SwapOutcome::Processed(_)));
        assert_eq!(
            restored.load(Ordering::Acquire),
            1,
            "the freshly loaded voice model must be restored after the swap unloads it"
        );
    }
}
