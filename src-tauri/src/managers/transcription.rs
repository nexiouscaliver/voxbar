use crate::audio_toolkit::command_matrix::{matrix_from_settings, CompiledCommandMatrix};
use crate::audio_toolkit::commands::{flush_command_prefix_len, held_prefix_len};
use crate::audio_toolkit::{
    apply_custom_words, apply_terminal_punctuation, apply_voice_deletion, detect_output_language,
    interim_display_transform, normalize_spoken_punctuation, normalize_transcription_output,
    remove_filler_words, remove_trailing_word_from_buffer_reporting, OutputLanguageEvidence,
    VoiceDeletionOutcome,
};
use crate::chinese_script::{convert_chinese_script, ChineseVariety};
use crate::engine_supervisor::{
    DeviceInfo, DeviceSelector, EngineError, EngineSupervisor, LoadSpec, LoadedInfo,
    StreamProgress, Unloading,
};
use crate::managers::audio::AudioRecordingManager;
use crate::managers::model::{
    canonical_language_code, EngineType, ModelInfo, ModelManager, ModelSource,
};
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
    language_intent: &str,
) -> Vec<memory::FallbackCandidate> {
    // Language-aware (spec F7): when the intent is concrete and not
    // English, only candidates serving the intent's base code are
    // offered. A silent swap to a wrong-language model would effectively
    // translate the dictation; with no downloaded candidate both fitting
    // RAM and serving the language, the gate's existing Refuse path
    // stands.
    let intent_base = canonical_language_code(language_intent);
    let language_gated =
        !language_intent.is_empty() && language_intent != "auto" && intent_base != "en";
    models
        .iter()
        .filter(|info| {
            info.is_downloaded
                && info.id != failed_id
                && (!language_gated
                    || info
                        .supported_languages
                        .iter()
                        .any(|lang| canonical_language_code(lang) == intent_base))
        })
        .map(|info| memory::FallbackCandidate {
            rank: fallback_rank(info),
            footprint_bytes: info.size_mb.saturating_mul(1024 * 1024),
            id: info.id.clone(),
        })
        .collect()
}

/// The memory-pressure gate's verdict for one attempted load. Plain data so
/// the toggle wiring ([`decide_memory_gate`]) is unit-testable without an
/// app handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MemoryGateDecision {
    /// Proceed with the requested model.
    Allow,
    /// Refuse the load with the not-enough-memory error.
    Refuse,
    /// Swap to this already-downloaded model id for this load.
    Fallback(String),
}

impl MemoryGateDecision {
    /// The wire word for the structured gate-decision log line.
    fn as_log_str(&self) -> String {
        match self {
            MemoryGateDecision::Allow => "allow".to_string(),
            MemoryGateDecision::Refuse => "refuse".to_string(),
            MemoryGateDecision::Fallback(id) => format!("fallback->{id}"),
        }
    }
}

/// The gate's decision, pure over its inputs (spec F3). The toggles are
/// passed in explicitly so their wiring is provable by test:
///
/// - `guard_enabled == false` (Settings' `memory_pressure_guard` off) returns
///   [`MemoryGateDecision::Allow`] IMMEDIATELY: the gate is bypassed
///   entirely - no refusal and, notably, no RAM auto-fallback either (the
///   candidate inventory is never even consulted).
/// - A forecast that fits (`forecast + headroom <= free`, `None` probe fails
///   open) is allowed - exactly [`memory::gate_should_refuse`]'s semantics.
///   `headroom` is the caller's user margin (the `memory_gate_headroom_mb`
///   setting in MB, multiplied by 1 MiB): 0 (the default) means the
///   forecast alone must fit; the gate never adds a hidden margin on top.
/// - A refusal becomes [`MemoryGateDecision::Fallback`] only when BOTH the
///   Settings' `auto_fallback` toggle and the caller's no-cascade
///   `allow_fallback` are on AND some downloaded candidate fits (against
///   the same headroom); with `auto_fallback == false` a refusal stays a
///   refusal - models are never swapped underneath the user.
pub(crate) fn decide_memory_gate<F>(
    guard_enabled: bool,
    auto_fallback: bool,
    allow_fallback: bool,
    free: Option<u64>,
    forecast: u64,
    headroom: u64,
    candidates: F,
    failed_id: &str,
) -> MemoryGateDecision
where
    F: FnOnce() -> Vec<memory::FallbackCandidate>,
{
    if !guard_enabled {
        return MemoryGateDecision::Allow;
    }
    if !memory::gate_should_refuse(free, forecast, headroom) {
        return MemoryGateDecision::Allow;
    }
    if allow_fallback && auto_fallback {
        if let Some(fallback) =
            memory::resolve_fallback_model(free, &candidates(), failed_id, headroom)
        {
            return MemoryGateDecision::Fallback(fallback.id.clone());
        }
    }
    MemoryGateDecision::Refuse
}

/// Format a forecast or free-memory amount for the refusal message:
/// integer MB below 1 GiB (minimum 1, so a tiny figure never reads as
/// nothing), one-decimal GB at or above it. 45,088,768 B renders "43 MB",
/// 766,509,056 B "731 MB", 1,610,612,736 B "1.5 GB". The old inline
/// GB-only formatting turned a 43 MiB model into "needs ~0.0 GB", telling
/// the user the model needs zero memory while being refused.
fn format_memory_amount(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    if bytes < GIB {
        format!("{} MB", (bytes / (1024 * 1024)).max(1))
    } else {
        format!("{:.1} GB", bytes as f64 / GIB as f64)
    }
}

/// Format the memory safety margin for the refusal message: ALWAYS integer
/// MB (minimum 1). The setting is MB-granular by construction, so MB is
/// exact, and the message must echo the unit of the Advanced UI the user
/// set the margin in (1536 MB, never format_memory_amount's "1.5 GB").
fn format_margin_mb(headroom: u64) -> String {
    format!("{} MB", (headroom / (1024 * 1024)).max(1))
}

/// The exact refusal string users see, pure over the decision's inputs so
/// the wording is unit-testable. The margin clause appears only when the
/// margin is non-zero.
fn memory_gate_refusal_message(
    model_name: &str,
    forecast: u64,
    free: u64,
    headroom: u64,
) -> String {
    let margin_clause = if headroom > 0 {
        format!(" plus a {} safety margin,", format_margin_mb(headroom))
    } else {
        String::new()
    };
    format!(
        "Not enough free memory for {}: needs ~{}{}, ~{} free (margin adjustable, guard can be \
         disabled, in Settings)",
        model_name,
        format_memory_amount(forecast),
        margin_clause,
        format_memory_amount(free),
    )
}

/// The structured numbers behind a memory-gate refusal, so the UI never
/// parses the error string. `free_bytes` is the resident-credited reading
/// the decision actually used. Present only on the gate's Refuse path; a
/// failed-open probe never refuses and leaves the field absent.
#[derive(Clone, Debug, Serialize)]
pub struct MemoryGateRefusalPayload {
    pub forecast_bytes: u64,
    pub free_bytes: u64,
    pub headroom_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelStateEvent {
    pub event_type: String,
    pub model_id: Option<String>,
    pub model_name: Option<String>,
    pub error: Option<String>,
    /// Structured memory-gate refusal numbers; absent except on the gate's
    /// Refuse path (loading_failed events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_gate: Option<MemoryGateRefusalPayload>,
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
/// volatile suffix the model may still rewrite. `deleted` carries the text a
/// buffer-side deletion just removed (the delete-word hotkey or a command-mode
/// DeleteWord / DeleteLine) so the overlay can show what went; absent from
/// the payload entirely when nothing was deleted.
#[derive(Clone, Debug, Serialize, Deserialize, Type, tauri_specta::Event)]
pub struct StreamTextEvent {
    pub committed: String,
    pub tentative: String,
    /// Present only when a buffer-side deletion just removed text; the
    /// payload omits the key entirely otherwise (the StreamPhaseEvent
    /// `kind` precedent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<String>,
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
    /// True while the command-mode modifier is held for this session. The
    /// flag mirrors the coordinator's decision (press during a live
    /// session) at snapshot granularity; see [`Self::render`].
    command_active: bool,
    /// True while a command delta's trailing fragment is HELD BACK (a
    /// proper prefix of some command phrase, possibly a partial word).
    /// The held region is the `raw_seen` shortfall itself (nothing is
    /// stored twice); the marker exists because `render` assigns
    /// `last_full` BEFORE the modifier branch, so a release-tick
    /// shortfall alone cannot tell a held fragment apart from FRESH
    /// release-snapshot dictation. Only a set marker fires the
    /// release/finalize flush.
    holding: bool,
    /// Text removed by the most recent buffer-side deletion (the
    /// delete-word hotkey or a command-mode DeleteWord / DeleteLine), set
    /// by whichever path applied it and drained by [`Self::take_deleted`]
    /// for the next stream-text emission so the overlay can show what
    /// went. Draining keeps a stale value off later emissions.
    last_deleted: Option<String>,
    /// The compiled command matrix this session's command grammar and
    /// interim transforms run against, captured at `begin` (the edited
    /// table when `command_phrases` is set, the shared defaults
    /// otherwise). Per-tick cost is a clone of an Arc; no regex rebuilds.
    matrix: Arc<CompiledCommandMatrix>,
    /// Captured at `begin` from `selected_language == "hi-Latn"`: the
    /// interim display transliterates Devanagari to Roman so the overlay
    /// matches the paste (the finalize pipeline does the same).
    hinglish: bool,
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
            command_active: false,
            holding: false,
            last_deleted: None,
            matrix: crate::audio_toolkit::command_matrix::default_compiled_matrix(),
            hinglish: false,
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
        matrix: Arc<CompiledCommandMatrix>,
        hinglish: bool,
    ) {
        self.live = true;
        self.command_active = false;
        self.holding = false;
        self.last_deleted = None;
        self.spoken_punctuation = spoken_punctuation;
        self.voice_deletion = voice_deletion;
        self.preview_script = preview_script;
        self.supported_languages = supported_languages.to_vec();
        self.matrix = matrix;
        self.hinglish = hinglish;
        self.base.clear();
        self.raw_seen.clear();
        self.last_full.clear();
    }

    fn end(&mut self) {
        self.live = false;
        self.command_active = false;
        self.holding = false;
        self.last_deleted = None;
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
    ///
    /// `command_modifier` is whether the command-mode binding is held for
    /// this live session (the coordinator's mirror). While it is held,
    /// engine material beyond what was already consumed is a COMMAND
    /// DELTA, not dictation: the delta is parsed by the command grammar
    /// and its actions edit `base` directly (punctuation/newline inserts,
    /// delete word, delete line); unrecognized words are discarded - that
    /// is the command contract. The ENGAGEMENT tick folds the whole
    /// current snapshot into `base` verbatim (in-flight tentative words
    /// included, so a word completing across the boundary is never
    /// truncated into a bogus one-letter command) and marks it consumed
    /// via `raw_seen`; only material arriving on later snapshots parses
    /// as commands.
    ///
    /// A delta whose TRAILING token sequence is a proper prefix of some
    /// command phrase (the last token possibly a partial word, as when a
    /// streaming snapshot cuts "comma" into "com") is HELD BACK: the
    /// delta before the fragment applies, `raw_seen` stops at the
    /// fragment's start INCLUDING its preceding separator, and the
    /// `holding` marker is set. Nothing is stored twice: the shortfall
    /// between `raw_seen` and the snapshot IS the held region, so the
    /// next delta re-includes the whole token and nothing double-counts;
    /// the fragment stays visible in the interim display, separator
    /// included. Releasing with a fragment held flushes it through the
    /// grammar (see [`Self::flush_held_region`]) and then resumes normal
    /// dictation from the consumed boundary; releasing with nothing held
    /// behaves exactly as before. The finalized buffer (and therefore the
    /// final paste) is exactly the edited `base` plus any later normal
    /// speech, because `combine_final` never re-consumes material covered
    /// by `raw_seen`.
    fn render(&mut self, committed: &str, tentative: &str, command_modifier: bool) -> String {
        let snapshot = format!("{committed}{tentative}");
        self.last_full = snapshot.clone();
        if command_modifier {
            if !self.command_active {
                self.base = self.combine(&snapshot);
                self.raw_seen = snapshot;
                self.command_active = true;
            } else {
                let keep = common_prefix_len(&self.raw_seen, &snapshot);
                let delta = snapshot[keep..].to_string();
                let held = held_prefix_len(&delta, &self.matrix);
                let applicable_end = delta.len() - held;
                self.last_deleted = crate::audio_toolkit::apply_command_delta_to_buffer(
                    &mut self.base,
                    &delta[..applicable_end],
                    &self.matrix,
                );
                self.raw_seen = snapshot[..snapshot.len() - held].to_string();
                self.holding = held > 0;
            }
        } else if self.holding {
            // Release tick with a fragment still held: the shortfall
            // mixes the held fragment with the release snapshot's FRESH
            // dictation, which is why the marker gates this flush. Apply
            // the flush span rule from raw_seen's end (the common prefix;
            // an engine revision inside the held region is consumed by
            // the same rule, the pre-existing revision edge), then resume
            // normal dictation: the remainder of the release snapshot
            // flows through combine() and is never parsed as commands.
            let start = common_prefix_len(&self.raw_seen, &snapshot);
            let region = snapshot[start..].to_string();
            let consumed = self.flush_held_region(&region);
            self.raw_seen = snapshot[..start + consumed].to_string();
            self.holding = false;
            self.command_active = false;
        } else {
            self.command_active = false;
        }
        self.interim_display()
    }

    /// The interim display string for the combined raw buffer: script
    /// conversion, then Hinglish transliteration (Devanagari to Roman,
    /// captured at `begin` from the "hi-Latn" intent so the overlay
    /// matches the paste), then the interim text passes.
    fn interim_display(&mut self) -> String {
        let raw = self.combine(&self.last_full);
        let (converted, _) = self
            .preview_script
            .convert(&raw, "", &self.supported_languages);
        let converted = if self.hinglish {
            crate::hindi_script::transliterate_devanagari_to_roman(&converted)
        } else {
            converted
        };
        interim_display_transform(
            &converted,
            self.spoken_punctuation,
            self.voice_deletion,
            &self.matrix,
        )
    }

    /// Flush an unresolved held fragment through the command grammar and
    /// return the byte count the flush consumed from `region`. Shared by
    /// the release tick and the finalize fold: consume the region's first
    /// word with its preceding separator, then whole words while the span
    /// remains a proper prefix of some command phrase (so a held " new l"
    /// resolves to the whole "new line"); the consumed span parses as
    /// commands (unrecognized words discarded per the contract) and any
    /// remainder stays unconsumed, flowing on as normal dictation.
    fn flush_held_region(&mut self, region: &str) -> usize {
        let consumed = flush_command_prefix_len(region, &self.matrix);
        if consumed > 0 {
            self.last_deleted = crate::audio_toolkit::apply_command_delta_to_buffer(
                &mut self.base,
                &region[..consumed],
                &self.matrix,
            );
        }
        consumed
    }

    /// Take the text removed by the most recent buffer-side deletion (the
    /// delete-word hotkey or a command-mode DeleteWord / DeleteLine), if
    /// any, so the caller can surface it on the stream-text emission.
    /// Draining keeps a stale value off later emissions.
    fn take_deleted(&mut self) -> Option<String> {
        self.last_deleted.take()
    }

    /// Apply the delete-last-word hotkey to the buffer. Returns the refreshed
    /// display text, or `None` when no live session buffer exists (no
    /// stream, batch model, or the session already finalized).
    fn delete_last_word(&mut self) -> Option<String> {
        if !self.live {
            return None;
        }
        let buffer = self.combine(&self.last_full);
        let (edited, removed_word) = remove_trailing_word_from_buffer_reporting(&buffer);
        self.base = edited;
        // Report what the hotkey removed so the overlay can show it.
        self.last_deleted = removed_word;
        self.raw_seen = self.last_full.clone();
        // The held region (if any) is consumed unparsed; a stale marker
        // must never fire a later flush.
        self.holding = false;
        Some(self.interim_display())
    }

    /// Clear everything dictated so far, keeping the session live: the
    /// buffer empties and everything the engine has already reported is
    /// consumed via `raw_seen`, so only speech after this point reaches
    /// the final text. Key-based start-over (the Undo binding's in-session
    /// semantics); mirrors the voice clear-everything commands
    /// ("delete everything" / "scratch everything").
    fn clear_all(&mut self) -> Option<String> {
        if !self.live {
            return None;
        }
        self.base = String::new();
        self.raw_seen = self.last_full.clone();
        // Same as the delete-word hotkey: the reset consumes any held
        // fragment without parsing it, and the marker must not survive.
        // The clear-everything reset is not one of the instrumented
        // word/line deletions, so nothing is reported as removed.
        self.holding = false;
        self.last_deleted = None;
        Some(self.interim_display())
    }

    /// Fold the engine's final raw text into the buffer and end the session.
    /// With no manual edits this is exactly the engine text unchanged.
    ///
    /// A fragment still held at finalize flushes through the grammar first
    /// (ONLY on the `holding` marker: final material beyond the last
    /// consumed snapshot is dictation by design, so a raw_seen shortfall
    /// alone must not trigger a parse). The flush consumes from the common
    /// prefix with `raw_seen`; everything beyond stays dictation and the
    /// fold appends it as usual.
    fn combine_final(&mut self, final_raw: String) -> String {
        let combined = if self.live {
            if self.holding {
                let start = common_prefix_len(&self.raw_seen, &final_raw);
                let region = final_raw[start..].to_string();
                let consumed = self.flush_held_region(&region);
                self.raw_seen = final_raw[..start + consumed].to_string();
                self.holding = false;
            }
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
    /// `true` only when a live reading was taken from the transcribe-cpp
    /// worker process (Activity-Monitor-equivalent physical footprint on
    /// macOS, RSS elsewhere); `false` marks the size-derived estimate
    /// (in-process ONNX engines, or the worker's footprint could not be
    /// read). The measured figure includes the worker's runtime overhead
    /// (runtime + compute buffers), because that memory is really resident -
    /// it is what the system process monitor shows for the worker, not just
    /// the weights file. `None` when nothing is resident or no estimate
    /// resolves - callers omit the segment. Distinct from the gate's
    /// [`Self::resident_model_footprint_bytes`] above, which credits
    /// TranscribeCpp by conservative measured RSS only (load decisions must
    /// not assume clean pages were freed).
    pub fn resident_model_footprint(&self) -> Option<(u64, bool)> {
        let estimate = || {
            self.get_current_model()
                .and_then(|id| self.model_manager.get_model_info(&id))
                .map(|info| info.size_mb.saturating_mul(1024 * 1024))
        };
        if self.engine.loaded().is_some() {
            if let Some(footprint) = self
                .engine
                .worker_pid()
                .and_then(memory::display_footprint_bytes_for_pid)
            {
                return Some((footprint, true));
            }
            // Footprint read failed → fall back to the ~-estimate (spec F4).
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
        let language_intent = get_settings(&self.app_handle).selected_language;
        fallback_candidate_list(
            &self.model_manager.get_available_models(),
            failed_id,
            &language_intent,
        )
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
                memory_gate: None,
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
                memory_gate: None,
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
                memory_gate: None,
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
                        memory_gate: None,
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
                    memory_gate: None,
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
        // failure fails open (gate returns false for `None`). The whole
        // block is skipped when memory_pressure_guard is off - the toggle
        // bypasses the gate entirely, the RAM auto-fallback included. The
        // margin is the user's memory_gate_headroom_mb setting (default 0);
        // no hidden headroom is added on top.
        let forecast = model_info.size_mb.saturating_mul(1024 * 1024);
        let gate_settings = get_settings(&self.app_handle);
        if gate_settings.memory_pressure_guard {
            let headroom = gate_settings
                .memory_gate_headroom_mb
                .saturating_mul(1024 * 1024);
            let credit = self.resident_model_footprint_bytes();
            let probe = memory::probe_availability();
            let free = probe.available_bytes.map(|f| f.saturating_add(credit));
            if free.is_none() {
                // Spec F3: the fail-open path must log a WARNING so an inert
                // probe is visible on the console (default filter Info), not
                // silently skipped.
                warn!(
                    "memory gate: available-memory probe unavailable, failing open for {}",
                    model_info.name
                );
            }
            let decision = decide_memory_gate(
                true,
                gate_settings.auto_fallback,
                allow_fallback,
                free,
                forecast,
                headroom,
                || self.fallback_candidates(model_id),
                model_id,
            );
            // The one structured line at every gate decision: probe bytes,
            // the kernel pressure verdict, the inactive factor the probe's
            // composition applied, the model, its forecast, and the verdict.
            // This is the field diagnostic for any future misfire.
            info!(
                "memory gate decision: probe_bytes={} pressure_level={} inactive_factor={} model={} forecast_bytes={} decision={}",
                free.map(|b| b.to_string()).unwrap_or_else(|| "unavailable".to_string()),
                match probe.pressure_level {
                    Some(level) => level.to_string(),
                    None => "unreadable".to_string(),
                },
                probe
                    .inactive_factor
                    .map(|f| format!("{f:.2}"))
                    .unwrap_or_else(|| "n/a".to_string()),
                model_id,
                forecast,
                decision.as_log_str(),
            );
            match decision {
                MemoryGateDecision::Allow => {}
                // RAM auto-fallback: load the best ALREADY-DOWNLOADED model
                // that fits instead of failing the dictation. The
                // tray/indicator state follows the model that is actually
                // resident (current_model_id is set to the fallback below),
                // and the frontend gets a one-shot "model-fallback" event to
                // toast the switch. Toggle off (or nothing fitting) keeps the
                // exact refuse-with-toast path.
                MemoryGateDecision::Fallback(fallback_id) => {
                    let fallback_name = self
                        .model_manager
                        .get_model_info(&fallback_id)
                        .map(|info| info.name)
                        .unwrap_or_else(|| fallback_id.clone());
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
                    return self.load_model_with_device_internal(&fallback_id, device_index, false);
                }
                MemoryGateDecision::Refuse => {
                    let free_bytes = free.unwrap_or(0);
                    let error_msg = memory_gate_refusal_message(
                        &model_info.name,
                        forecast,
                        free_bytes,
                        headroom,
                    );
                    warn!("memory gate refused a load: {}", error_msg);
                    // The structured numbers ride along so the UI never
                    // parses the message: forecast, the resident-credited
                    // free reading the decision used, and the margin.
                    let _ = self.app_handle.emit(
                        "model-state-changed",
                        ModelStateEvent {
                            event_type: "loading_failed".to_string(),
                            model_id: Some(model_id.to_string()),
                            model_name: Some(model_info.name.clone()),
                            error: Some(error_msg.clone()),
                            memory_gate: Some(MemoryGateRefusalPayload {
                                forecast_bytes: forecast,
                                free_bytes,
                                headroom_bytes: headroom,
                            }),
                        },
                    );
                    return Err(anyhow::anyhow!(error_msg));
                }
            }
        } else {
            // memory_pressure_guard off: the gate is bypassed entirely (no
            // probe, no refusal, no fallback). One structured line so a
            // future misfire report shows the toggle state, same shape as
            // the engaged-gate line above.
            info!(
                "memory gate decision: probe_bytes=skipped pressure_level=skipped \
                 inactive_factor=n/a model={} forecast_bytes={} decision=allow \
                 reason=memory_pressure_guard_disabled",
                model_id, forecast
            );
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
                memory_gate: None,
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
                    // The command-mode modifier is consulted per snapshot:
                    // while it is held for this live session the new engine
                    // material parses as commands and edits the buffer
                    // instead of appending as dictation. A missing
                    // coordinator reads as "not held".
                    let command_modifier = app_handle
                        .try_state::<crate::TranscriptionCoordinator>()
                        .is_some_and(|c| c.is_command_modifier_active());
                    let (display, deleted) = {
                        let mut session = session_buffer_for_progress
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        let display =
                            session.render(&text.committed, &text.tentative, command_modifier);
                        // Surface what a command-mode deletion just removed
                        // (None on every ordinary tick).
                        (display, session.take_deleted())
                    };
                    // The whole displayed text is emitted as the committed
                    // part: the interim transform runs over the full raw
                    // buffer, so the committed/tentative visual split cannot
                    // be preserved exactly across punctuation joins and
                    // deletions. The model's own rewrites still surface
                    // because the display is recomputed from every snapshot.
                    emit_stream_text(&app_handle, &display, "", deleted.as_deref());
                }
                perf.maybe_log();
            }
        };

        // The session buffer goes live before the engine stream starts, so
        // no interim callback can race past `begin`. Toggles and the
        // compiled command matrix are captured here (once per session),
        // matching `PreviewScript`. The auto-interpretation master gate
        // ANDs with the per-pass toggles (same composition as
        // post_process_transcription_text) so the interim display matches
        // the paste.
        self.session_buffer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .begin(
                preview_script,
                settings.spoken_punctuation && settings.auto_interpret_commands,
                settings.voice_deletion_commands && settings.auto_interpret_commands,
                &languages,
                matrix_from_settings(&settings),
                settings.selected_language == "hi-Latn",
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
        // session-buffer edits (delete-last-word hotkey, in-session command
        // grammar: both consume engine material into `raw_seen` and edit
        // `base`). The finalize path then transforms this raw transcript
        // exactly once through the canonical pipeline; it is never fed the
        // interim-displayed text. Idempotence argument: the interim display
        // transform is recomputed from the raw buffer on every tick and
        // never writes back into it (the engine accumulator and `base` stay
        // raw-domain), so no text pass can run twice over the same words.
        // The interim transform is additionally idempotent on its own
        // output (tested), but the architecture does not rely on that.
        let final_raw = self
            .session_buffer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .combine_final(finalized.text);

        let settings = get_settings(&self.app_handle);
        // Streaming models do not receive a decode prompt, so custom words
        // always go through the shared fuzzy post-correction path.
        let filtered = post_process_transcription_text(
            final_raw,
            &settings,
            false,
            &finalized.output_language,
            &finalized.supported_languages,
        );

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
                let deleted = session.take_deleted();
                let _ = StreamTextEvent {
                    committed: display,
                    tentative: String::new(),
                    deleted,
                }
                .emit(&self.app_handle);
                true
            }
            None => false,
        }
    }

    /// Apply the Undo binding to the live session buffer: clears everything
    /// dictated so far (a key-based start-over). Returns `true` when a live
    /// buffer was cleared and the overlay refreshed; `false` when there is
    /// no live buffer (same contract as the word-deletion path).
    pub fn clear_session_buffer(&self) -> bool {
        let mut session = self
            .session_buffer
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match session.clear_all() {
            Some(display) => {
                let _ = StreamTextEvent {
                    committed: display,
                    tentative: String::new(),
                    // The clear-everything reset is not one of the
                    // instrumented word/line deletions; nothing reports as
                    // removed.
                    deleted: None,
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
    /// fallback, not the persisted selection. History entries record it so
    /// a mid-dictation model switch stays auditable. Returns `(text, model_id)`;
    /// the model id is empty when unknown.
    pub fn transcribe_with_model(&self, audio: Vec<f32>) -> Result<(String, String)> {
        self.transcribe_audio(audio)
    }

    fn transcribe_audio(&self, audio: Vec<f32>) -> Result<(String, String)> {
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

        // Apply fuzzy word correction if custom words are configured, UNLESS the
        // words were already handed to the model as an initial prompt (whisper
        // family). We don't pass a prompt to non-whisper models (it requires
        // the whisper-kind run extension), so they still get fuzzy correction here,
        // same as the ONNX engines.
        let filtered_result = post_process_transcription_text(
            run.text,
            &settings,
            run.model_is_whisper,
            &output_language,
            &run.languages,
        );

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
                        memory_gate: None,
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

fn emit_stream_text(
    app_handle: &AppHandle,
    committed: &str,
    tentative: &str,
    deleted: Option<&str>,
) {
    let _ = StreamTextEvent {
        committed: committed.to_string(),
        tentative: tentative.to_string(),
        deleted: deleted.map(str::to_string),
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
            && canonical_language_code(&settings.selected_language)
                == canonical_language_code(language)
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
    // The command matrix compiles once per call (shared Arc for the
    // defaults; only an edited table builds fresh regexes) and feeds both
    // spoken-command passes below.
    let matrix = matrix_from_settings(settings);
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

        // Hinglish (selected_language "hi-Latn") expresses a SCRIPT intent:
        // the model still yields Devanagari, so transliterate to Roman
        // before every text pass. This is script conversion (the same class
        // as the Chinese slot above), never translation, and it only runs
        // when the intent explicitly selects it.
        let raw = if settings.selected_language == "hi-Latn" {
            crate::hindi_script::transliterate_devanagari_to_roman(&raw)
        } else {
            raw
        };

        // Spoken punctuation first, then voice deletion, then the terminal
        // fallback, so the custom-word pass and every later stage see final
        // wording and punctuation. Each pass is independently toggleable;
        // off reproduces today's behavior. The auto-interpretation master
        // gate ANDs with the per-pass toggles in NORMAL dictation only;
        // OFF leaves command words as plain words (the command-mode
        // modifier is a separate surface and stays untouched).
        let punctuated = if settings.spoken_punctuation && settings.auto_interpret_commands {
            normalize_spoken_punctuation(&raw, &matrix)
        } else {
            raw
        };

        // Voice deletion runs after punctuation insertion (so "hello comma
        // scratch that" deletes the punctuated word "hello,") and before the
        // terminal fallback. A "delete everything" command short-circuits
        // every later pass: the dictation pastes nothing (the paste site
        // already skips empty text).
        let deleted = if settings.voice_deletion_commands && settings.auto_interpret_commands {
            apply_voice_deletion(&punctuated, &matrix)
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
            crate::audio_toolkit::command_matrix::default_compiled_matrix(),
            false,
        );
        buffer
    }

    #[test]
    fn session_buffer_without_edits_passes_snapshots_through() {
        let mut session = session_buffer();
        assert_eq!(session.render("Hello ", "wor", false), "Hello wor");
        assert_eq!(session.render("Hello world ", "", false), "Hello world ");

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
        session.render("hello ", "world", false);
        assert_eq!(session.delete_last_word(), Some("hello ".to_string()));

        // The engine commits the (already deleted) tentative word verbatim
        // and the user keeps talking: the word must not resurrect.
        assert_eq!(
            session.render("hello world and more", "", false),
            "hello and more"
        );
        assert_eq!(
            session.combine_final("hello world and more".to_string()),
            "hello and more"
        );
    }

    #[test]
    fn session_buffer_delete_committed_word_joins_cleanly() {
        let mut session = session_buffer();
        session.render("hello world", "", false);
        assert_eq!(session.delete_last_word(), Some("hello ".to_string()));

        // New speech arrives after the committed prefix that was already
        // consumed: exactly one separating space survives the join.
        assert_eq!(session.render("hello world next", "", false), "hello next");
    }

    #[test]
    fn session_buffer_delete_word_repeated_and_on_empty() {
        let mut session = session_buffer();
        session.render("one two three", "", false);
        assert_eq!(session.delete_last_word(), Some("one two ".to_string()));
        assert_eq!(session.delete_last_word(), Some("one ".to_string()));
        assert_eq!(session.delete_last_word(), Some("".to_string()));
        // Deleting from an empty buffer is a no-op that still refreshes.
        assert_eq!(session.delete_last_word(), Some("".to_string()));

        // The interim display transform trims a leading space; the raw
        // buffer keeps it, so speech after a full deletion joins normally.
        assert_eq!(session.render(" fresh", "", false), "fresh");
        assert_eq!(session.render(" fresh words", "", false), "fresh words");
    }

    #[test]
    fn session_buffer_delete_word_requires_live_session() {
        let mut session = StreamSessionBuffer::default();
        assert_eq!(session.delete_last_word(), None);

        // After finalize the buffer is no longer live.
        let mut live = session_buffer();
        live.render("hello", "", false);
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
        assert_eq!(session.render("say full", " stop now", false), "say. Now");

        // After a manual edit the same holds across the base/new-material
        // seam: "full" lands in base, "stop" in the new material.
        let mut edited = session_buffer();
        edited.render("alpha ", "beta", false);
        edited.delete_last_word(); // removes "beta", freezes "alpha "
        assert_eq!(
            edited.render("alpha beta full", " stop now", false),
            "alpha. Now"
        );
    }

    #[test]
    fn session_buffer_display_grows_no_terminal_punctuation() {
        let mut session = session_buffer();
        // Mid-sentence, mid-question: no period or question mark may appear
        // on any tick (the terminal fallback runs only at finalize).
        assert_eq!(session.render("hello world", "", false), "hello world");
        assert_eq!(session.render("what is this", "", false), "what is this");

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
        session.render("one two", "", false);
        session.delete_last_word(); // buffer is now "one "
        session.render("one two three", "", false);
        assert_eq!(
            session.render("one two three scratch that", "", false),
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

    // -----------------------------------------------------------------
    // Command-mode modifier over the live session buffer: engagement
    // folds the snapshot so far in as dictation, later deltas parse as
    // grammar commands editing the buffer, unrecognized words are
    // discarded, and the final paste is exactly the edited buffer.
    // -----------------------------------------------------------------

    #[test]
    fn session_buffer_command_modifier_edits_instead_of_appending() {
        let mut session = session_buffer();
        session.render("hello world", "", false);

        // Engagement tick: the snapshot so far is dictation, folded
        // verbatim (the flag arrives with this snapshot, material in it
        // predates activation).
        assert_eq!(session.render("hello world", "", true), "hello world");
        // Command words arriving on later snapshots while held parse and
        // edit the buffer instead of appending as words.
        assert_eq!(
            session.render("hello world comma", "", true),
            "hello world,"
        );
        assert_eq!(
            session.render("hello world comma question mark", "", true),
            "hello world,?"
        );
        // Unrecognized words while held are DISCARDED (the command contract).
        assert_eq!(
            session.render("hello world comma question mark um stuff", "", true),
            "hello world,?"
        );

        // Released: dictation resumes appending beyond the consumed
        // snapshot; the command words never re-enter the raw buffer.
        assert_eq!(
            session.render(
                "hello world comma question mark um stuff and more",
                "",
                false
            ),
            "hello world,? and more"
        );
        // The final paste delivers exactly the edited buffer (plus later
        // normal speech); the command tokens are gone.
        assert_eq!(
            session.combine_final("hello world comma question mark um stuff and more".to_string()),
            "hello world,? and more"
        );
    }

    #[test]
    fn session_buffer_command_modifier_delete_word_and_line() {
        let mut session = session_buffer();
        session.render("alpha beta", "", false);
        session.render("alpha beta", "", true); // engage

        // "delete word" removes the trailing word from the buffer.
        assert_eq!(session.render("alpha beta delete word", "", true), "alpha ");
        // "new line" inserts a break; unrecognized filler is discarded.
        assert_eq!(
            session.render("alpha beta delete word new line gamma delta", "", true),
            "alpha \n"
        );
        // Released: normal speech resumes on the fresh line (the engine
        // separates it with a space; the raw domain keeps it).
        assert_eq!(
            session.render(
                "alpha beta delete word new line gamma delta second line words",
                "",
                false
            ),
            "alpha \n second line words"
        );
        // Re-engaged "delete line" clears the current trailing line back
        // to the newline (same semantics as the voice-deletion phrase).
        assert_eq!(
            session.render(
                "alpha beta delete word new line gamma delta second line words delete line",
                "",
                true
            ),
            "alpha \n"
        );
        // A delete line with no newline clears the whole buffer.
        let mut single = session_buffer();
        single.render("only line", "", false);
        single.render("only line", "", true); // engage
        assert_eq!(single.render("only line delete line", "", true), "");
    }

    #[test]
    fn session_buffer_command_modifier_engagement_keeps_in_flight_tentative() {
        // A word still tentative at the engagement tick completes on the
        // next snapshot: the completion is dictation that predates
        // activation, so it must survive intact rather than parse as a
        // one-letter "command" (and be discarded).
        let mut session = session_buffer();
        session.render("hello ", "wor", false);
        assert_eq!(session.render("hello world", "", true), "hello world");
        assert_eq!(
            session.render("hello world comma", "", true),
            "hello world,"
        );
    }

    #[test]
    fn session_buffer_command_modifier_words_stay_out_of_final_raw() {
        // The engine's final text contains the command words verbatim;
        // everything covered by raw_seen at finalize is skipped, so the
        // final raw is the edited buffer plus genuinely new material.
        let mut session = session_buffer();
        session.render("one two", "", false);
        session.render("one two", "", true); // engage
        session.render("one two delete word new line", "", true);
        assert_eq!(
            session.combine_final("one two delete word new line three".to_string()),
            "one \n three"
        );
    }

    // -----------------------------------------------------------------
    // Fragmented command words: a delta ending mid-token or mid-phrase is
    // a proper prefix of a vocabulary phrase, so it is HELD (visible in
    // the interim display, separator intact) until a later delta
    // completes it; release/finalize flush what never completed.
    // -----------------------------------------------------------------

    #[test]
    fn session_buffer_command_modifier_fragmented_word_parses_whole() {
        let mut session = session_buffer();
        session.render("hello world", "", false);
        session.render("hello world", "", true); // engage on a clean snapshot

        // The fragment arrives on a LATER tick: "com" is a partial-word
        // proper prefix of "comma", so the delta is held, no command
        // applies, and the display keeps the separator.
        assert_eq!(
            session.render("hello world com", "", true),
            "hello world com"
        );
        // The next delta re-includes the whole token (the raw_seen
        // shortfall IS the held region); the comma applies exactly once
        // and no stray "com" survives.
        assert_eq!(
            session.render("hello world comma", "", true),
            "hello world,"
        );
    }

    #[test]
    fn session_buffer_command_modifier_fragmented_phrase_parses_whole() {
        let mut session = session_buffer();
        session.render("hello world", "", false);
        session.render("hello world", "", true); // engage

        // " question" is a whole word that only OPENS "question mark":
        // held rather than silently discarded.
        assert_eq!(
            session.render("hello world question", "", true),
            "hello world question"
        );
        // The completing word arrives on the next delta: the phrase
        // parses whole.
        assert_eq!(
            session.render("hello world question mark", "", true),
            "hello world?"
        );
    }

    #[test]
    fn session_buffer_command_modifier_release_flush_discards_unresolved_fragment() {
        let mut session = session_buffer();
        session.render("hello world", "", false);
        session.render("hello world", "", true); // engage
        assert_eq!(
            session.render("hello world com", "", true),
            "hello world com"
        );

        // Released with the snapshot unchanged: the flush consumes the
        // fragment, the grammar discards it ("com" is no command), and it
        // never re-enters the buffer.
        assert_eq!(session.render("hello world com", "", false), "hello world");
        assert_eq!(
            session.combine_final("hello world com".to_string()),
            "hello world"
        );
    }

    #[test]
    fn session_buffer_release_flush_fuzzy_near_miss_fragment_resolves() {
        // Declared narrowed contract: a release-time held fragment that is
        // a proper prefix of a >= 5-char single-word phrase AND within edit
        // distance 1 of it resolves to that command at flush. The user was
        // issuing a command, so converting is the desired outcome.
        let mut session = session_buffer();
        session.render("hello world", "", false);
        session.render("hello world", "", true); // engage
        assert_eq!(
            session.render("hello world perio", "", true),
            "hello world perio"
        );
        assert_eq!(session.render("hello world perio", "", false), "hello world.");
        // Fragments outside fuzzy reach ("com" is 3 chars, distance 2 from
        // "comma") still discard: the pre-existing pin's exact input.
        let mut short = session_buffer();
        short.render("hello world", "", false);
        short.render("hello world", "", true); // engage
        assert_eq!(
            short.render("hello world com", "", true),
            "hello world com"
        );
        assert_eq!(short.render("hello world com", "", false), "hello world");
    }

    #[test]
    fn session_buffer_command_modifier_release_flush_resolves_and_appends_dictation() {
        let mut session = session_buffer();
        session.render("hello world", "", false);
        session.render("hello world", "", true); // engage
        assert_eq!(
            session.render("hello world com", "", true),
            "hello world com"
        );

        // Released with the snapshot GROWN: the flush span resolves the
        // held fragment into the comma command and stops; the fresh
        // release-snapshot dictation " and more" is appended, never
        // parsed as commands. This is the exact rhythm the holding
        // marker gates (a bare raw_seen shortfall cannot tell the two
        // apart).
        assert_eq!(
            session.render("hello world comma and more", "", false),
            "hello world, and more"
        );
    }

    #[test]
    fn session_buffer_command_modifier_finalize_flush_resolves_held_fragment() {
        let mut session = session_buffer();
        session.render("hello world", "", false);
        session.render("hello world", "", true); // engage
        assert_eq!(
            session.render("hello world com", "", true),
            "hello world com"
        );

        // Finalize flushes ONLY because the holding marker is set: the
        // held word resolves to the comma from the final text and the
        // remainder (" three") appends as dictation.
        assert_eq!(
            session.combine_final("hello world comma three".to_string()),
            "hello world, three"
        );
    }

    #[test]
    fn session_buffer_command_modifier_duplicate_symbol_inserts_coalesce() {
        let mut session = session_buffer();
        session.render("hello world", "", false);
        session.render("hello world", "", true); // engage

        // A whole "comma" delta applies on arrival.
        assert_eq!(
            session.render("hello world comma", "", true),
            "hello world,"
        );
        // Repeating the identical symbol command coalesces: one comma.
        assert_eq!(
            session.render("hello world comma comma", "", true),
            "hello world,"
        );
        // Line breaks never coalesce, and a comma after a line break
        // still lands.
        assert_eq!(
            session.render("hello world comma comma new line", "", true),
            "hello world,\n"
        );
        assert_eq!(
            session.render("hello world comma comma new line comma", "", true),
            "hello world,\n,"
        );
    }

    // -----------------------------------------------------------------
    // Deletion reporting: the delete-word hotkey and the command-mode
    // DeleteWord / DeleteLine surface exactly what they removed, so the
    // refreshed display emission can carry deleted: Some(text).
    // -----------------------------------------------------------------

    #[test]
    fn session_buffer_hotkey_word_deletion_reports_the_removed_word() {
        let mut session = session_buffer();
        session.render("one two three", "", false);
        assert_eq!(session.delete_last_word(), Some("one two ".to_string()));
        assert_eq!(session.take_deleted(), Some("three".to_string()));
        // Drained: a later emission reports nothing.
        assert_eq!(session.take_deleted(), None);
        // Deleting with nothing visible left reports None.
        let mut empty = session_buffer();
        empty.render("   ", "", false);
        assert_eq!(empty.delete_last_word(), Some("".to_string()));
        assert_eq!(empty.take_deleted(), None);
    }

    #[test]
    fn session_buffer_command_mode_deletion_reports_the_removed_text() {
        let mut session = session_buffer();
        session.render("alpha beta", "", false);
        session.render("alpha beta", "", true); // engage
        assert_eq!(session.render("alpha beta delete word", "", true), "alpha ");
        assert_eq!(session.take_deleted(), Some("beta".to_string()));
        // An ordinary command tick reports nothing.
        assert_eq!(session.render("alpha beta comma", "", true), "alpha ,");
        assert_eq!(session.take_deleted(), None);

        // DeleteLine reports the cleared trailing line.
        let mut lines = session_buffer();
        lines.render("first\nsecond", "", false);
        lines.render("first\nsecond", "", true); // engage
        assert_eq!(
            lines.render("first\nsecond delete line", "", true),
            "first\n"
        );
        assert_eq!(lines.take_deleted(), Some("second".to_string()));
    }

    #[test]
    fn session_buffer_command_modifier_hotkey_deletion_still_works_while_held() {
        // The delete-last-word hotkey edits the same live buffer the
        // command grammar is editing; the two compose.
        let mut session = session_buffer();
        session.render("one two three", "", false);
        session.render("one two three", "", true); // engage
        session.render("one two three comma", "", true);
        assert_eq!(session.delete_last_word(), Some("one two ".to_string()));
        assert_eq!(
            session.render("one two three comma period", "", true),
            "one two ."
        );
    }

    #[test]
    fn session_buffer_command_modifier_resets_between_sessions() {
        let mut session = session_buffer();
        session.render("hello comma", "", true);
        assert!(session.command_active);
        session.end();
        assert!(!session.command_active);

        // A fresh session starts in normal dictation even though the
        // modifier may still be held (the coordinator clears its flag on
        // session end; the buffer forgets on begin/end too).
        session.begin(
            PreviewScript::new(
                ChineseScript::AsTranscribed,
                &OutputLanguageEvidence::Unknown,
            ),
            true,
            true,
            &languages(&["en"]),
            crate::audio_toolkit::command_matrix::default_compiled_matrix(),
            false,
        );
        assert!(!session.command_active);
        assert_eq!(session.render("hello there", "", false), "hello there");
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

    /// The auto-interpretation master gate OFF leaves spoken command words
    /// as plain text in normal dictation: with both per-pass toggles ON and
    /// the terminal pass disabled (it is NOT gated by this toggle), the
    /// input survives the FULL pipeline verbatim; with the terminal pass
    /// left at its default ON the same input gains only the trailing
    /// period, proving the OFF gate touches nothing else. With the gate at
    /// its default ON, the conversion pins above run today's behavior.
    #[test]
    fn auto_interpret_commands_off_types_command_words_as_plain_words() {
        let en = OutputLanguageEvidence::UserSelected("en".to_string());
        let supported = languages(&["en"]);
        let raw = "hello comma scratch that world".to_string();

        let gate_off = AppSettings {
            chinese_script: ChineseScript::AsTranscribed,
            auto_interpret_commands: false,
            terminal_punctuation: false,
            ..Default::default()
        };
        assert_eq!(
            post_process_transcription_text(raw.clone(), &gate_off, false, &en, &supported),
            "hello comma scratch that world"
        );

        let gate_off_terminal_on = AppSettings {
            chinese_script: ChineseScript::AsTranscribed,
            auto_interpret_commands: false,
            ..Default::default()
        };
        assert_eq!(
            post_process_transcription_text(raw, &gate_off_terminal_on, false, &en, &supported),
            "hello comma scratch that world."
        );
    }

    // -----------------------------------------------------------------
    // Hinglish (selected_language "hi-Latn"): Devanagari output is
    // transliterated to Roman, interim and final alike; script
    // conversion, never translation.
    // -----------------------------------------------------------------

    #[test]
    fn hinglish_post_process_transliterates_only_for_the_latin_intent() {
        let hi = OutputLanguageEvidence::UserSelected("hi".to_string());
        let supported = languages(&["hi"]);

        let hinglish = AppSettings {
            chinese_script: ChineseScript::AsTranscribed,
            selected_language: "hi-Latn".to_string(),
            terminal_punctuation: false,
            ..Default::default()
        };
        assert_eq!(
            post_process_transcription_text(
                "नमस्ते दिल्ली".to_string(),
                &hinglish,
                false,
                &hi,
                &supported,
            ),
            "namaste dillee"
        );

        // Without the intent the Devanagari passes through untouched.
        let devanagari = AppSettings {
            chinese_script: ChineseScript::AsTranscribed,
            terminal_punctuation: false,
            ..Default::default()
        };
        assert_eq!(
            post_process_transcription_text(
                "नमस्ते दिल्ली".to_string(),
                &devanagari,
                false,
                &hi,
                &supported,
            ),
            "नमस्ते दिल्ली"
        );
    }

    #[test]
    fn hinglish_interim_render_matches_the_paste() {
        let mut session = StreamSessionBuffer::default();
        session.begin(
            PreviewScript::new(
                ChineseScript::AsTranscribed,
                &OutputLanguageEvidence::UserSelected("hi".to_string()),
            ),
            true,
            true,
            &languages(&["hi"]),
            crate::audio_toolkit::command_matrix::default_compiled_matrix(),
            true,
        );
        // The overlay shows Roman while speaking...
        assert_eq!(session.render("नमस्ते", "", false), "namaste");
        assert_eq!(session.render("नमस्ते दिल्ली", "", false), "namaste dillee");
        // ...and the finalize pipeline pastes the same words.
        let final_raw = session.combine_final("नमस्ते दिल्ली".to_string());
        let settings = AppSettings {
            chinese_script: ChineseScript::AsTranscribed,
            selected_language: "hi-Latn".to_string(),
            terminal_punctuation: false,
            ..Default::default()
        };
        assert_eq!(
            post_process_transcription_text(
                final_raw,
                &settings,
                false,
                &OutputLanguageEvidence::UserSelected("hi".to_string()),
                &languages(&["hi"]),
            ),
            "namaste dillee"
        );
    }

    /// Spoken "new line" must survive the FULL finalize pipeline, not just the
    /// spoken-punctuation pass: the custom-words stage runs on the DEFAULT
    /// configuration (the dictionary seed is non-empty out of the box) and
    /// used to flatten the break with its whitespace-token rebuild. Terminal
    /// punctuation is off to isolate the newline outcome.
    #[test]
    fn newline_phrase_survives_the_full_pipeline_including_custom_words() {
        let settings = AppSettings {
            chinese_script: ChineseScript::AsTranscribed,
            terminal_punctuation: false,
            ..Default::default()
        };
        let en = OutputLanguageEvidence::UserSelected("en".to_string());
        let supported = languages(&["en"]);

        let result = post_process_transcription_text(
            "line one new line line two".to_string(),
            &settings,
            false,
            &en,
            &supported,
        );
        assert_eq!(result, "line one\nline two");
    }

    /// A trailing "new line" keeps its final break through the whole
    /// pipeline under the same settings (the whitespace cleanup retains a
    /// trailing newline run instead of trimming it away).
    #[test]
    fn trailing_newline_phrase_keeps_its_break_through_the_pipeline() {
        let settings = AppSettings {
            chinese_script: ChineseScript::AsTranscribed,
            terminal_punctuation: false,
            ..Default::default()
        };
        let en = OutputLanguageEvidence::UserSelected("en".to_string());
        let supported = languages(&["en"]);

        let result = post_process_transcription_text(
            "line one new line".to_string(),
            &settings,
            false,
            &en,
            &supported,
        );
        assert_eq!(result, "line one\n");
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
            "one two three delete everything four five".to_string(),
            &settings,
            false,
            &en,
            &supported,
        );
        assert_eq!(restarted, "");
        // "start over" left the default ClearAll table: ordinary dictation
        // saying it keeps its sentence (terminal punctuation still lands).
        let kept = post_process_transcription_text(
            "let me start over and try again".to_string(),
            &settings,
            false,
            &en,
            &supported,
        );
        assert_eq!(kept, "let me start over and try again.");
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

        let candidates = fallback_candidate_list(&models, "selected", "auto");

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
        let candidates = fallback_candidate_list(&[added], "selected", "auto");

        // Plenty free: the user-added model is selected.
        assert_eq!(
            memory::resolve_fallback_model(
                Some(8 * GIB),
                &candidates,
                "selected",
                memory::DEFAULT_HEADROOM_BYTES
            )
            .map(|c| c.id.as_str()),
            Some("org/custom-asr/model-Q8_0.gguf")
        );
        // Too tight for it (700 MiB + 1.5 GiB headroom > 2 GiB): nothing.
        assert_eq!(
            memory::resolve_fallback_model(
                Some(2 * GIB),
                &candidates,
                "selected",
                memory::DEFAULT_HEADROOM_BYTES
            ),
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
        let candidates =
            fallback_candidate_list(&[added.clone(), catalog.clone()], "selected", "auto");

        // Both fit, the ranked catalog entry wins...
        assert_eq!(
            memory::resolve_fallback_model(
                Some(16 * GIB),
                &candidates,
                "selected",
                memory::DEFAULT_HEADROOM_BYTES
            )
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
        let forced = fallback_candidate_list(&[huge_catalog, huge], "selected", "auto");
        assert_eq!(
            memory::resolve_fallback_model(
                Some(8 * GIB),
                &forced,
                "selected",
                memory::DEFAULT_HEADROOM_BYTES
            )
            .map(|c| c.id.as_str()),
            Some("org/custom-asr/model-Q8_0.gguf")
        );
    }

    #[test]
    fn language_aware_fallback_refuses_rather_than_swapping_languages() {
        const GIB: u64 = 1024 * 1024 * 1024;
        // The failed model is the downloaded Hindi model; the only other
        // downloaded candidate is English-only.
        let mut hi_model = model_info_for("hi-model", 4000, true);
        hi_model.supported_languages = vec!["hi".to_string()];
        let mut en_model = model_info_for("en-model", 100, true);
        en_model.supported_languages = vec!["en".to_string()];

        let candidates =
            fallback_candidate_list(&[hi_model.clone(), en_model.clone()], "hi-model", "hi");
        assert!(
            candidates.is_empty(),
            "an English-only model must never be offered for a Hindi intent"
        );

        // Nothing fits the language: the gate refuses instead of swapping
        // (decide_memory_gate itself is unchanged; the empty candidate
        // list drives the Refuse).
        let decision = decide_memory_gate(
            true,
            true,
            true,
            Some(2 * GIB),
            5 * GIB,
            memory::DEFAULT_HEADROOM_BYTES,
            || candidates,
            "hi-model",
        );
        assert_eq!(decision, MemoryGateDecision::Refuse);

        // A downloaded Hindi-capable candidate that fits IS offered.
        let mut hi_fallback = model_info_for("hi-small", 100, true);
        hi_fallback.supported_languages = vec!["en".to_string(), "hi".to_string()];
        let candidates =
            fallback_candidate_list(&[hi_model, en_model, hi_fallback], "hi-model", "hi");
        let decision = decide_memory_gate(
            true,
            true,
            true,
            Some(2 * GIB),
            5 * GIB,
            memory::DEFAULT_HEADROOM_BYTES,
            || candidates,
            "hi-model",
        );
        assert_eq!(
            decision,
            MemoryGateDecision::Fallback("hi-small".to_string())
        );
    }

    /// AUDIT TEST 3: with whisper tiny as the ONLY downloaded model, a gate
    /// refusal is terminal - the fallback inventory is empty by construction
    /// (the failed model is excluded from its own candidate list), so the
    /// decision is Refuse, not Fallback. This is the backend shape of the
    /// v1.0.2 first-run dead end; the recovery surface is the onboarding
    /// refusal card.
    #[test]
    fn single_downloaded_model_refusal_is_terminal() {
        let whisper_tiny = model_info_for("whisper-tiny-q8", 43, true);
        let forecast = 45_088_768_u64; // size_mb 43 x 1 MiB
        assert_eq!(
            whisper_tiny.size_mb.saturating_mul(1024 * 1024),
            forecast,
            "fixture must be whisper tiny's real forecast"
        );
        let decision = decide_memory_gate(
            true,
            true,
            true,
            Some(1_087_373_312), // the audit's WARN probe reading
            forecast,
            memory::DEFAULT_HEADROOM_BYTES, // the retired fixed margin: refuses
            || fallback_candidate_list(&[whisper_tiny], "whisper-tiny-q8", "auto"),
            "whisper-tiny-q8",
        );
        assert_eq!(
            decision,
            MemoryGateDecision::Refuse,
            "only the failed model on disk means no fallback can exist"
        );
    }

    /// AUDIT TEST 4: the refusal message is the exact string users see, and
    /// it must never render a 43 MiB forecast as "needs ~0.0 GB". The
    /// forecast and free amounts use the MB-below-1-GiB-else-GB rule; the
    /// margin ALWAYS renders in integer MB so the message echoes the unit
    /// of the Advanced UI the user set it in (1536 MB, never "1.5 GB").
    #[test]
    fn refusal_message_formats_tiny_forecast_in_mb() {
        // The amount formatters, pinned to the audit's exact figures.
        assert_eq!(format_memory_amount(45_088_768), "43 MB");
        assert_eq!(format_memory_amount(766_509_056), "731 MB");
        assert_eq!(format_memory_amount(1_610_612_736), "1.5 GB");
        assert_eq!(format_margin_mb(1_610_612_736), "1536 MB");

        // A 43 MiB forecast at margin 0: no "0.0 GB", no margin clause. The
        // friend's free reading (1,087,373,312 B) sits above the 1 GiB
        // boundary, so the amount rule renders it "1.0 GB".
        let msg = memory_gate_refusal_message("Whisper Tiny", 45_088_768, 1_087_373_312, 0);
        assert!(msg.contains("43 MB"), "message must name 43 MB: {msg}");
        assert!(
            msg.contains("1.0 GB"),
            "free renders per the amount rule: {msg}"
        );
        assert!(
            !msg.contains("0.0 GB"),
            "message must not claim 0.0 GB: {msg}"
        );
        assert!(
            !msg.contains("safety margin"),
            "margin 0 has no margin clause: {msg}"
        );

        // The same forecast at the 1536 MiB preset: the margin clause
        // appears with the always-MB formatter.
        let msg = memory_gate_refusal_message(
            "Whisper Tiny",
            45_088_768,
            1_087_373_312,
            memory::DEFAULT_HEADROOM_BYTES,
        );
        assert!(
            msg.contains("1536 MB safety margin"),
            "margin clause must read 1536 MB: {msg}"
        );
        assert!(!msg.contains("1.5 GB safety margin"), "{msg}");

        // The GB side of the amounts: a 731 MiB forecast stays MB, a 1.5
        // GiB free reading renders GB.
        let msg = memory_gate_refusal_message("Parakeet EN Q8", 766_509_056, 1_610_612_736, 0);
        assert!(msg.contains("731 MB"), "{msg}");
        assert!(msg.contains("1.5 GB"), "{msg}");
    }

    #[test]
    fn auto_intent_fallback_candidates_behave_as_before() {
        let mut en_model = model_info_for("en-model", 100, true);
        en_model.supported_languages = vec!["en".to_string()];
        let mut hi_model = model_info_for("hi-model", 100, true);
        hi_model.supported_languages = vec!["hi".to_string()];

        // auto (and empty) intents offer every downloaded candidate,
        // exactly as before the language filter existed.
        assert_eq!(
            fallback_candidate_list(&[en_model.clone(), hi_model.clone()], "selected", "auto")
                .len(),
            2
        );
        assert_eq!(
            fallback_candidate_list(&[en_model.clone(), hi_model.clone()], "selected", "").len(),
            2
        );
        // English intents are not gated either.
        assert_eq!(
            fallback_candidate_list(&[en_model.clone(), hi_model.clone()], "selected", "en").len(),
            2
        );
        // The script subtag base-matches: "hi-Latn" gates like "hi".
        let gated: Vec<String> =
            fallback_candidate_list(&[en_model, hi_model], "selected", "hi-Latn")
                .into_iter()
                .map(|c| c.id)
                .collect();
        assert_eq!(gated, vec!["hi-model".to_string()]);
    }

    // --- Toggle wiring for the memory gate -----------------------------------

    /// Helper: a fallback candidate that always fits the tight fixtures
    /// below (footprint well under a 2 GiB free reading with 1.5 GiB
    /// headroom).
    fn gate_fitting_candidate() -> memory::FallbackCandidate {
        memory::FallbackCandidate {
            id: "whisper-base".to_string(),
            rank: 1,
            footprint_bytes: 150 * 1024 * 1024,
        }
    }

    /// memory_pressure_guard=false bypasses the gate ENTIRELY: the tightest
    /// possible memory state still allows, and the fallback inventory is
    /// never even consulted (proven by the flag the closure sets).
    #[test]
    fn memory_gate_bypasses_entirely_when_guard_off() {
        let consulted = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&consulted);
        let decision = decide_memory_gate(
            false,                  // memory_pressure_guard = false
            true,                   // auto_fallback = true
            true,                   // top-level load (cascades allowed)
            Some(1),                // one byte free
            8 * 1024 * 1024 * 1024, // 8 GiB forecast: would always refuse
            memory::DEFAULT_HEADROOM_BYTES,
            move || {
                flag.store(true, Ordering::SeqCst);
                vec![gate_fitting_candidate()]
            },
            "selected",
        );
        assert_eq!(decision, MemoryGateDecision::Allow);
        assert!(
            !consulted.load(Ordering::SeqCst),
            "guard off must not even inventory fallback candidates"
        );
    }

    /// auto_fallback=false never swaps models: a refusal stays a refusal
    /// even with a fitting already-downloaded candidate on disk.
    #[test]
    fn memory_gate_never_swaps_models_when_auto_fallback_off() {
        let decision = decide_memory_gate(
            true,  // memory_pressure_guard = true
            false, // auto_fallback = false
            true,
            Some(2 * 1024 * 1024 * 1024), // 2 GiB free
            8 * 1024 * 1024 * 1024,       // 8 GiB forecast: refuses
            memory::DEFAULT_HEADROOM_BYTES,
            || vec![gate_fitting_candidate()],
            "selected",
        );
        assert_eq!(decision, MemoryGateDecision::Refuse);
    }

    /// The fallback load's own re-entry (allow_fallback=false, the
    /// no-cascade guard) may not fall back again even with auto_fallback
    /// on and a second candidate fitting.
    #[test]
    fn memory_gate_fallback_never_cascades() {
        let decision = decide_memory_gate(
            true,
            true,
            false, // the fallback's own load
            Some(2 * 1024 * 1024 * 1024),
            8 * 1024 * 1024 * 1024,
            memory::DEFAULT_HEADROOM_BYTES,
            || vec![gate_fitting_candidate()],
            "selected",
        );
        assert_eq!(decision, MemoryGateDecision::Refuse);
    }

    /// The engaged gate's happy paths: a fitting forecast allows, a refused
    /// one falls back to the best fitting candidate, an unavailable probe
    /// fails open, and a refusal with nothing fitting stays a refusal.
    #[test]
    fn memory_gate_allows_fits_falls_back_and_fails_open() {
        let fitting = 700 * 1024 * 1024;
        // Plenty free: allowed.
        assert_eq!(
            decide_memory_gate(
                true,
                true,
                true,
                Some(16 * 1024 * 1024 * 1024),
                fitting,
                memory::DEFAULT_HEADROOM_BYTES,
                || vec![],
                "selected"
            ),
            MemoryGateDecision::Allow
        );
        // Tight free (700 MiB + 1.5 GiB headroom > 2 GiB): falls back.
        assert_eq!(
            decide_memory_gate(
                true,
                true,
                true,
                Some(2 * 1024 * 1024 * 1024),
                fitting,
                memory::DEFAULT_HEADROOM_BYTES,
                || vec![gate_fitting_candidate()],
                "selected"
            ),
            MemoryGateDecision::Fallback("whisper-base".to_string())
        );
        // Probe unavailable: fails open exactly like the gate predicate.
        assert_eq!(
            decide_memory_gate(
                true,
                true,
                true,
                None,
                8 * 1024 * 1024 * 1024,
                memory::DEFAULT_HEADROOM_BYTES,
                || vec![],
                "selected"
            ),
            MemoryGateDecision::Allow
        );
        // Refused and nothing fits (or nothing downloaded): refusal.
        assert_eq!(
            decide_memory_gate(
                true,
                true,
                true,
                Some(2 * 1024 * 1024 * 1024),
                fitting,
                memory::DEFAULT_HEADROOM_BYTES,
                || vec![],
                "selected"
            ),
            MemoryGateDecision::Refuse
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
