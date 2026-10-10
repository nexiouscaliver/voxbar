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

    /// A registry whose run ids can never collide with the process-wide
    /// singleton's: `pp_log` appends lines to the SINGLETON by run id, so
    /// a private registry minting ids from 1 would cross-contaminate the
    /// parallel tests' records. Tests that exercise registry semantics in
    /// isolation (the cancel sweep) use this instead.
    #[cfg(test)]
    pub(crate) fn isolated_for_test() -> Self {
        static INSTANCE: AtomicU64 = AtomicU64::new(0);
        let instance = INSTANCE.fetch_add(1, Ordering::Relaxed);
        Self {
            runs: Mutex::new(VecDeque::with_capacity(MAX_RUNS)),
            starts: Mutex::new(std::collections::HashMap::new()),
            next_run_id: AtomicU64::new(1_000_000_000 + instance * 1_000_000),
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

// ---- THE shared output validator (both engines) ----

// (Cloud failure classification moved into llm_client as the structured
// PostProcessError when send_chat_completion_with_schema stopped returning
// bare Strings; the pp: outcome layer now reads the class off the error.)

/// Field name for structured output JSON schema. Shared by the cloud
/// structured mode and the local engine, whose worker output is parsed
/// against the same schema.
pub(crate) const TRANSCRIPTION_FIELD: &str = "transcription";

/// Strip invisible Unicode characters that some LLMs may insert.
pub(crate) fn strip_invisible_chars(s: &str) -> String {
    s.replace(['\u{200B}', '\u{200C}', '\u{200D}', '\u{FEFF}'], "")
}

/// Strip a leading `<think>...</think>` block. Some endpoints can't disable
/// reasoning, and some local servers put the reasoning text into `content`
/// instead of a separate field - without this the user would get the model's
/// chain of thought pasted along with the cleaned transcription.
pub(crate) fn strip_think_block(s: &str) -> &str {
    if let Some(rest) = s.trim_start().strip_prefix("<think>") {
        if let Some(end) = rest.find("</think>") {
            return rest[end + "</think>".len()..].trim_start();
        }
    }
    s
}

/// Which shape a post-process engine's raw output has, for the shared
/// validator: structured engines answer JSON against the schema; free-text
/// engines (the legacy prompt shape, Apple Intelligence) answer prose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PostProcessOutputMode {
    StructuredJson,
    FreeText,
}

/// Why the shared validator rejected an engine's output. Carries the
/// local-engine skip vocabulary (the local path's existing toast UX keys
/// off it) plus a human diagnostic; every failure classifies as
/// output_invalid at the outcome layer (the WS3 rule: fall back to the raw
/// transcript).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PostProcessValidationFailure {
    pub skip_reason: crate::local_llm::SkipReason,
    pub detail: String,
}

fn validation_failure(
    skip_reason: crate::local_llm::SkipReason,
    detail: impl Into<String>,
) -> PostProcessValidationFailure {
    PostProcessValidationFailure {
        skip_reason,
        detail: detail.into(),
    }
}

/// The scripts the language-sanity check can tell apart. Coarse on
/// purpose: it exists to catch a model that ANSWERED in the wrong language
/// (or translated against the template's keep-language rule), not to
/// police mixed-script text. Kana and Hangul are distinct from Han so a
/// zh transcript answered in Japanese or Korean still trips.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum TextScript {
    Latin,
    Cyrillic,
    Greek,
    Arabic,
    Hebrew,
    Devanagari,
    Bengali,
    Thai,
    Han,
    Kana,
    Hangul,
}

impl TextScript {
    pub fn name(self) -> &'static str {
        match self {
            TextScript::Latin => "latin",
            TextScript::Cyrillic => "cyrillic",
            TextScript::Greek => "greek",
            TextScript::Arabic => "arabic",
            TextScript::Hebrew => "hebrew",
            TextScript::Devanagari => "devanagari",
            TextScript::Bengali => "bengali",
            TextScript::Thai => "thai",
            TextScript::Han => "han",
            TextScript::Kana => "kana",
            TextScript::Hangul => "hangul",
        }
    }

    fn from_char(c: char) -> Option<TextScript> {
        let cp = c as u32;
        Some(match cp {
            // ASCII letters + Latin-1 supplement + Latin extended A/B.
            0x41..=0x5A | 0x61..=0x7A | 0xC0..=0x24F => TextScript::Latin,
            0x370..=0x3FF | 0x1F00..=0x1FFF => TextScript::Greek,
            0x400..=0x52F => TextScript::Cyrillic,
            0x590..=0x5FF => TextScript::Hebrew,
            0x600..=0x6FF | 0xFB50..=0xFDFF | 0xFE70..=0xFEFF => TextScript::Arabic,
            0x900..=0x97F => TextScript::Devanagari,
            0x980..=0x9FF => TextScript::Bengali,
            0xE00..=0xE7F => TextScript::Thai,
            // Hiragana + katakana (plus kana supplement block tail).
            0x3040..=0x30FF | 0x31F0..=0x31FF => TextScript::Kana,
            // CJK ideographs + extension A + compatibility ideographs.
            0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF => TextScript::Han,
            0xAC00..=0xD7AF | 0x1100..=0x11FF => TextScript::Hangul,
            _ => return None,
        })
    }
}

/// A text's dominant script: the most frequent script-classified
/// character, when at least [`SCRIPT_SIGNAL_MIN`] characters carry a
/// script and one script strictly leads. Digits, punctuation, whitespace,
/// and weak signals read as None (the check is skipped, never guessed).
pub(crate) const SCRIPT_SIGNAL_MIN: usize = 3;

pub(crate) fn dominant_script(text: &str) -> Option<TextScript> {
    let mut counts = std::collections::BTreeMap::<TextScript, usize>::new();
    let mut total = 0usize;
    for script in text.chars().filter_map(TextScript::from_char) {
        *counts.entry(script).or_default() += 1;
        total += 1;
    }
    if total < SCRIPT_SIGNAL_MIN {
        return None;
    }
    let mut ranked: Vec<(TextScript, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1));
    match ranked.as_slice() {
        // A strict leader only: a tie is genuinely mixed text, and mixed
        // text must never fail a script check.
        [(first, first_n), (second, second_n), ..] if first_n > second_n => Some(*first),
        [(only, _)] => Some(*only),
        _ => None,
    }
}

/// The script a template's language tag DECLARES for its output, from the
/// tag's ISO 15924 subtag (hi-Latn declares Latin). `auto`, bare language
/// tags, and unknown script subtags declare nothing.
pub(crate) fn declared_script(template_language: &str) -> Option<TextScript> {
    let declared = template_language
        .split('-')
        .find(|part| part.len() == 4 && part.chars().all(|c| c.is_ascii_alphabetic()))?;
    match declared.to_ascii_lowercase().as_str() {
        "latn" => Some(TextScript::Latin),
        "cyrl" => Some(TextScript::Cyrillic),
        "grek" => Some(TextScript::Greek),
        "arab" => Some(TextScript::Arabic),
        "hebr" => Some(TextScript::Hebrew),
        "deva" => Some(TextScript::Devanagari),
        "beng" => Some(TextScript::Bengali),
        "thai" => Some(TextScript::Thai),
        "hani" | "hans" | "hant" => Some(TextScript::Han),
        "hrkt" | "hira" | "kana" => Some(TextScript::Kana),
        "hang" => Some(TextScript::Hangul),
        _ => None,
    }
}

/// The language-sanity rule: the output's dominant script must match the
/// transcript's unless the template's language tag sanctions the change
/// (hi-Latn asks for Latin; a Serbian cyrillic->latin template declares
/// Cyrl->Latn the same way).
pub(crate) fn fails_script_guard(
    transcript_script: TextScript,
    output_script: TextScript,
    template_language: Option<&str>,
) -> bool {
    if transcript_script == output_script {
        return false;
    }
    match template_language.and_then(declared_script) {
        Some(declared) => declared != output_script,
        None => true,
    }
}

/// The upper fidelity bound, the mirror of forecast::fails_fidelity_guard
/// (which catches COLLAPSE): a cleanup that EXPANDED the text more than
/// 10x is not a cleanup (the model answered the transcript, or padded it).
/// Same >= 20-unit floor as the collapse guard, so short inputs are exempt
/// from ratio checks entirely. Pure and CJK-aware through text_units.
pub(crate) const EXPANSION_GUARD_RATIO: u64 = 10;

pub(crate) fn fails_expansion_guard(input: &str, output: &str) -> bool {
    let input_units = crate::local_llm::forecast::text_units(input);
    if input_units < 20 {
        return false;
    }
    crate::local_llm::forecast::text_units(output)
        > input_units.saturating_mul(EXPANSION_GUARD_RATIO)
}

/// THE shared output validator: every post-process success path (cloud
/// structured, cloud legacy, Apple Intelligence, the local engine) routes
/// its raw output through here before any `Some(..)` can be returned, so
/// the never-lose-the-transcript rule is enforced in exactly one place.
/// Pure, and colocated with the lifecycle that reports its failures.
///
/// Structured mode strips the think belt and invisibles, parses the JSON,
/// extracts the transcription field, and strips again; free-text mode
/// strips only. Both modes then apply, against the transcript:
///
/// 1. non-empty (an empty extraction never pastes),
/// 2. the CJK-aware length guards (collapse via forecast::
///    fails_fidelity_guard, expansion via fails_expansion_guard),
/// 3. language sanity (the dominant script must match unless the
///    template's language tag sanctions the change).
///
/// Every failure means the caller must fall back to the raw transcript.
pub(crate) fn validate_post_process_output(
    transcript: &str,
    generated: &str,
    mode: PostProcessOutputMode,
    template_language: Option<&str>,
) -> Result<String, PostProcessValidationFailure> {
    use crate::local_llm::SkipReason;
    let engine_invalid =
        |detail: &str| validation_failure(SkipReason::EngineFailed, detail.to_string());

    let extracted = match mode {
        PostProcessOutputMode::StructuredJson => {
            let content = strip_invisible_chars(strip_think_block(generated));
            match serde_json::from_str::<serde_json::Value>(&content) {
                Ok(json) => match json.get(TRANSCRIPTION_FIELD).and_then(|t| t.as_str()) {
                    Some(field) => strip_invisible_chars(strip_think_block(field)),
                    None => {
                        return Err(engine_invalid(
                            "the model output had no transcription field",
                        ))
                    }
                },
                Err(_) => {
                    return Err(engine_invalid("the model output was not valid JSON"));
                }
            }
        }
        PostProcessOutputMode::FreeText => strip_invisible_chars(strip_think_block(generated)),
    };
    if extracted.trim().is_empty() {
        return Err(engine_invalid("the model output was empty"));
    }
    if crate::local_llm::forecast::fails_fidelity_guard(transcript, &extracted) {
        return Err(validation_failure(
            SkipReason::LengthGuard,
            "the cleaned text collapsed below the fidelity threshold",
        ));
    }
    if fails_expansion_guard(transcript, &extracted) {
        return Err(validation_failure(
            SkipReason::LengthGuard,
            "the cleaned text expanded past the fidelity ceiling",
        ));
    }
    if let (Some(transcript_script), Some(output_script)) =
        (dominant_script(transcript), dominant_script(&extracted))
    {
        if fails_script_guard(transcript_script, output_script, template_language) {
            return Err(engine_invalid(&format!(
                "the output switched script from {} to {}",
                transcript_script.name(),
                output_script.name()
            )));
        }
    }
    Ok(extracted)
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
        // An isolated registry: this test mints 100+ runs to prove the
        // ring cap, and doing that on the process-wide singleton would
        // evict the live runs the parallel tests hold mid-flight.
        let registry = RunsRegistry::isolated_for_test();
        let first = registry.begin(None, meta("b", PostProcessEngineKind::Cloud, "m"));
        for _ in 0..(MAX_RUNS + 2) {
            registry.begin(None, meta("b", PostProcessEngineKind::Cloud, "m"));
        }
        let all = registry.latest(None);
        assert_eq!(all.len(), MAX_RUNS, "ring holds at most MAX_RUNS");
        // Newest first.
        assert!(all[0].run_id > all[all.len() - 1].run_id);
        // The very first run fell off the front.
        assert!(registry.snapshot(first).is_none(), "oldest run evicted");
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

    /// Cloud error classification moved into llm_client as the structured
    /// PostProcessError; its table is pinned there
    /// (completion_status_classification_table). The registry-level class
    /// tokens stay pinned here.
    #[test]
    fn failure_class_tokens_stay_snake_case() {
        assert_eq!(failure_class_str(PostProcessFailureClass::Auth), "auth");
        assert_eq!(
            failure_class_str(PostProcessFailureClass::ContextLength),
            "context_length"
        );
        assert_eq!(
            failure_class_str(PostProcessFailureClass::OutputInvalid),
            "output_invalid"
        );
        assert_eq!(
            failure_class_str(PostProcessFailureClass::Cancelled),
            "cancelled"
        );
    }

    // ---- The shared output validator (both engines) ----

    use super::{
        dominant_script, fails_expansion_guard, fails_script_guard, validate_post_process_output,
        PostProcessOutputMode, TextScript,
    };

    /// The dominant-script classifier over the pure tables: strong signals
    /// read their script, weak signals read None, ties read None, and
    /// digits/punctuation never carry a script.
    #[test]
    fn dominant_script_tables() {
        assert_eq!(dominant_script(""), None);
        assert_eq!(dominant_script("1234 !? ..."), None);
        assert_eq!(dominant_script("hi"), None, "under the signal floor");
        assert_eq!(dominant_script("hello world"), Some(TextScript::Latin));
        assert_eq!(
            dominant_script("Привет мир как дела"),
            Some(TextScript::Cyrillic)
        );
        assert_eq!(dominant_script("こんにちは世界"), Some(TextScript::Kana));
        assert_eq!(dominant_script("你好世界今天"), Some(TextScript::Han));
        assert_eq!(dominant_script("안녕하세요 세계"), Some(TextScript::Hangul));
        assert_eq!(
            dominant_script("नमस्ते दुनिया कैसी है"),
            Some(TextScript::Devanagari)
        );
        // Mixed but led: zh with a couple of English words stays Han.
        assert_eq!(
            dominant_script(&format!("你好世界测试 {} ok", "字".repeat(10))),
            Some(TextScript::Han)
        );
        // An exact tie is genuinely mixed: no dominant script, no check.
        assert_eq!(dominant_script("ab你好"), None);
    }

    /// The script guard: same script passes; a switch fails unless the
    /// template's language tag declares the output script (hi-Latn), and
    /// auto/None/unknown tags sanction nothing.
    #[test]
    fn script_guard_sanctions_only_declared_switches() {
        use TextScript::*;
        assert!(!fails_script_guard(Latin, Latin, None));
        assert!(!fails_script_guard(Han, Han, Some("zh")));
        assert!(
            fails_script_guard(Latin, Han, None),
            "an English transcript answered in Chinese fails"
        );
        assert!(fails_script_guard(Latin, Han, Some("auto")));
        assert!(fails_script_guard(Latin, Kana, Some("en")));
        // hi-Latn: Latin output for a Devanagari transcript is sanctioned.
        assert!(!fails_script_guard(Devanagari, Latin, Some("hi-Latn")));
        assert!(fails_script_guard(Devanagari, Latin, Some("hi")));
        assert!(fails_script_guard(Devanagari, Latin, Some("hi-Deva")));
        // The declared script must be the OUTPUT's: hi-Latn does not
        // sanction a Han answer.
        assert!(fails_script_guard(Devanagari, Han, Some("hi-Latn")));
    }

    /// The expansion guard: a cleanup that grew the text more than 10x
    /// fails on meaningful inputs, mirrors the collapse guard's 20-unit
    /// floor, and is CJK-aware through text_units.
    #[test]
    fn expansion_guard_bounds_runaway_output() {
        let input_20 = "one two three four five six seven eight nine ten \
                        eleven twelve thirteen fourteen fifteen sixteen \
                        seventeen eighteen nineteen twenty";
        assert!(fails_expansion_guard(
            input_20,
            &format!("word {}", "pad ".repeat(200))
        ));
        assert!(!fails_expansion_guard(input_20, "one two three four"));
        // Short inputs are exempt.
        assert!(!fails_expansion_guard("hi", &"x ".repeat(500)));
        // CJK counts per character.
        let zh_input: String = "字".repeat(30);
        assert!(fails_expansion_guard(&zh_input, &"字".repeat(400)));
        assert!(!fails_expansion_guard(&zh_input, &"字".repeat(200)));
    }

    /// The shared validator's language sanity: an English transcript whose
    /// cleaned output came back in Japanese fails (the model answered or
    /// translated); the same switch under an explicit hi-Latn template
    /// passes; a legitimate same-script cleanup passes in both modes.
    #[test]
    fn validator_rejects_script_switches_unless_sanctioned() {
        let transcript = "hello um world this is a test transcript with words";
        let japanese = "こんにちは、これはテストの書き起こしです。";
        for mode in [
            PostProcessOutputMode::StructuredJson,
            PostProcessOutputMode::FreeText,
        ] {
            let (transcript_text, output_text) = match mode {
                PostProcessOutputMode::StructuredJson => {
                    (transcript, format!("{{\"transcription\":\"{japanese}\"}}"))
                }
                PostProcessOutputMode::FreeText => (transcript, japanese.to_string()),
            };
            let failure = validate_post_process_output(transcript_text, &output_text, mode, None)
                .unwrap_err();
            assert!(
                failure.detail.contains("switched script"),
                "mode {mode:?}: {}",
                failure.detail
            );
        }

        // Sanctioned: a Devanagari transcript under an hi-Latn template may
        // come back Latin.
        let hindi = "नमस्ते दुनिया यह एक परीक्षण प्रतिलिपि है जिसमें कई शब्द हैं";
        let latin = "Namaste duniya yah ek parikshan pratilipi hai jis mein kai shabd hain";
        assert!(validate_post_process_output(
            hindi,
            &format!("{{\"transcription\":\"{latin}\"}}"),
            PostProcessOutputMode::StructuredJson,
            Some("hi-Latn"),
        )
        .is_ok());
        // Without the sanction the same output fails.
        assert!(validate_post_process_output(
            hindi,
            &format!("{{\"transcription\":\"{latin}\"}}"),
            PostProcessOutputMode::StructuredJson,
            Some("hi"),
        )
        .is_err());
    }

    /// The shared validator's length guards: a 10x expansion fails, a
    /// collapse fails (the existing forecast guard, still applied), and a
    /// faithful cleanup passes in both modes. This is the same function
    /// the local engine's validate_output delegates to.
    #[test]
    fn validator_length_guards_pin_both_directions() {
        let input_20 = "one two three four five six seven eight nine ten \
                        eleven twelve thirteen fourteen fifteen sixteen \
                        seventeen eighteen nineteen twenty";
        // 10x expansion.
        let expanded = format!("{} {}", input_20, "pad ".repeat(200).trim_end());
        let failure = validate_post_process_output(
            input_20,
            &format!("{{\"transcription\":\"{expanded}\"}}"),
            PostProcessOutputMode::StructuredJson,
            None,
        )
        .unwrap_err();
        assert_eq!(
            failure.skip_reason,
            crate::local_llm::SkipReason::LengthGuard
        );
        // Collapse (the pre-existing guard).
        let failure = validate_post_process_output(
            input_20,
            "{\"transcription\":\"gone\"}",
            PostProcessOutputMode::StructuredJson,
            None,
        )
        .unwrap_err();
        assert_eq!(
            failure.skip_reason,
            crate::local_llm::SkipReason::LengthGuard
        );
        // Empty output.
        let failure = validate_post_process_output(
            input_20,
            "{\"transcription\":\"\"}",
            PostProcessOutputMode::StructuredJson,
            None,
        )
        .unwrap_err();
        assert_eq!(
            failure.skip_reason,
            crate::local_llm::SkipReason::EngineFailed
        );
        // Faithful cleanup passes in both modes.
        assert_eq!(
            validate_post_process_output(
                input_20,
                "{\"transcription\":\"One two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty.\"}",
                PostProcessOutputMode::StructuredJson,
                None,
            )
            .unwrap(),
            "One two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty."
        );
        assert!(validate_post_process_output(
            input_20,
            "One two three four five six seven eight nine ten.",
            PostProcessOutputMode::FreeText,
            None,
        )
        .is_ok());
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
    /// Runs against a PRIVATE registry: the production sweep operates on
    /// the process-wide singleton, and a test sweeping the singleton would
    /// cancel whatever live runs the parallel registry tests hold.
    #[test]
    fn cancel_closes_live_dictation_runs_but_not_retries() {
        let registry = RunsRegistry::isolated_for_test();
        let dictation = registry.begin(
            None,
            meta(
                "transcribe_with_post_process",
                PostProcessEngineKind::Cloud,
                "m",
            ),
        );
        let retry = registry.begin(
            None,
            meta("history_retry", PostProcessEngineKind::Cloud, "m"),
        );
        registry.cancel_live_dictation_runs();
        assert_eq!(
            registry.snapshot(dictation).unwrap().outcome,
            Some(PostProcessOutcome::Failed {
                class: PostProcessFailureClass::Cancelled
            })
        );
        assert_eq!(registry.snapshot(retry).unwrap().outcome, None);
    }
}
