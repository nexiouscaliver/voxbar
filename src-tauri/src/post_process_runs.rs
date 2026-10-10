//! The pp: observability lifecycle and the per-run registry.
//!
//! Every post-process run (cloud, Apple Intelligence, or the local engine)
//! answers to ONE lifecycle vocabulary: a `pp:` line per phase in voxbar.log,
//! a [`PostProcessRunRecord`] in an in-memory ring (the Debug tab's table and
//! the copy-log button read it), and a [`PostProcessRunEvent`] per phase so
//! the overlay chip and the main-window toasts can follow the run live. The
//! pattern is the successful `cmd-mode:` line family (transcription.rs): one
//! greppable single line, stable field order, snake_case tokens.
//!
//! Phases, in order, exactly one line each:
//!
//! 1. `requested` - binding, engine kind, provider, model, prompt, language
//! 2. `engine`    - model load ms / cache hit / swap wait (local engine; the
//!    cloud engines print `-` placeholders so both paths emit the same set)
//! 3. `generation`- ms and retries
//! 4. `outcome`   - applied | skipped(reason) | failed(class), chars in/out,
//!    changed ratio, total ms
//!
//! The hard rule lives here too: on ANY failure the raw transcript is what
//! gets pasted; the outcome line and the notice say why. This module records,
//! it never rewrites output.

use crate::llm_client::PostProcessFailureClass;
use crate::local_llm::SkipReason;
use log::{debug, error, info, warn};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;
use tauri::AppHandle;
use tauri_specta::Event as _;

/// The single failure-class vocabulary, re-exported so the observability
/// surface and the settings/Test Connection surface (llm_client) can never
/// drift apart. Wire values are stable tokens: auth, network, timeout,
/// context_length, output_invalid, oom, cancelled.
pub use crate::llm_client::PostProcessFailureClass as FailureClass;

/// How many recent runs the ring keeps. Old runs fall off the front; the
/// Debug tab reads the tail. 100 is a session's worth of polishing runs
/// without being a memory concern (records carry only bounded log lines).
pub const MAX_RUNS: usize = 100;

/// Which engine kind produced a run. Rides the `requested` line so a log
/// reader can tell the three paths apart before any other field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum PostProcessEngineKind {
    Cloud,
    Local,
    AppleIntelligence,
}

impl PostProcessEngineKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PostProcessEngineKind::Cloud => "cloud",
            PostProcessEngineKind::Local => "local",
            PostProcessEngineKind::AppleIntelligence => "apple_intelligence",
        }
    }
}

/// How one run ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PostProcessOutcome {
    /// The processed text was pasted.
    Applied,
    /// The engine never produced output (expected, recoverable); the raw
    /// transcript was used. Carries the local-engine skip vocabulary.
    Skipped { reason: SkipReason },
    /// The engine tried and failed; the raw transcript was used. Carries
    /// the failure class.
    Failed { class: PostProcessFailureClass },
}

impl PostProcessOutcome {
    /// The token written on the outcome line and into history
    /// (`applied` | `skipped:<reason>` | `failed:<class>`), one greppable
    /// word either way.
    pub fn token(&self) -> String {
        match self {
            PostProcessOutcome::Applied => "applied".to_string(),
            PostProcessOutcome::Skipped { reason } => {
                format!(
                    "skipped:{}",
                    crate::local_llm::manager::skip_reason_str(*reason)
                )
            }
            PostProcessOutcome::Failed { class } => {
                format!("failed:{}", failure_class_str(*class))
            }
        }
    }
}

/// The snake_case token for a failure class, shared by the outcome line,
/// the history column, and the record (the serde wire value is the same
/// token, so log lines, DB rows, and event payloads always agree).
pub fn failure_class_str(class: PostProcessFailureClass) -> &'static str {
    match class {
        PostProcessFailureClass::Auth => "auth",
        PostProcessFailureClass::Network => "network",
        PostProcessFailureClass::Timeout => "timeout",
        PostProcessFailureClass::ContextLength => "context_length",
        PostProcessFailureClass::OutputInvalid => "output_invalid",
        PostProcessFailureClass::Oom => "oom",
        PostProcessFailureClass::Cancelled => "cancelled",
    }
}

/// The lifecycle phases, in order. Exactly one `pp:` line and one
/// [`PostProcessRunEvent`] per phase per run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum PostProcessRunPhase {
    Requested,
    Engine,
    Generation,
    Outcome,
}

impl PostProcessRunPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            PostProcessRunPhase::Requested => "requested",
            PostProcessRunPhase::Engine => "engine",
            PostProcessRunPhase::Generation => "generation",
            PostProcessRunPhase::Outcome => "outcome",
        }
    }
}

/// The engine phase facts (local engine): how long the model load took,
/// whether the model was already resident (it never is today - the swap
/// spawns a fresh worker - but the field is the honest place for a future
/// keep-alive), and how long the run waited on the swap machinery (lease,
/// slot, gate, voice unload) before the load began. Cloud runs carry None
/// for all three.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct PostProcessEnginePhase {
    pub model_load_ms: Option<u64>,
    pub cache_hit: Option<bool>,
    pub swap_wait_ms: Option<u64>,
}

/// The generation phase facts: wall-clock ms and retry count (a structured
/// attempt that fell back to the legacy prompt shape counts as one retry).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct PostProcessGenerationPhase {
    pub ms: Option<u64>,
    pub retries: Option<u32>,
}

/// One completed (or in-flight) post-process run. Everything the Debug
/// table shows and everything the copy button returns lives here.
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct PostProcessRunRecord {
    /// Monotonic per-app-launch id, starting at 1.
    pub run_id: u64,
    /// Unix epoch milliseconds (UTC) when the requested phase fired.
    pub started_at_unix_ms: i64,
    pub binding: String,
    pub engine: PostProcessEngineKind,
    pub provider_id: String,
    pub model: String,
    pub prompt_id: Option<String>,
    pub prompt_name: Option<String>,
    /// The prompt template's version token, when the prompt carries one.
    pub prompt_version: Option<String>,
    /// The template's target language (the per-language prompt library),
    /// when the prompt carries one.
    pub template_language: Option<String>,
    pub phases_engine: PostProcessEnginePhase,
    pub phases_generation: PostProcessGenerationPhase,
    /// `None` while the run is still live.
    pub outcome: Option<PostProcessOutcome>,
    pub chars_in: Option<u64>,
    pub chars_out: Option<u64>,
    /// chars_out / chars_in (how much of the transcript survived), None
    /// when chars_in is 0 or the run produced no output.
    pub changed_ratio: Option<f64>,
    pub total_ms: Option<u64>,
    /// The run's own captured `pp:` lines, in order. This is exactly what
    /// the Debug table's copy button puts on the clipboard.
    pub log_lines: Vec<String>,
}

/// Everything a run needs at its `requested` phase.
#[derive(Clone, Debug)]
pub struct RunRequestMeta {
    pub binding: String,
    pub engine: PostProcessEngineKind,
    pub provider_id: String,
    pub model: String,
    pub prompt_id: Option<String>,
    pub prompt_name: Option<String>,
    pub prompt_version: Option<String>,
    pub template_language: Option<String>,
    pub chars_in: u64,
}

/// The per-phase event payload. Only the fields belonging to the event's
/// phase are populated; the rest stay None.
#[derive(Clone, Debug, Default, Serialize, Deserialize, Type)]
pub struct PostProcessRunPayload {
    pub binding: Option<String>,
    pub engine: Option<PostProcessEngineKind>,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    pub prompt_name: Option<String>,
    pub model_load_ms: Option<u64>,
    pub cache_hit: Option<bool>,
    pub swap_wait_ms: Option<u64>,
    pub generation_ms: Option<u64>,
    pub retries: Option<u32>,
    pub outcome: Option<PostProcessOutcome>,
    pub chars_in: Option<u64>,
    pub chars_out: Option<u64>,
    pub total_ms: Option<u64>,
}

/// Emitted once per lifecycle phase. The overlay window drives its
/// "Polishing (model) 1.8s" chip off Requested/Outcome; the main window
/// toasts failure classes off Outcome.
#[derive(Clone, Debug, Serialize, Deserialize, Type, tauri_specta::Event)]
pub struct PostProcessRunEvent {
    pub run_id: u64,
    pub phase: PostProcessRunPhase,
    pub payload: PostProcessRunPayload,
}

/// The process-wide run registry: a ring of the last [`MAX_RUNS`] records,
/// the monotonic id counter, and each live run's start instant (kept out of
/// the serializable record).
pub struct RunsRegistry {
    runs: Mutex<VecDeque<PostProcessRunRecord>>,
    starts: Mutex<std::collections::HashMap<u64, Instant>>,
    next_run_id: AtomicU64,
}

static RUNS: Lazy<RunsRegistry> = Lazy::new(RunsRegistry::new);

/// The process-wide registry.
pub fn runs() -> &'static RunsRegistry {
    &RUNS
}

fn opt_num<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map(|x| x.to_string()).unwrap_or_else(|| "-".to_string())
}

impl RunsRegistry {
    pub fn new() -> Self {
        Self {
            runs: Mutex::new(VecDeque::with_capacity(MAX_RUNS)),
            starts: Mutex::new(std::collections::HashMap::new()),
            next_run_id: AtomicU64::new(1),
        }
    }

    /// The `requested` phase: mint the run id, push the record, write the
    /// line, emit the event. Returns the run id every later phase uses.
    pub fn begin(&self, app: Option<&AppHandle>, meta: RunRequestMeta) -> u64 {
        let run_id = self.next_run_id.fetch_add(1, Ordering::Relaxed);
        let record = PostProcessRunRecord {
            run_id,
            started_at_unix_ms: chrono::Utc::now().timestamp_millis(),
            binding: meta.binding.clone(),
            engine: meta.engine,
            provider_id: meta.provider_id.clone(),
            model: meta.model.clone(),
            prompt_id: meta.prompt_id.clone(),
            prompt_name: meta.prompt_name.clone(),
            prompt_version: meta.prompt_version.clone(),
            template_language: meta.template_language.clone(),
            phases_engine: PostProcessEnginePhase::default(),
            phases_generation: PostProcessGenerationPhase::default(),
            outcome: None,
            chars_in: Some(meta.chars_in),
            chars_out: None,
            changed_ratio: None,
            total_ms: None,
            log_lines: Vec::new(),
        };
        if let Ok(mut starts) = self.starts.lock() {
            starts.insert(run_id, Instant::now());
        }
        if let Ok(mut runs) = self.runs.lock() {
            if runs.len() == MAX_RUNS {
                runs.pop_front();
            }
            runs.push_back(record);
        }

        let line = format!(
            "pp: run={} phase=requested binding={} engine={} provider={} model={} prompt={} prompt_version={} lang={} chars_in={}",
            run_id,
            meta.binding,
            meta.engine.as_str(),
            meta.provider_id,
            meta.model,
            meta.prompt_id.as_deref().unwrap_or("-"),
            meta.prompt_version.as_deref().unwrap_or("-"),
            meta.template_language.as_deref().unwrap_or("-"),
            meta.chars_in,
        );
        pp_log(run_id, log::Level::Info, line);
        let _ = app.map(|app| {
            let _ = PostProcessRunEvent {
                run_id,
                phase: PostProcessRunPhase::Requested,
                payload: PostProcessRunPayload {
                    binding: Some(meta.binding),
                    engine: Some(meta.engine),
                    provider_id: Some(meta.provider_id),
                    model: Some(meta.model),
                    prompt_name: meta.prompt_name,
                    ..Default::default()
                },
            }
            .emit(app);
        });
        run_id
    }

    /// The `engine` phase. `swap_wait_ms` is derived from the run's start
    /// instant when a model load was measured (local runs); cloud runs pass
    /// None for both and print placeholders.
    pub fn engine_phase(
        &self,
        app: Option<&AppHandle>,
        run_id: u64,
        model_load_ms: Option<u64>,
        cache_hit: Option<bool>,
    ) {
        // The swap wait is everything between the requested phase and the
        // load completing that was NOT the load itself (lease + slot +
        // gate + voice unload on the local engine).
        let swap_wait_ms = model_load_ms.and_then(|load_ms| {
            self.starts
                .lock()
                .ok()
                .and_then(|starts| starts.get(&run_id).copied())
                .map(|started| started.elapsed().as_millis() as u64)
                .map(|total| total.saturating_sub(load_ms))
        });
        let phase = PostProcessEnginePhase {
            model_load_ms,
            cache_hit,
            swap_wait_ms,
        };
        if let Ok(mut runs) = self.runs.lock() {
            if let Some(record) = runs.iter_mut().find(|r| r.run_id == run_id) {
                record.phases_engine = phase;
            }
        }

        let line = format!(
            "pp: run={} phase=engine model_load_ms={} cache_hit={} swap_wait_ms={}",
            run_id,
            opt_num(phase.model_load_ms),
            phase
                .cache_hit
                .map(|b| if b { "true" } else { "false" })
                .unwrap_or("-"),
            opt_num(phase.swap_wait_ms),
        );
        pp_log(run_id, log::Level::Info, line);
        let _ = app.map(|app| {
            let _ = PostProcessRunEvent {
                run_id,
                phase: PostProcessRunPhase::Engine,
                payload: PostProcessRunPayload {
                    model_load_ms,
                    cache_hit,
                    swap_wait_ms,
                    ..Default::default()
                },
            }
            .emit(app);
        });
    }

    /// The `generation` phase.
    pub fn generation_phase(
        &self,
        app: Option<&AppHandle>,
        run_id: u64,
        generation_ms: Option<u64>,
        retries: Option<u32>,
    ) {
        let phase = PostProcessGenerationPhase {
            ms: generation_ms,
            retries,
        };
        if let Ok(mut runs) = self.runs.lock() {
            if let Some(record) = runs.iter_mut().find(|r| r.run_id == run_id) {
                record.phases_generation = phase;
            }
        }

        let line = format!(
            "pp: run={} phase=generation ms={} retries={}",
            run_id,
            opt_num(phase.ms),
            opt_num(phase.retries),
        );
        pp_log(run_id, log::Level::Info, line);
        let _ = app.map(|app| {
            let _ = PostProcessRunEvent {
                run_id,
                phase: PostProcessRunPhase::Generation,
                payload: PostProcessRunPayload {
                    generation_ms,
                    retries,
                    ..Default::default()
                },
            }
            .emit(app);
        });
    }

    /// The `outcome` phase. First writer wins: skips conclude runs from
    /// inside the engine (the local skip sink), and a second finish call
    /// (the runner's terminal arm) is a logged no-op, so exactly one
    /// outcome line exists per run.
    pub fn finish(
        &self,
        app: Option<&AppHandle>,
        run_id: u64,
        outcome: PostProcessOutcome,
        chars_out: Option<u64>,
    ) {
        self.finish_with_detail(app, run_id, outcome, chars_out, None);
    }

    /// [`Self::finish`] with a diagnostic detail riding the outcome line
    /// (the skip reasons' explanations, the cloud failure's error text).
    pub fn finish_with_detail(
        &self,
        app: Option<&AppHandle>,
        run_id: u64,
        outcome: PostProcessOutcome,
        chars_out: Option<u64>,
        detail: Option<&str>,
    ) {
        let mut already_finished = false;
        if let Ok(mut runs) = self.runs.lock() {
            let Some(record) = runs.iter_mut().find(|r| r.run_id == run_id) else {
                // The run fell off the ring (or was never begun); the line
                // still lands in the file log so the session stays
                // diagnosable.
                return;
            };
            if record.outcome.is_some() {
                already_finished = true;
            } else {
                record.outcome = Some(outcome.clone());
                record.chars_out = chars_out;
                record.changed_ratio = match (record.chars_in, chars_out) {
                    (Some(chars_in), Some(chars_out)) if chars_in > 0 => {
                        Some(chars_out as f64 / chars_in as f64)
                    }
                    _ => None,
                };
                if let Ok(starts) = self.starts.lock() {
                    if let Some(started) = starts.get(&run_id) {
                        record.total_ms = Some(started.elapsed().as_millis() as u64);
                    }
                }
            }
        }
        if already_finished {
            debug!(
                "pp: run={} outcome already recorded; the later terminal report is a no-op",
                run_id
            );
            return;
        }
        if let Ok(mut starts) = self.starts.lock() {
            starts.remove(&run_id);
        }

        let (chars_in, chars_out_v, total_ms, provider_id, model) = {
            let Ok(runs) = self.runs.lock() else {
                return;
            };
            let Some(record) = runs.iter().find(|r| r.run_id == run_id) else {
                return;
            };
            (
                record.chars_in,
                record.chars_out,
                record.total_ms,
                record.provider_id.clone(),
                record.model.clone(),
            )
        };
        let line = format!(
            "pp: run={} phase=outcome outcome={} chars_in={} chars_out={} changed={} total_ms={} detail={}",
            run_id,
            outcome.token(),
            opt_num(chars_in),
            opt_num(chars_out_v),
            match (chars_in, chars_out_v) {
                (Some(ci), Some(co)) if ci > 0 => format!("{:.2}", co as f64 / ci as f64),
                _ => "-".to_string(),
            },
            opt_num(total_ms),
            detail.unwrap_or("-"),
        );
        pp_log(run_id, outcome_log_level(&outcome), line);
        let _ = app.map(|app| {
            let _ = PostProcessRunEvent {
                run_id,
                phase: PostProcessRunPhase::Outcome,
                payload: PostProcessRunPayload {
                    outcome: Some(outcome),
                    chars_in,
                    chars_out: chars_out_v,
                    total_ms,
                    // Repeated from the requested phase so an outcome-only
                    // listener can name the engine in its toast detail.
                    provider_id: Some(provider_id),
                    model: Some(model),
                    ..Default::default()
                },
            }
            .emit(app);
        });
    }

    /// Conclude every still-live dictation run as `failed(cancelled)`.
    ///
    /// The one path that needs this: a user cancel drops the stop
    /// pipeline's post-process future at an await point, and a cloud
    /// request has no detached runner to conclude the record afterwards -
    /// without this, the run would sit "live" in the Debug table forever.
    /// Local runs self-conclude (their runner is detached and
    /// cancel-aware); first-writer-wins makes the double finish harmless.
    /// History retries (`history_retry` binding) are never touched: they
    /// run outside the dictation cancel generation.
    pub fn cancel_live_dictation_runs(&self) {
        let live: Vec<u64> = match self.runs.lock() {
            Ok(runs) => runs
                .iter()
                .filter(|r| r.outcome.is_none() && r.binding != "history_retry")
                .map(|r| r.run_id)
                .collect(),
            Err(_) => return,
        };
        for run_id in live {
            self.finish_with_detail(
                None,
                run_id,
                PostProcessOutcome::Failed {
                    class: PostProcessFailureClass::Cancelled,
                },
                None,
                Some("cancelled with the dictation"),
            );
        }
    }

    /// One run's record, if the ring still holds it.
    pub fn snapshot(&self, run_id: u64) -> Option<PostProcessRunRecord> {
        self.runs
            .lock()
            .ok()
            .and_then(|runs| runs.iter().find(|r| r.run_id == run_id).cloned())
    }

    /// The most recent `limit` runs, newest first (the Debug table's order).
    pub fn latest(&self, limit: Option<usize>) -> Vec<PostProcessRunRecord> {
        self.runs
            .lock()
            .ok()
            .map(|runs| {
                let take = limit.unwrap_or(MAX_RUNS).min(MAX_RUNS);
                runs.iter().rev().take(take).cloned().collect::<Vec<_>>()
            })
            .unwrap_or_default()
    }

    /// Append a raw line to a run's record (diagnostic lines beyond the
    /// four phase lines, if a path ever needs one). Does not log.
    pub fn append_line(&self, run_id: u64, line: String) {
        if let Ok(mut runs) = self.runs.lock() {
            if let Some(record) = runs.iter_mut().find(|r| r.run_id == run_id) {
                record.log_lines.push(line);
            }
        }
    }
}

/// Write one greppable `pp:` line at the given severity AND append it to
/// the run's record. This is the single helper every phase line goes
/// through, so the file log and the copyable log slice can never diverge.
pub(crate) fn pp_log(run_id: u64, level: log::Level, line: String) {
    match level {
        log::Level::Error => error!("{line}"),
        log::Level::Warn => warn!("{line}"),
        log::Level::Info => info!("{line}"),
        _ => debug!("{line}"),
    }
    runs().append_line(run_id, line);
}

/// The log severity of an outcome line: applied is routine info; skips
/// keep the local sink's severity table (expected/recoverable at info,
/// something-broke at warn); failures always warn.
pub(crate) fn outcome_log_level(outcome: &PostProcessOutcome) -> log::Level {
    match outcome {
        PostProcessOutcome::Applied => log::Level::Info,
        PostProcessOutcome::Skipped { reason } => {
            crate::local_llm::manager::skip_log_level(*reason)
        }
        PostProcessOutcome::Failed { .. } => log::Level::Warn,
    }
}

/// Classify a cloud (llm_client) error detail into the failure class. The
/// detail strings come from `send_chat_completion_with_schema` (HTTP status
/// lines, sanitized transport diagnostics); the mapping is deliberately
/// conservative: only markers that name a class map, everything unmatched
/// is a network failure (the endpoint was unreachable or misbehaved).
pub(crate) fn classify_cloud_failure(detail: &str) -> PostProcessFailureClass {
    let lower = detail.to_lowercase();
    if lower.contains("status 401") || lower.contains("status 403") {
        return PostProcessFailureClass::Auth;
    }
    if lower.contains("status 400")
        && (lower.contains("context length")
            || lower.contains("context window")
            || lower.contains("maximum context")
            || lower.contains("too many tokens"))
    {
        return PostProcessFailureClass::ContextLength;
    }
    if lower.contains("failed to parse") || lower.contains("could not parse") {
        return PostProcessFailureClass::OutputInvalid;
    }
    if lower.contains("timeout")
        || lower.contains("timed out")
        || lower.contains("status 408")
        || lower.contains("status 504")
    {
        return PostProcessFailureClass::Timeout;
    }
    PostProcessFailureClass::Network
}

/// The per-run summary history persists (the two save_entry call sites).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostProcessRunSummary {
    pub provider_id: String,
    pub model: String,
    pub prompt_id: String,
    /// `applied` | `skipped:<reason>` | `failed:<class>`
    pub outcome: String,
    pub latency_ms: u64,
}

/// The Tauri command the Debug tab's table calls. Returns the most recent
/// runs, newest first.
#[tauri::command]
#[specta::specta]
pub async fn get_post_process_runs(
    limit: Option<usize>,
) -> Result<Vec<PostProcessRunRecord>, String> {
    Ok(runs().latest(limit.map(|l| l.min(MAX_RUNS))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(binding: &str, engine: PostProcessEngineKind, model: &str) -> RunRequestMeta {
        RunRequestMeta {
            binding: binding.to_string(),
            engine,
            provider_id: if engine == PostProcessEngineKind::Local {
                "local".to_string()
            } else {
                "openai".to_string()
            },
            model: model.to_string(),
            prompt_id: Some("prompt-1".to_string()),
            prompt_name: Some("Clean up".to_string()),
            prompt_version: None,
            template_language: Some("en".to_string()),
            chars_in: 120,
        }
    }

    /// Every lifecycle phase emits exactly one pp: line carrying the run_id,
    /// and the four lines name the four phases in order.
    #[test]
    fn full_run_emits_one_line_per_phase() {
        let id = runs().begin(
            None,
            meta(
                "transcribe_with_post_process",
                PostProcessEngineKind::Local,
                "qwen",
            ),
        );
        runs().engine_phase(None, id, Some(812), Some(false));
        runs().generation_phase(None, id, Some(940), Some(0));
        runs().finish(None, id, PostProcessOutcome::Applied, Some(104));

        let rec = runs().snapshot(id).expect("record kept");
        assert_eq!(rec.log_lines.len(), 4, "exactly one line per phase");
        for (i, phase) in [
            PostProcessRunPhase::Requested,
            PostProcessRunPhase::Engine,
            PostProcessRunPhase::Generation,
            PostProcessRunPhase::Outcome,
        ]
        .iter()
        .enumerate()
        {
            let line = &rec.log_lines[i];
            assert!(
                line.starts_with(&format!("pp: run={} phase={}", id, phase.as_str())),
                "line {i}: {line}"
            );
            assert_eq!(
                rec.log_lines
                    .iter()
                    .filter(|l| l.contains(&format!("phase={}", phase.as_str())))
                    .count(),
                1,
                "phase {} appears exactly once",
                phase.as_str()
            );
        }
        assert_eq!(rec.outcome, Some(PostProcessOutcome::Applied));
        assert_eq!(rec.chars_in, Some(120));
        assert_eq!(rec.chars_out, Some(104));
        let ratio = rec.changed_ratio.expect("ratio computed");
        assert!((ratio - 104.0 / 120.0).abs() < 1e-9, "ratio: {ratio}");
    }

    /// The ring caps at MAX_RUNS: the oldest runs fall off, ids stay
    /// monotonic, and latest() returns newest-first.
    #[test]
    fn ring_caps_at_max_runs() {
        let first = runs().begin(None, meta("b", PostProcessEngineKind::Cloud, "m"));
        for _ in 0..(MAX_RUNS + 2) {
            runs().begin(None, meta("b", PostProcessEngineKind::Cloud, "m"));
        }
        let all = runs().latest(None);
        assert_eq!(all.len(), MAX_RUNS, "ring holds at most MAX_RUNS");
        // Newest first.
        assert!(all[0].run_id > all[all.len() - 1].run_id);
        // The very first run fell off the front.
        assert!(runs().snapshot(first).is_none(), "oldest run evicted");
        let ids: Vec<u64> = all.iter().map(|r| r.run_id).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted.into_iter().rev().collect::<Vec<_>>());
    }

    /// Each record holds its own captured lines: two interleaved runs never
    /// cross-contaminate (this is what the copy button returns).
    #[test]
    fn records_hold_their_own_lines() {
        let a = runs().begin(None, meta("ba", PostProcessEngineKind::Cloud, "ma"));
        let b = runs().begin(None, meta("bb", PostProcessEngineKind::Cloud, "mb"));
        runs().engine_phase(None, a, None, None);
        runs().generation_phase(None, b, Some(10), Some(0));
        runs().finish(
            None,
            a,
            PostProcessOutcome::Failed {
                class: PostProcessFailureClass::Auth,
            },
            None,
        );
        runs().finish(
            None,
            b,
            PostProcessOutcome::Skipped {
                reason: SkipReason::TooLong,
            },
            None,
        );

        let ra = runs().snapshot(a).unwrap();
        let rb = runs().snapshot(b).unwrap();
        assert!(ra.log_lines.iter().all(|l| l.contains(&format!("run={a}"))));
        assert!(rb.log_lines.iter().all(|l| l.contains(&format!("run={b}"))));
        assert_eq!(ra.model, "ma");
        assert_eq!(rb.model, "mb");
        // Cloud engine phase prints placeholders, not numbers.
        let engine_line = ra.log_lines[1].clone();
        assert!(engine_line.contains("model_load_ms=-"), "{engine_line}");
        assert!(engine_line.contains("cache_hit=-"), "{engine_line}");
        assert!(engine_line.contains("swap_wait_ms=-"), "{engine_line}");
    }

    /// First finisher wins: a skip that concludes the run inside the engine
    /// is the one outcome; the runner's later terminal finish is a no-op
    /// that adds no second outcome line.
    #[test]
    fn finish_is_first_writer_wins() {
        let id = runs().begin(None, meta("b", PostProcessEngineKind::Local, "m"));
        runs().finish(
            None,
            id,
            PostProcessOutcome::Skipped {
                reason: SkipReason::MemoryGate,
            },
            None,
        );
        runs().finish(None, id, PostProcessOutcome::Applied, Some(10));
        let rec = runs().snapshot(id).unwrap();
        assert_eq!(
            rec.outcome,
            Some(PostProcessOutcome::Skipped {
                reason: SkipReason::MemoryGate
            })
        );
        assert_eq!(
            rec.log_lines
                .iter()
                .filter(|l| l.contains("phase=outcome"))
                .count(),
            1,
            "exactly one outcome line even after a second finish"
        );
    }

    /// The outcome tokens that ride the line and the history column.
    #[test]
    fn outcome_tokens() {
        assert_eq!(PostProcessOutcome::Applied.token(), "applied");
        assert_eq!(
            PostProcessOutcome::Skipped {
                reason: SkipReason::DownloadMissing
            }
            .token(),
            "skipped:download_missing"
        );
        assert_eq!(
            PostProcessOutcome::Failed {
                class: PostProcessFailureClass::ContextLength
            }
            .token(),
            "failed:context_length"
        );
    }

    /// Cloud error details classify onto the failure-class vocabulary: a
    /// 401 is auth, timeouts are timeouts, context-window rejections are
    /// context_length, parse failures are output_invalid, and an unknown
    /// transport failure is network.
    #[test]
    fn cloud_failures_classify() {
        assert_eq!(
            classify_cloud_failure(
                "API request failed with status 401: {\"error\":{\"code\":401}}"
            ),
            PostProcessFailureClass::Auth
        );
        assert_eq!(
            classify_cloud_failure("API request failed with status 403: forbidden"),
            PostProcessFailureClass::Auth
        );
        assert_eq!(
            classify_cloud_failure(
                "HTTP request failed (kind: connect, timeout, url: https://api/x)"
            ),
            PostProcessFailureClass::Timeout
        );
        assert_eq!(
            classify_cloud_failure("API request failed with status 400: This model's maximum context length is 4096 tokens"),
            PostProcessFailureClass::ContextLength
        );
        assert_eq!(
            classify_cloud_failure("Failed to parse API response (kind: decode)"),
            PostProcessFailureClass::OutputInvalid
        );
        assert_eq!(
            classify_cloud_failure("HTTP request failed (kind: connect)"),
            PostProcessFailureClass::Network
        );
    }

    /// The local engine's swap_wait rides the engine line (measured from
    /// the requested phase, minus the load itself).
    #[test]
    fn local_engine_phase_carries_swap_wait() {
        let id = runs().begin(None, meta("b", PostProcessEngineKind::Local, "m"));
        runs().engine_phase(None, id, Some(500), Some(false));
        let rec = runs().snapshot(id).unwrap();
        assert_eq!(rec.phases_engine.model_load_ms, Some(500));
        assert_eq!(rec.phases_engine.cache_hit, Some(false));
        // The wait is near-zero in a test (measured across real instants),
        // but it must be present and small rather than absent.
        let wait = rec.phases_engine.swap_wait_ms.expect("swap wait measured");
        assert!(wait < 5_000, "wait: {wait}");
    }

    /// A user cancel closes every live dictation run as failed(cancelled)
    /// (a dropped cloud future has no detached runner to conclude it),
    /// while history-retry runs are left alone to conclude themselves.
    #[test]
    fn cancel_closes_live_dictation_runs_but_not_retries() {
        let dictation = runs().begin(
            None,
            meta(
                "transcribe_with_post_process",
                PostProcessEngineKind::Cloud,
                "m",
            ),
        );
        let retry = runs().begin(
            None,
            meta("history_retry", PostProcessEngineKind::Cloud, "m"),
        );
        runs().cancel_live_dictation_runs();
        assert_eq!(
            runs().snapshot(dictation).unwrap().outcome,
            Some(PostProcessOutcome::Failed {
                class: PostProcessFailureClass::Cancelled
            })
        );
        assert_eq!(runs().snapshot(retry).unwrap().outcome, None);
    }
}
