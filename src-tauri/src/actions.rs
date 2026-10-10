#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use crate::apple_intelligence;
use crate::audio_feedback::{play_feedback_sound, play_feedback_sound_blocking, SoundType};
use crate::audio_toolkit::{is_microphone_access_denied, is_no_input_device_error, VadPolicy};
use crate::managers::audio::AudioRecordingManager;
use crate::managers::history::HistoryManager;
use crate::managers::model::ModelManager;
use crate::managers::transcription::{StreamTextEvent, StreamWorkKind, TranscriptionManager};
use crate::settings::{
    get_settings, AppSettings, ModelUnloadTimeout, OverlayStyle, APPLE_INTELLIGENCE_PROVIDER_ID,
};
use crate::shortcut;
use crate::tray::{set_tray_state, TrayIconState};
use crate::utils::{
    self, show_processing_overlay, show_recording_overlay, show_transcribing_overlay,
};
use crate::TranscriptionCoordinator;
use log::{debug, error, info, warn};
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::Manager;
use tauri::{AppHandle, Emitter};
use tauri_specta::Event as _;

const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// How long the final-text preview stays on screen before the paste fires
/// when `preview_before_paste` is enabled.
const PREVIEW_BEFORE_PASTE_DELAY: Duration = Duration::from_millis(1200);

#[derive(Clone, serde::Serialize)]
struct RecordingErrorEvent {
    error_type: String,
    detail: Option<String>,
}

/// Drop guard that finishes the transcription pipeline, including immediate
/// model unloading on early exits.
struct FinishGuard(AppHandle, Arc<TranscriptionManager>);
impl Drop for FinishGuard {
    fn drop(&mut self) {
        // The cancel shortcut stays registered for the WHOLE pipeline
        // (Recording through Processing: finalize, batch, post-process,
        // paste). It used to be unregistered the moment stop() fired, which
        // left a wedged post-process request with no keyboard escape
        // (the handler gate also required a live recording), so only the
        // tray Cancel could break out. Unregistering here, at the true end
        // of the pipeline, keeps Escape alive the entire time.
        crate::shortcut::unregister_cancel_shortcut(&self.0);
        self.1.maybe_unload_immediately("transcription session");
        if let Some(c) = self.0.try_state::<TranscriptionCoordinator>() {
            c.notify_processing_finished();
        }
        // The pipeline just freed its large transient buffers (captured PCM,
        // WAV copy, engine scratch); hand the cached pages back to the OS so
        // they don't sit in malloc arenas until they get swapped out (#1792).
        crate::memory::trim_freed_memory();
    }
}

// Shortcut Action Trait
pub trait ShortcutAction: Send + Sync {
    fn start(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str);
    fn stop(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str);
}

// Transcribe Action
struct TranscribeAction {
    post_process: bool,
}

/// Pure routing table from a transcribe binding id to its
/// [`TranscribeAction`] configuration: `post_process`.
///
/// Extracted from the ACTION_MAP literals so a test can pin the safety
/// property the operator relies on: only the dictation bindings
/// ("transcribe", "transcribe_with_post_process") route here. The
/// command-mode binding ("transcribe_commands") is NOT a recording action
/// at all - it is a during-dictation modifier routed to the coordinator's
/// `send_command_modifier`, so no binding id can ever start a command
/// capture recording.
fn transcribe_action_config(binding_id: &str) -> Option<bool> {
    match binding_id {
        "transcribe" => Some(false),
        "transcribe_with_post_process" => Some(true),
        _ => None,
    }
}

/// Field name for structured output JSON schema. Shared with the local
/// post-process engine, whose worker output is parsed against the same
/// schema.
pub(crate) const TRANSCRIPTION_FIELD: &str = "transcription";

/// Strip invisible Unicode characters that some LLMs may insert
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

/// Build a system prompt from the user's prompt template.
/// Removes `${output}` placeholder since the transcription is sent as the user message.
fn build_system_prompt(prompt_template: &str) -> String {
    prompt_template.replace("${output}", "").trim().to_string()
}

/// Returns `true` when a transcription has no meaningful content to
/// post-process (empty or whitespace-only). Used to skip the post-processing
/// LLM call when nothing was actually transcribed, which would otherwise make
/// the model reply with an error message such as "you need to provide the
/// transcription".
fn is_blank_transcription(transcription: &str) -> bool {
    transcription.trim().is_empty()
}

async fn complete_unless_cancelled<F, C>(operation: F, is_cancelled: C) -> Option<F::Output>
where
    F: Future,
    C: Fn() -> bool,
{
    tokio::pin!(operation);

    loop {
        if is_cancelled() {
            return None;
        }

        if let Ok(result) =
            tokio::time::timeout(CANCELLATION_POLL_INTERVAL, operation.as_mut()).await
        {
            return Some(result);
        }
    }
}

fn should_use_streaming_overlay(style: OverlayStyle, is_streaming: bool) -> bool {
    style == OverlayStyle::Live && is_streaming
}

/// What the stop pipeline does when a stream finalize produced no usable
/// text (`None`, or a blank/whitespace-only string). Extracted so the
/// decision is unit-testable.
enum EmptyFinalizeOutcome {
    /// Transcribe the captured samples in batch: the genuine fallback for
    /// stream failures and every ordinary configuration.
    Batch,
    /// Return the quiet empty result. With
    /// [`ModelUnloadTimeout::Immediately`] the stream's finalize already
    /// unloaded the model (a routine silent sub-second tap finalizes to
    /// empty because the audio is zero-padded to 1.25s), so a batch attempt
    /// dies at transcribe_audio's "Model is not loaded" check and turned
    /// the tap into an error toast plus a failed history entry.
    QuietEmpty,
}

/// The pure decision: `finalize outcome x unload timeout -> batch | quiet
/// empty`. `model_loaded` is the residency observed AFTER finalize_stream
/// returned (Immediately's unload reads as unloaded at once).
fn empty_finalize_outcome(unload_is_immediately: bool, model_loaded: bool) -> EmptyFinalizeOutcome {
    if unload_is_immediately && !model_loaded {
        EmptyFinalizeOutcome::QuietEmpty
    } else {
        EmptyFinalizeOutcome::Batch
    }
}

/// The per-dictation outcome summary (pure formatter, unit-tested): the
/// stop pipeline logs ONE such line at its true end, so voxbar.log can
/// answer the end-to-end questions the usage survey could not (what the
/// session did, from key release to paste).
pub(crate) struct SessionOutcomeSummary {
    pub binding: String,
    pub audio_seconds: f64,
    pub sample_count: usize,
    /// "stream-finalize" | "batch" | "quiet-empty" | "failed".
    pub source: &'static str,
    /// "off" | "processed" | "raw-fallback" | "failed".
    pub post_process: &'static str,
    /// "ok" | "failed" | "skipped-empty" | "dispatch-failed" | "error".
    pub paste: &'static str,
    /// Key release (stop) to the pipeline's true end, in milliseconds.
    pub stop_to_end_ms: u128,
}

pub(crate) fn format_session_outcome(summary: &SessionOutcomeSummary) -> String {
    format!(
        "dictation outcome: binding={} audio={:.2}s samples={} source={} post_process={} paste={} stop_to_end={}ms",
        summary.binding,
        summary.audio_seconds,
        summary.sample_count,
        summary.source,
        summary.post_process,
        summary.paste,
        summary.stop_to_end_ms,
    )
}

/// Whether this provider id routes to the local on-device engine (T28's
/// routing predicate, extracted so the decision is testable and the branch
/// binds to it).
pub(crate) fn uses_local_engine(provider_id: &str) -> bool {
    provider_id == crate::settings::LOCAL_LLM_PROVIDER_ID
}

/// The local branch's availability decision (T28): the pinned model must
/// be downloaded before the engine can run; when it is not, the branch
/// skips with the download_missing reason and the raw transcript is used.
pub(crate) fn local_engine_availability(
    model_downloaded: bool,
) -> Option<(crate::local_llm::SkipReason, Option<String>)> {
    if model_downloaded {
        None
    } else {
        Some((
            crate::local_llm::SkipReason::DownloadMissing,
            Some("the post-process model is not downloaded; download it in Settings".to_string()),
        ))
    }
}

/// The structured-output JSON schema for post-processing. Extracted
/// verbatim from the API path so the local engine's grammar is generated
/// from exactly the same contract (T28 pins the identity).
pub(crate) fn post_process_output_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            (TRANSCRIPTION_FIELD): {
                "type": "string",
                "description": "The cleaned and processed transcription text"
            }
        },
        "required": [TRANSCRIPTION_FIELD],
        "additionalProperties": false
    })
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
/// off it) plus a human diagnostic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PostProcessValidationFailure {
    pub skip_reason: crate::local_llm::SkipReason,
    pub detail: String,
}

/// THE shared output validator: every post-process success path (cloud
/// structured, cloud legacy, Apple Intelligence, the local engine) routes
/// its raw output through here before any `Some(..)` can be returned, so
/// the never-lose-the-transcript rule is enforced in exactly one place.
///
/// Structured mode strips the think belt and invisibles, parses the JSON,
/// extracts the transcription field, and strips again; free-text mode
/// strips only. Both modes then apply the CJK-aware fidelity guard against
/// the transcript. An empty result always fails. Every failure means the
/// caller must fall back to the raw transcript.
pub(crate) fn validate_post_process_output(
    transcript: &str,
    generated: &str,
    mode: PostProcessOutputMode,
) -> Result<String, PostProcessValidationFailure> {
    let invalid = |detail: String| PostProcessValidationFailure {
        skip_reason: crate::local_llm::SkipReason::EngineFailed,
        detail,
    };
    let extracted = match mode {
        PostProcessOutputMode::StructuredJson => {
            let content = strip_invisible_chars(strip_think_block(generated));
            match serde_json::from_str::<serde_json::Value>(&content) {
                Ok(json) => match json.get(TRANSCRIPTION_FIELD).and_then(|t| t.as_str()) {
                    Some(field) => strip_invisible_chars(strip_think_block(field)),
                    None => {
                        return Err(invalid(
                            "the model output had no transcription field".to_string(),
                        ))
                    }
                },
                Err(_) => {
                    return Err(invalid("the model output was not valid JSON".to_string()));
                }
            }
        }
        PostProcessOutputMode::FreeText => strip_invisible_chars(strip_think_block(generated)),
    };
    if extracted.trim().is_empty() {
        return Err(invalid("the model output was empty".to_string()));
    }
    if crate::local_llm::forecast::fails_fidelity_guard(transcript, &extracted) {
        return Err(PostProcessValidationFailure {
            skip_reason: crate::local_llm::SkipReason::LengthGuard,
            detail: "the cleaned text collapsed below the fidelity threshold".to_string(),
        });
    }
    Ok(extracted)
}

/// The result of one post-process attempt: the processed text (None means
/// the raw transcript must be used) plus the run summary for history.
#[derive(Default)]
pub(crate) struct PostProcessAttempt {
    pub text: Option<String>,
    pub summary: Option<crate::post_process_runs::PostProcessRunSummary>,
}

/// The injectable seam for cloud completions: production binds llm_client;
/// tests bind canned responses so the full requested->engine->generation->
/// outcome lifecycle (and its failure classes) is pinnable without a
/// network, a provider, or a Tauri app.
pub(crate) type CloudSendResult = Result<Option<String>, String>;

pub(crate) trait CloudCompletionSeam: Send {
    fn send(
        &mut self,
        user_content: String,
        system_prompt: Option<String>,
        json_schema: Option<serde_json::Value>,
    ) -> std::pin::Pin<Box<dyn Future<Output = CloudSendResult> + Send + '_>>;
}

/// The production seam: one `send` per attempt, carrying everything the
/// provider needs; `send_chat_completion` (legacy) is the schema-less
/// shape of the same call.
struct LlmClientSeam {
    provider: crate::settings::PostProcessProvider,
    api_key: String,
    model: String,
    disable_reasoning: bool,
    timeout_secs: u64,
}

impl CloudCompletionSeam for LlmClientSeam {
    fn send(
        &mut self,
        user_content: String,
        system_prompt: Option<String>,
        json_schema: Option<serde_json::Value>,
    ) -> std::pin::Pin<Box<dyn Future<Output = CloudSendResult> + Send + '_>> {
        let provider = self.provider.clone();
        let api_key = self.api_key.clone();
        let model = self.model.clone();
        let disable_reasoning = self.disable_reasoning;
        let timeout_secs = self.timeout_secs;
        Box::pin(async move {
            crate::llm_client::send_chat_completion_with_schema(
                &provider,
                api_key,
                &model,
                user_content,
                system_prompt,
                json_schema,
                disable_reasoning,
                timeout_secs,
            )
            .await
        })
    }
}

/// The cloud/Apple lifecycle after the `requested` phase was already
/// emitted by the caller: engine phase (placeholders; no local swap), one
/// structured attempt with a legacy retry, validation on every success,
/// classified outcome on every failure. Returns the processed text or None
/// (the raw transcript always wins on failure).
async fn run_cloud_lifecycle(
    app: Option<&AppHandle>,
    seam: &mut dyn CloudCompletionSeam,
    run_id: u64,
    transcript: &str,
    system_prompt: Option<String>,
    structured_supported: bool,
    legacy_prompt: String,
    notify: impl Fn(crate::managers::transcription::NoticeCode, Option<String>),
) -> Option<String> {
    use crate::post_process_runs::{runs, PostProcessOutcome};
    // Engine phase: cloud engines have no model load, no cache, no swap.
    runs().engine_phase(app, run_id, None, None);

    let generation_started = Instant::now();
    let mut retries: u32 = 0;

    // ---- Structured attempt (when the provider supports it) ----
    if structured_supported {
        let json_schema = post_process_output_schema();
        match seam
            .send(
                transcript.to_string(),
                system_prompt.clone(),
                Some(json_schema),
            )
            .await
        {
            Ok(Some(content)) => {
                // THE shared validator: nothing reaches the paste without
                // passing it. A validation failure falls back to the raw
                // transcript and says so through the notice channel.
                return match validate_post_process_output(
                    transcript,
                    &content,
                    PostProcessOutputMode::StructuredJson,
                ) {
                    Ok(result) => {
                        let ms = generation_started.elapsed().as_millis() as u64;
                        runs().generation_phase(app, run_id, Some(ms), Some(retries));
                        runs().finish(
                            app,
                            run_id,
                            PostProcessOutcome::Applied,
                            Some(result.len() as u64),
                        );
                        debug!(
                            "Structured output post-processing succeeded. Output length: {} chars",
                            result.len()
                        );
                        Some(result)
                    }
                    Err(failure) => {
                        let ms = generation_started.elapsed().as_millis() as u64;
                        runs().generation_phase(app, run_id, Some(ms), Some(retries));
                        runs().finish(
                            app,
                            run_id,
                            PostProcessOutcome::Failed {
                                class: crate::llm_client::PostProcessFailureClass::OutputInvalid,
                            },
                            None,
                        );
                        warn!(
                            "Post-process output failed validation: {}. Using the raw transcript.",
                            failure.detail
                        );
                        notify(
                            crate::managers::transcription::NoticeCode::PostProcessOutputInvalid,
                            Some(failure.detail.clone()),
                        );
                        None
                    }
                };
            }
            Ok(None) => {
                let ms = generation_started.elapsed().as_millis() as u64;
                runs().generation_phase(app, run_id, Some(ms), Some(retries));
                runs().finish(
                    app,
                    run_id,
                    PostProcessOutcome::Failed {
                        class: crate::llm_client::PostProcessFailureClass::OutputInvalid,
                    },
                    None,
                );
                warn!("Post-process failed: the API response had no content");
                notify(
                    crate::managers::transcription::NoticeCode::PostProcessCloudFailed,
                    Some("the API response had no content".to_string()),
                );
                return None;
            }
            Err(e) => {
                // Fall through to the legacy attempt; this counts as the
                // one retry (today's behavior, now visible in the record).
                retries += 1;
                warn!(
                    "Structured output failed: {}. Falling back to legacy mode.",
                    e
                );
            }
        }
    }

    // ---- Legacy attempt (the retry, or the only mode) ----
    debug!("Processed prompt length: {} chars", legacy_prompt.len());
    match seam.send(legacy_prompt, None, None).await {
        Ok(Some(content)) => match validate_post_process_output(
            transcript,
            &content,
            PostProcessOutputMode::FreeText,
        ) {
            Ok(result) => {
                let ms = generation_started.elapsed().as_millis() as u64;
                runs().generation_phase(app, run_id, Some(ms), Some(retries));
                runs().finish(
                    app,
                    run_id,
                    PostProcessOutcome::Applied,
                    Some(result.len() as u64),
                );
                debug!(
                    "LLM post-processing succeeded. Output length: {} chars",
                    result.len()
                );
                Some(result)
            }
            Err(failure) => {
                let ms = generation_started.elapsed().as_millis() as u64;
                runs().generation_phase(app, run_id, Some(ms), Some(retries));
                runs().finish(
                    app,
                    run_id,
                    PostProcessOutcome::Failed {
                        class: crate::llm_client::PostProcessFailureClass::OutputInvalid,
                    },
                    None,
                );
                warn!(
                    "Post-process output failed validation: {}. Using the raw transcript.",
                    failure.detail
                );
                notify(
                    crate::managers::transcription::NoticeCode::PostProcessOutputInvalid,
                    Some(failure.detail.clone()),
                );
                None
            }
        },
        Ok(None) => {
            let ms = generation_started.elapsed().as_millis() as u64;
            runs().generation_phase(app, run_id, Some(ms), Some(retries));
            runs().finish(
                app,
                run_id,
                PostProcessOutcome::Failed {
                    class: crate::llm_client::PostProcessFailureClass::OutputInvalid,
                },
                None,
            );
            warn!("Post-process failed: the API response had no content");
            notify(
                crate::managers::transcription::NoticeCode::PostProcessCloudFailed,
                Some("the API response had no content".to_string()),
            );
            None
        }
        Err(e) => {
            let class = crate::post_process_runs::classify_cloud_failure(&e);
            let ms = generation_started.elapsed().as_millis() as u64;
            runs().generation_phase(app, run_id, Some(ms), Some(retries));
            runs().finish(app, run_id, PostProcessOutcome::Failed { class }, None);
            warn!(
                "LLM post-processing failed ({}): {}. Using the raw transcript.",
                crate::post_process_runs::failure_class_str(class),
                e
            );
            notify(
                crate::managers::transcription::NoticeCode::PostProcessCloudFailed,
                Some(e),
            );
            None
        }
    }
}

async fn post_process_transcription(
    app: &AppHandle,
    settings: &AppSettings,
    binding: &str,
    transcription: &str,
    is_cancelled: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
) -> PostProcessAttempt {
    if is_blank_transcription(transcription) {
        debug!("Post-processing skipped because the transcription is empty");
        return PostProcessAttempt::default();
    }

    let provider = match settings.active_post_process_provider().cloned() {
        Some(provider) => provider,
        None => {
            debug!("Post-processing enabled but no provider is selected");
            return PostProcessAttempt::default();
        }
    };

    let model = settings
        .post_process_models
        .get(&provider.id)
        .cloned()
        .unwrap_or_default();

    if model.trim().is_empty() {
        debug!(
            "Post-processing skipped because provider '{}' has no model configured",
            provider.id
        );
        return PostProcessAttempt::default();
    }

    let selected_prompt_id = match &settings.post_process_selected_prompt_id {
        Some(id) => id.clone(),
        None => {
            debug!("Post-processing skipped because no prompt is selected");
            return PostProcessAttempt::default();
        }
    };

    let prompt_entry = match settings
        .post_process_prompts
        .iter()
        .find(|prompt| prompt.id == selected_prompt_id)
    {
        Some(prompt) => prompt.clone(),
        None => {
            debug!(
                "Post-processing skipped because prompt '{}' was not found",
                selected_prompt_id
            );
            return PostProcessAttempt::default();
        }
    };

    if prompt_entry.prompt.trim().is_empty() {
        debug!("Post-processing skipped because the selected prompt is empty");
        return PostProcessAttempt::default();
    }

    // The lifecycle starts here: a resolvable request (provider, model,
    // prompt) exists, so the run gets its id, its requested line, and its
    // record. The config-level guards above are pre-run states, not runs.
    let engine_kind = if uses_local_engine(&provider.id) {
        crate::post_process_runs::PostProcessEngineKind::Local
    } else if provider.id == APPLE_INTELLIGENCE_PROVIDER_ID {
        crate::post_process_runs::PostProcessEngineKind::AppleIntelligence
    } else {
        crate::post_process_runs::PostProcessEngineKind::Cloud
    };
    let run_id = crate::post_process_runs::runs().begin(
        Some(app),
        crate::post_process_runs::RunRequestMeta {
            binding: binding.to_string(),
            engine: engine_kind,
            provider_id: provider.id.clone(),
            model: model.clone(),
            prompt_id: Some(prompt_entry.id.clone()),
            prompt_name: Some(prompt_entry.name.clone()),
            // The prompt library's version/language tokens land here when
            // WS4's versioned prompts ship; the fields exist now so the
            // record schema is stable across that change.
            prompt_version: None,
            template_language: None,
            chars_in: transcription.chars().count() as u64,
        },
    );
    let summarize = |run_id: u64| build_run_summary(run_id, &provider.id, &model, &prompt_entry.id);

    debug!(
        "Starting LLM post-processing with provider '{}' (model: {})",
        provider.id, model
    );

    let api_key = settings
        .post_process_api_keys
        .get(&provider.id)
        .cloned()
        .unwrap_or_default();

    // Ask these providers to skip reasoning/thinking - post-processing rarely
    // benefits from it and it adds seconds of latency. llm_client picks the
    // field the endpoint understands and retries without it if rejected.
    let disable_reasoning = matches!(provider.id.as_str(), "custom" | "openrouter");

    let attempt = if uses_local_engine(&provider.id) {
        run_local_lifecycle(
            app,
            run_id,
            settings,
            transcription,
            &prompt_entry.prompt,
            is_cancelled,
        )
        .await
    } else if provider.id == APPLE_INTELLIGENCE_PROVIDER_ID {
        run_apple_intelligence_lifecycle(app, run_id, transcription, &prompt_entry.prompt, &model)
            .await
    } else {
        let system_prompt = if provider.supports_structured_output {
            Some(build_system_prompt(&prompt_entry.prompt))
        } else {
            None
        };
        let legacy_prompt = prompt_entry.prompt.replace("${output}", transcription);
        let notify = |code: crate::managers::transcription::NoticeCode, detail: Option<String>| {
            crate::managers::transcription::emit_overlay_notice(app, code, detail);
        };
        let mut seam = LlmClientSeam {
            provider: provider.clone(),
            api_key,
            model: model.clone(),
            disable_reasoning,
            timeout_secs: settings.post_process_timeout_secs,
        };
        run_cloud_lifecycle(
            Some(app),
            &mut seam,
            run_id,
            transcription,
            system_prompt,
            provider.supports_structured_output,
            legacy_prompt,
            notify,
        )
        .await
    };

    PostProcessAttempt {
        summary: summarize(run_id),
        text: attempt,
    }
}

/// Build the per-run summary history persists. Reads the finished record so
/// the outcome token and latency are the same ones the Debug table shows.
fn build_run_summary(
    run_id: u64,
    provider_id: &str,
    model: &str,
    prompt_id: &str,
) -> Option<crate::post_process_runs::PostProcessRunSummary> {
    let record = crate::post_process_runs::runs().snapshot(run_id)?;
    Some(crate::post_process_runs::PostProcessRunSummary {
        provider_id: provider_id.to_string(),
        model: model.to_string(),
        prompt_id: prompt_id.to_string(),
        outcome: record.outcome?.token(),
        latency_ms: record.total_ms.unwrap_or(0),
    })
}

/// The local on-device engine: the same branch shape as Apple Intelligence
/// (availability check, then the engine), but through the exclusive swap:
/// voice out (waited), LLM in, generate, LLM out (waited), voice restore.
/// Every failure returns None so the raw transcript is pasted; the runner
/// reports the engine/generation phases and concludes the run's outcome
/// itself (skips through the single skip sink, applied/raw at its terminal).
async fn run_local_lifecycle(
    app: &AppHandle,
    run_id: u64,
    _settings: &AppSettings,
    transcription: &str,
    prompt: &str,
    is_cancelled: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
) -> Option<String> {
    let downloaded = app
        .state::<Arc<ModelManager>>()
        .get_model_info(crate::local_llm::LOCAL_LLM_MODEL_ID)
        .is_some_and(|info| info.is_downloaded);
    if let Some((reason, detail)) = local_engine_availability(downloaded) {
        debug!("Local post-process unavailable; using the raw transcript");
        crate::local_llm::manager::emit_post_process_skip(app, Some(run_id), reason, detail);
        return None;
    }

    let system_prompt = build_system_prompt(prompt);
    let user_content = transcription.to_string();

    // The grammar is rendered from the exact schema the API path
    // uses, so both engines answer to the same contract.
    let grammar =
        match llama_cpp_2::json_schema_to_grammar(&post_process_output_schema().to_string()) {
            Ok(gbnf) => Some(gbnf),
            Err(e) => {
                warn!(
                    "Failed to render the post-process grammar: {}. Using the raw transcript.",
                    e
                );
                crate::post_process_runs::runs().finish(
                    Some(app),
                    run_id,
                    crate::post_process_runs::PostProcessOutcome::Failed {
                        class: crate::llm_client::PostProcessFailureClass::OutputInvalid,
                    },
                    None,
                );
                return None;
            }
        };

    let request = crate::local_llm::manager::SwapRequest {
        transcript: user_content,
        system_prompt,
        grammar,
        is_cancelled,
        run_id: Some(run_id),
    };
    let llm = app.state::<Arc<crate::local_llm::manager::LlmManager>>();
    // The runner is detached and bounded; awaiting the receiver
    // can be dropped at any instant without abandoning it (L6).
    let outcome = llm.run_swap(app, request).await;
    match outcome {
        Ok(crate::local_llm::manager::SwapOutcome::Processed(text)) => {
            // The runner already validated the structured output; this is
            // the final belt against a post-validation empty strip.
            let text = strip_invisible_chars(strip_think_block(&text));
            if text.trim().is_empty() {
                debug!("Local post-processing returned an empty response");
                crate::post_process_runs::runs().finish(
                    Some(app),
                    run_id,
                    crate::post_process_runs::PostProcessOutcome::Failed {
                        class: crate::llm_client::PostProcessFailureClass::OutputInvalid,
                    },
                    None,
                );
                None
            } else {
                debug!(
                    "Local post-processing succeeded. Output length: {} chars",
                    text.len()
                );
                Some(text)
            }
        }
        _ => {
            // The runner concluded the run (skip sink or terminal mapping).
            // If a panic belt or a failed runner spawn ended it without an
            // outcome, close the record defensively: those paths exist
            // precisely for memory exhaustion, so oom is the honest class.
            let finished = crate::post_process_runs::runs()
                .snapshot(run_id)
                .is_some_and(|r| r.outcome.is_some());
            if !finished {
                crate::post_process_runs::runs().finish(
                    Some(app),
                    run_id,
                    crate::post_process_runs::PostProcessOutcome::Failed {
                        class: crate::llm_client::PostProcessFailureClass::Oom,
                    },
                    None,
                );
            }
            None
        }
    }
}

/// Apple Intelligence: native Swift APIs, free-text output, so the shared
/// validator's free-text mode applies. The availability guards record a
/// skipped run (record-only; no toast, matching today's silent guard).
#[allow(unused_variables)]
async fn run_apple_intelligence_lifecycle(
    app: &AppHandle,
    run_id: u64,
    transcription: &str,
    prompt: &str,
    model: &str,
) -> Option<String> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        use crate::post_process_runs::{runs, PostProcessOutcome};

        if !apple_intelligence::check_apple_intelligence_availability() {
            debug!("Apple Intelligence selected but not currently available on this device");
            runs().finish(
                Some(app),
                run_id,
                PostProcessOutcome::Skipped {
                    reason: crate::local_llm::SkipReason::EngineFailed,
                },
                None,
            );
            return None;
        }

        let system_prompt = build_system_prompt(prompt);
        let token_limit = model.trim().parse::<i32>().unwrap_or(0);
        runs().engine_phase(Some(app), run_id, None, None);
        let generation_started = Instant::now();
        match apple_intelligence::process_text_with_system_prompt(
            &system_prompt,
            transcription,
            token_limit,
        ) {
            Ok(result) => {
                let ms = generation_started.elapsed().as_millis() as u64;
                runs().generation_phase(Some(app), run_id, Some(ms), Some(0));
                match validate_post_process_output(
                    transcription,
                    &result,
                    PostProcessOutputMode::FreeText,
                ) {
                    Ok(clean) => {
                        runs().finish(
                            Some(app),
                            run_id,
                            PostProcessOutcome::Applied,
                            Some(clean.len() as u64),
                        );
                        debug!(
                            "Apple Intelligence post-processing succeeded. Output length: {} chars",
                            clean.len()
                        );
                        Some(clean)
                    }
                    Err(failure) => {
                        runs().finish(
                            Some(app),
                            run_id,
                            PostProcessOutcome::Failed {
                                class: crate::llm_client::PostProcessFailureClass::OutputInvalid,
                            },
                            None,
                        );
                        warn!(
                            "Apple Intelligence output failed validation: {}. Using the raw transcript.",
                            failure.detail
                        );
                        crate::managers::transcription::emit_overlay_notice(
                            app,
                            crate::managers::transcription::NoticeCode::PostProcessOutputInvalid,
                            Some(failure.detail.clone()),
                        );
                        None
                    }
                }
            }
            Err(err) => {
                let ms = generation_started.elapsed().as_millis() as u64;
                runs().generation_phase(Some(app), run_id, Some(ms), Some(0));
                runs().finish(
                    Some(app),
                    run_id,
                    PostProcessOutcome::Failed {
                        class: crate::llm_client::PostProcessFailureClass::OutputInvalid,
                    },
                    None,
                );
                error!("Apple Intelligence post-processing failed: {}", err);
                crate::managers::transcription::emit_overlay_notice(
                    app,
                    crate::managers::transcription::NoticeCode::PostProcessCloudFailed,
                    Some(err.to_string()),
                );
                None
            }
        }
    }

    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    {
        debug!("Apple Intelligence provider selected on unsupported platform");
        crate::post_process_runs::runs().finish(
            Some(app),
            run_id,
            crate::post_process_runs::PostProcessOutcome::Skipped {
                reason: crate::local_llm::SkipReason::EngineFailed,
            },
            None,
        );
        None
    }
}

pub(crate) struct ProcessedTranscription {
    pub final_text: String,
    pub post_processed_text: Option<String>,
    pub post_process_prompt: Option<String>,
    /// The pp: run summary (provider, model, prompt, outcome, latency)
    /// history persists per entry. None when post-process did not run.
    pub post_process_summary: Option<crate::post_process_runs::PostProcessRunSummary>,
}

/// Apply the optional LLM post-process layer to a finished transcription.
///
/// The layer is OPT-IN end to end: it runs only when `post_process` is on
/// AND a prompt is selected (`post_process_selected_prompt_id` defaults to
/// None, so stock installs never rewrite output). It rewrites the ENTIRE
/// text AFTER the command pipeline has already converted spoken commands
/// and inserted their punctuation, so an aggressive prompt can reflow or
/// drop command-inserted marks; that is the documented contract of the
/// feature, not a bug in the command passes.
pub(crate) async fn process_transcription_output(
    app: &AppHandle,
    binding: &str,
    transcription: &str,
    post_process: bool,
    is_cancelled: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
) -> ProcessedTranscription {
    let settings = get_settings(app);
    let mut final_text = transcription.to_string();
    let mut post_processed_text: Option<String> = None;
    let mut post_process_prompt: Option<String> = None;
    let mut post_process_summary: Option<crate::post_process_runs::PostProcessRunSummary> = None;

    if post_process {
        let attempt =
            post_process_transcription(app, &settings, binding, &final_text, is_cancelled).await;
        post_process_summary = attempt.summary;
        if let Some(processed_text) = attempt.text {
            post_processed_text = Some(processed_text.clone());
            final_text = processed_text;

            if let Some(prompt_id) = &settings.post_process_selected_prompt_id {
                if let Some(prompt) = settings
                    .post_process_prompts
                    .iter()
                    .find(|prompt| &prompt.id == prompt_id)
                {
                    post_process_prompt = Some(prompt.prompt.clone());
                }
            }
        }
    }

    ProcessedTranscription {
        final_text,
        post_processed_text,
        post_process_prompt,
        post_process_summary,
    }
}

impl ShortcutAction for TranscribeAction {
    fn start(&self, app: &AppHandle, binding_id: &str, _shortcut_str: &str) {
        let start_time = Instant::now();
        debug!("TranscribeAction::start called for binding: {}", binding_id);

        // Load model in the background
        let tm = app.state::<Arc<TranscriptionManager>>();
        let rm = app.state::<Arc<AudioRecordingManager>>();

        // Load ASR model and VAD model in parallel
        let kickoff_started = Instant::now();
        tm.initiate_model_load();
        let rm_clone = Arc::clone(&rm);
        std::thread::spawn(move || {
            if let Err(e) = rm_clone.preload_vad() {
                debug!("VAD pre-load failed: {}", e);
            }
        });
        let kickoff_elapsed = kickoff_started.elapsed();

        // Don't open the mic if nothing can transcribe the recording; the load
        // kicked off above fails and reports why. Emit a user-facing event so
        // the hotkey explains itself instead of appearing broken (a bare
        // warn! log is invisible to the user).
        if !tm.is_model_loaded() {
            let selected_model = get_settings(app).selected_model;
            if let Err(e) = app
                .state::<Arc<ModelManager>>()
                .get_model_path(&selected_model)
            {
                warn!("Not starting recording: no model can transcribe it ({})", e);
                let _ = app.emit(
                    "recording-error",
                    RecordingErrorEvent {
                        error_type: "no_model_selected".to_string(),
                        detail: Some(e.to_string()),
                    },
                );
                crate::managers::transcription::emit_overlay_notice(
                    app,
                    crate::managers::transcription::NoticeCode::NoModelSelected,
                    Some(e.to_string()),
                );
                return;
            }
        }

        let binding_id = binding_id.to_string();
        let tray_started = Instant::now();
        set_tray_state(app, TrayIconState::Recording);
        let tray_elapsed = tray_started.elapsed();

        // Get the microphone mode to determine audio feedback timing
        let plan_started = Instant::now();
        let settings = get_settings(app);
        let is_always_on = settings.always_on_microphone;

        let selected_model_info = app
            .state::<Arc<ModelManager>>()
            .get_model_info(&settings.selected_model);

        // Use the app-facing model capability as the single pre-recording source
        // for live streaming decisions. Unknown support is represented as false
        // until the model registry is updated by discovery or runtime load.
        let model_supports_streaming = selected_model_info
            .as_ref()
            .map(|m| m.supports_streaming)
            .unwrap_or(false);
        let vad_policy = if !settings.vad_enabled {
            VadPolicy::Disabled
        } else if model_supports_streaming {
            VadPolicy::Streaming
        } else {
            VadPolicy::Offline
        };
        if model_supports_streaming {
            tm.start_stream();
        }
        let plan_elapsed = plan_started.elapsed();

        // Sizing the overlay follows the same advertised capability. A model that
        // doesn't stream (or whose capability is not known yet) gets the compact
        // pill instead of an oversized transparent live window.
        let overlay_started = Instant::now();
        match settings.overlay_style {
            OverlayStyle::Live if model_supports_streaming => utils::show_streaming_overlay(app),
            OverlayStyle::Live | OverlayStyle::Minimal => show_recording_overlay(app),
            OverlayStyle::None => {} // show_overlay_state no-ops on None anyway
        }
        // Everything above runs before capture can begin, so each span here is
        // added keypress->capture latency.
        debug!(
            "start-path pre-recording steps: model_kickoff={:?} tray={:?} settings+stream_plan={:?} overlay={:?}",
            kickoff_elapsed,
            tray_elapsed,
            plan_elapsed,
            overlay_started.elapsed()
        );
        debug!("Microphone mode - always_on: {}", is_always_on);

        let mut recording_error: Option<String> = None;
        let recording_start_time = Instant::now();
        match rm.try_start_recording(&binding_id, vad_policy) {
            Ok(readiness) => {
                debug!(
                    "Recording request accepted in {:?}; waiting for first microphone samples",
                    recording_start_time.elapsed()
                );
                let generation = readiness.generation();
                let app_clone = app.clone();
                let rm_clone = Arc::clone(&rm);
                std::thread::spawn(move || {
                    if !readiness.wait() {
                        debug!("Microphone readiness wait ended without receiving samples");
                        return;
                    }

                    // Development-only preview hook for evaluating the brief
                    // arming animation on hardware that normally starts too fast
                    // to make it visible.
                    #[cfg(debug_assertions)]
                    if let Ok(delay_ms) = std::env::var("HANDY_DEBUG_MIC_READY_DELAY_MS")
                        .unwrap_or_default()
                        .parse::<u64>()
                    {
                        let delay_ms = delay_ms.min(10_000);
                        if delay_ms > 0 {
                            debug!("Delaying microphone-ready cue by {delay_ms}ms for UI preview");
                            std::thread::sleep(Duration::from_millis(delay_ms));
                        }
                    }

                    if !rm_clone.is_recording_readiness_current(generation) {
                        debug!("Microphone became ready for an inactive recording");
                        return;
                    }

                    debug!("Microphone is receiving samples; recording is ready");
                    utils::emit_recording_ready(&app_clone);

                    // The start chime is a readiness cue, so it must follow the
                    // first real input callback rather than Stream::play() or a
                    // fixed delay. The helper returns immediately when feedback
                    // is disabled; mute still follows the same readiness point.
                    if rm_clone.is_recording_readiness_current(generation) {
                        play_feedback_sound_blocking(&app_clone, SoundType::Start);
                    }
                    if rm_clone.is_recording_readiness_current(generation) {
                        rm_clone.apply_mute();
                    }
                });
            }
            Err(e) => {
                debug!("Failed to start recording: {}", e);
                recording_error = Some(e);
            }
        }

        if recording_error.is_none() {
            // Dynamically register the cancel shortcut in a separate task to avoid deadlock
            shortcut::register_cancel_shortcut(app);
        } else {
            // Starting failed (for example due to blocked microphone permissions).
            // Revert UI state so we don't stay stuck in the recording overlay.
            // The overlay hide is DELAYED so the notice row below stays
            // readable on the card; the show-generation guard cancels the
            // hide if a new session starts within the read window.
            tm.cancel_stream();
            set_tray_state(app, TrayIconState::Idle);
            if let Some(err) = recording_error {
                let error_type = if is_microphone_access_denied(&err) {
                    "microphone_permission_denied"
                } else if is_no_input_device_error(&err) {
                    "no_input_device"
                } else {
                    "unknown"
                };
                let _ = app.emit(
                    "recording-error",
                    RecordingErrorEvent {
                        error_type: error_type.to_string(),
                        detail: Some(err.clone()),
                    },
                );
                crate::managers::transcription::emit_overlay_notice(
                    app,
                    crate::managers::transcription::NoticeCode::from_recording_error_type(
                        error_type,
                    ),
                    Some(err),
                );
            }
            utils::hide_recording_overlay_after_error(app);
        }

        debug!(
            "TranscribeAction::start completed in {:?}",
            start_time.elapsed()
        );
    }

    fn stop(&self, app: &AppHandle, binding_id: &str, _shortcut_str: &str) {
        // Prevent a slow microphone from emitting a ready event or start chime
        // after the user has already requested stop.
        app.state::<Arc<AudioRecordingManager>>()
            .invalidate_recording_readiness();

        // The cancel shortcut stays registered through the whole pipeline;
        // FinishGuard::drop (the pipeline's true end) unregisters it.
        let stop_time = Instant::now();
        debug!("TranscribeAction::stop called for binding: {}", binding_id);

        let ah = app.clone();
        let rm = Arc::clone(&app.state::<Arc<AudioRecordingManager>>());
        let tm = Arc::clone(&app.state::<Arc<TranscriptionManager>>());
        let hm = Arc::clone(&app.state::<Arc<HistoryManager>>());

        set_tray_state(app, TrayIconState::Transcribing);
        // Stop should give immediate visual feedback. Live streaming can keep
        // the larger panel, but it still switches from listening to a working
        // spinner while the stream finalizes. Non-streaming paths use the
        // compact transcribing pill (None no-ops in show_*).
        let style = get_settings(app).overlay_style;
        // Capture this before finalizing the stream so every later working state
        // targets the same overlay that was shown for this transcription.
        let use_streaming_overlay = should_use_streaming_overlay(style, tm.is_streaming());
        if use_streaming_overlay {
            tm.emit_stream_working(StreamWorkKind::Transcribing);
        } else {
            show_transcribing_overlay(app);
        }

        // Unmute before playing audio feedback so the stop sound is audible
        rm.remove_mute();

        // Play audio feedback for recording stop
        play_feedback_sound(app, SoundType::Stop);

        let binding_id = binding_id.to_string(); // Clone binding_id for the async task
        let post_process = self.post_process;
        let cancel_generation = rm.cancel_generation();

        // The stop pipeline runs on a dedicated std::thread (mirroring
        // run_stream_worker), entering the async world via block_on only for
        // its genuinely async seams (the WAV save, the post-process future,
        // the preview wait). It used to be spawned onto the shared async
        // runtime, pinning a tokio worker for the pipeline's whole blocking
        // tail: stop_recording's tail sleep, the finalize reply recv, a
        // synchronous batch transcription, the history SQLite insert, and
        // the 1.2s preview wait. stop() runs on the shortcut handler thread,
        // never inside the runtime, so block_on here cannot deadlock.
        std::thread::Builder::new()
            .name("stop-pipeline".into())
            .spawn(move || {
                tauri::async_runtime::block_on(async move {
            let _guard = FinishGuard(ah.clone(), Arc::clone(&tm));
            debug!(
                "Starting async transcription task for binding: {}",
                binding_id
            );

            let stop_recording_time = Instant::now();
            if let Some(samples) = rm.stop_recording(&binding_id, cancel_generation) {
                debug!(
                    "Recording stopped and samples retrieved in {:?}, sample count: {}",
                    stop_recording_time.elapsed(),
                    samples.len()
                );

                if rm.was_cancelled_since(cancel_generation) {
                    debug!("Transcription operation cancelled after recording stop");
                    tm.cancel_stream();
                    utils::hide_recording_overlay(&ah);
                    set_tray_state(&ah, TrayIconState::Idle);
                    return;
                }

                if samples.is_empty() {
                    debug!("Recording produced no audio samples; skipping persistence");
                    // Tear down any streaming worker so its channel doesn't leak
                    // and block the next start_stream.
                    tm.cancel_stream();
                    utils::hide_recording_overlay(&ah);
                    set_tray_state(&ah, TrayIconState::Idle);
                } else {
                    // Save WAV concurrently with transcription
                    let sample_count = samples.len();
                    let file_name = format!("voxbar-{}.wav", chrono::Utc::now().timestamp());
                    let wav_path = hm.recordings_dir().join(&file_name);
                    let wav_path_for_verify = wav_path.clone();
                    let samples_for_wav = samples.clone();
                    let wav_handle = tauri::async_runtime::spawn_blocking(move || {
                        crate::audio_toolkit::save_wav_file(&wav_path, &samples_for_wav)
                    });

                    // Transcribe concurrently with WAV save. If a live stream was
                    // running, finalize it and use its text (all audio was already
                    // fed to the stream); otherwise batch-transcribe the samples.
                    // The result also carries the model id that actually produced
                    // the text: after a RAM auto-fallback that can differ from
                    // the persisted selection, and history records it per entry.
                    // The streaming worker's model is read BEFORE finalize: an
                    // "unload immediately" setting clears it as soon as the
                    // stream ends.
                    //
                    // In-session command edits (the command-mode modifier) and
                    // manual buffer edits fold into the final text through the
                    // session buffer inside finalize_stream.
                    let transcription_time = Instant::now();
                    let stream_model = tm.get_current_model().unwrap_or_default();
                    let finalize = tm.finalize_stream();
                    // The summary line's source term: which arm produced the
                    // text (finalize-vs-batch, the survey's unanswerable
                    // question).
                    let mut outcome_source: &'static str = "stream-finalize";
                    let transcription_result = match finalize {
                        // A finalized stream with usable text wins. An empty result
                        // (no active stream, produced nothing, or the stream failed
                        // or its worker crashed) falls back to a full batch
                        // transcription of the same audio. A cancelled finalize is
                        // surfaced instead, so a cancel never starts a batch run.
                        Ok(Some(text)) if !text.trim().is_empty() => {
                            Ok((text, stream_model.clone()))
                        }
                        Ok(empty) => {
                            // The one exception: an empty finalize with the
                            // model already evicted by the Immediately unload
                            // returns the quiet empty result instead of a
                            // batch run that can only fail (see
                            // empty_finalize_outcome).
                            let unload_is_immediately = get_settings(&ah).model_unload_timeout
                                == ModelUnloadTimeout::Immediately;
                            match empty_finalize_outcome(
                                unload_is_immediately,
                                tm.is_model_loaded(),
                            ) {
                                EmptyFinalizeOutcome::QuietEmpty => {
                                    debug!(
                                        "Stream finalize produced no text ({}) and the model \
                                         was unloaded by the Immediately setting; returning the \
                                         quiet empty result",
                                        empty.as_deref().map(str::len).unwrap_or(0)
                                    );
                                    outcome_source = "quiet-empty";
                                    Ok((String::new(), stream_model.clone()))
                                }
                                EmptyFinalizeOutcome::Batch => {
                                    outcome_source = "batch";
                                    tm.transcribe_with_model(samples)
                                }
                            }
                        }
                        Err(err) => Err(err),
                    };

                    // Await WAV save and verify
                    let wav_saved = match wav_handle.await {
                        Ok(Ok(())) => {
                            match crate::audio_toolkit::verify_wav_file(
                                &wav_path_for_verify,
                                sample_count,
                            ) {
                                Ok(()) => true,
                                Err(e) => {
                                    error!("WAV verification failed: {}", e);
                                    false
                                }
                            }
                        }
                        Ok(Err(e)) => {
                            error!("Failed to save WAV file: {}", e);
                            false
                        }
                        Err(e) => {
                            error!("WAV save task panicked: {}", e);
                            false
                        }
                    };

                    if rm.was_cancelled_since(cancel_generation) {
                        debug!("Transcription operation cancelled before output handling");
                        utils::hide_recording_overlay(&ah);
                        set_tray_state(&ah, TrayIconState::Idle);
                        return;
                    }

                    match transcription_result {
                        Ok((transcription, used_model)) => {
                            debug!(
                                "Transcription completed in {:?}: '{}'",
                                transcription_time.elapsed(),
                                utils::redact_text(&transcription)
                            );

                            if post_process {
                                if use_streaming_overlay {
                                    tm.emit_stream_working(StreamWorkKind::Polishing);
                                } else {
                                    show_processing_overlay(&ah);
                                }
                            }
                            // The same cancel generation the paste gates
                            // use, threaded into the swap runner so user
                            // cancellation aborts post-processing promptly
                            // (polled every 25ms, never awaited).
                            let rm_for_swap = Arc::clone(&rm);
                            let is_cancelled: Option<Arc<dyn Fn() -> bool + Send + Sync>> =
                                Some(Arc::new(move || {
                                    rm_for_swap.was_cancelled_since(cancel_generation)
                                }));
                            let Some(processed) = complete_unless_cancelled(
                                process_transcription_output(
                                    &ah,
                                    &binding_id,
                                    &transcription,
                                    post_process,
                                    is_cancelled,
                                ),
                                || rm.was_cancelled_since(cancel_generation),
                            )
                            .await
                            else {
                                debug!("Transcription operation cancelled during output handling");
                                // The dropped future leaves a cloud run's
                                // record unconcluded (no detached runner);
                                // close it as cancelled so the Debug table
                                // never shows a phantom live run.
                                crate::post_process_runs::runs()
                                    .cancel_live_dictation_runs();
                                utils::hide_recording_overlay(&ah);
                                set_tray_state(&ah, TrayIconState::Idle);
                                return;
                            };

                            if rm.was_cancelled_since(cancel_generation) {
                                debug!("Transcription operation cancelled before paste");
                                utils::hide_recording_overlay(&ah);
                                set_tray_state(&ah, TrayIconState::Idle);
                                return;
                            }

                            // Save to history if WAV was saved
                            if wav_saved {
                                if let Err(err) = hm.save_entry(
                                    file_name,
                                    transcription,
                                    post_process,
                                    processed.post_processed_text.clone(),
                                    processed.post_process_prompt.clone(),
                                    Some(used_model),
                                    processed.post_process_summary.clone(),
                                ) {
                                    error!("Failed to save history entry: {}", err);
                                }
                            }

                            // The outcome line's post-process term, computed
                            // once for every exit below.
                            let post_process_term: &'static str = if !post_process {
                                "off"
                            } else if processed.post_processed_text.is_some() {
                                "processed"
                            } else {
                                "raw-fallback"
                            };

                            if processed.final_text.is_empty() {
                                info!(
                                    "{}",
                                    format_session_outcome(&SessionOutcomeSummary {
                                        binding: binding_id.clone(),
                                        audio_seconds: sample_count as f64 / 16_000.0,
                                        sample_count,
                                        source: outcome_source,
                                        post_process: post_process_term,
                                        paste: "skipped-empty",
                                        stop_to_end_ms: stop_time.elapsed().as_millis(),
                                    })
                                );
                                utils::hide_recording_overlay(&ah);
                                set_tray_state(&ah, TrayIconState::Idle);
                            } else {
                                // Final-text preview before the paste: batch
                                // models show nothing while recording, so
                                // this is the operator's only look at the
                                // text before it lands. The streaming overlay
                                // already showed interim text; its final
                                // preview swaps in exactly what will be
                                // pasted. The delay sits BEFORE the single
                                // utils::paste call, and paste performs the
                                // paste, auto-submit, and clipboard handling
                                // as one unit, so the preview can never
                                // stack with auto-submit into a double
                                // paste; cancelling during the window skips
                                // the paste entirely (the loop below polls
                                // the same cancellation generation the paste
                                // closure checks).
                                if get_settings(&ah).preview_before_paste {
                                    if use_streaming_overlay {
                                        let _ = StreamTextEvent {
                                            committed: processed.final_text.clone(),
                                            tentative: String::new(),
                                            // The final preview shows the
                                            // finished text; no deletion to
                                            // report.
                                            deleted: None,
                                        }
                                        .emit(&ah);
                                    } else {
                                        utils::show_final_preview_overlay(
                                            &ah,
                                            &processed.final_text,
                                        );
                                    }

                                    let mut waited = Duration::ZERO;
                                    while waited < PREVIEW_BEFORE_PASTE_DELAY {
                                        if rm.was_cancelled_since(cancel_generation) {
                                            debug!(
                                                "Transcription operation cancelled during final preview"
                                            );
                                            utils::hide_recording_overlay(&ah);
                                            set_tray_state(&ah, TrayIconState::Idle);
                                            return;
                                        }
                                        tokio::time::sleep(CANCELLATION_POLL_INTERVAL).await;
                                        waited += CANCELLATION_POLL_INTERVAL;
                                    }
                                }

                                let ah_clone = ah.clone();
                                let paste_time = Instant::now();
                                let final_text = processed.final_text;
                                let rm_for_paste = Arc::clone(&rm);
                                let paste_failed = Arc::new(std::sync::atomic::AtomicBool::new(
                                    false,
                                ));
                                let paste_failed_flag = Arc::clone(&paste_failed);
                                // The outcome summary completes inside the
                                // paste closure: only there is the paste's
                                // ok/failed known, and the closure runs at
                                // the pipeline's true end.
                                let outcome_summary = SessionOutcomeSummary {
                                    binding: binding_id.clone(),
                                    audio_seconds: sample_count as f64 / 16_000.0,
                                    sample_count,
                                    source: outcome_source,
                                    post_process: post_process_term,
                                    // Filled in by the paste outcome below.
                                    paste: "ok",
                                    stop_to_end_ms: 0,
                                };
                                ah.run_on_main_thread(move || {
                                    if rm_for_paste.was_cancelled_since(cancel_generation) {
                                        debug!("Transcription operation cancelled before paste");
                                        utils::hide_recording_overlay(&ah_clone);
                                        set_tray_state(&ah_clone, TrayIconState::Idle);
                                        return;
                                    }

                                    let paste_ok = match utils::paste(
                                        final_text,
                                        ah_clone.clone(),
                                    ) {
                                        Ok(()) => {
                                            debug!(
                                                "Text pasted successfully in {:?}",
                                                paste_time.elapsed()
                                            );
                                            true
                                        }
                                        Err(e) => {
                                            error!("Failed to paste transcription: {}", e);
                                            let _ = ah_clone.emit("paste-error", ());
                                            crate::managers::transcription::emit_overlay_notice(
                                                &ah_clone,
                                                crate::managers::transcription::NoticeCode::PasteFailed,
                                                Some(e.to_string()),
                                            );
                                            paste_failed_flag.store(
                                                true,
                                                std::sync::atomic::Ordering::Release,
                                            );
                                            false
                                        }
                                    };
                                    let mut outcome_summary = outcome_summary;
                                    outcome_summary.paste =
                                        if paste_ok { "ok" } else { "failed" };
                                    outcome_summary.stop_to_end_ms =
                                        stop_time.elapsed().as_millis();
                                    info!("{}", format_session_outcome(&outcome_summary));
                                    set_tray_state(&ah_clone, TrayIconState::Idle);
                                    if paste_failed_flag.load(std::sync::atomic::Ordering::Acquire)
                                    {
                                        // Delayed hide so the in-card paste
                                        // error stays readable.
                                        utils::hide_recording_overlay_after_error(&ah_clone);
                                    } else {
                                        utils::hide_recording_overlay(&ah_clone);
                                    }
                                })
                                .unwrap_or_else(|e| {
                                    error!("Failed to run paste on main thread: {:?}", e);
                                    info!(
                                        "{}",
                                        format_session_outcome(&SessionOutcomeSummary {
                                            binding: binding_id.clone(),
                                            audio_seconds: sample_count as f64 / 16_000.0,
                                            sample_count,
                                            source: outcome_source,
                                            post_process: post_process_term,
                                            paste: "dispatch-failed",
                                            stop_to_end_ms: stop_time.elapsed().as_millis(),
                                        })
                                    );
                                    utils::hide_recording_overlay(&ah);
                                    set_tray_state(&ah, TrayIconState::Idle);
                                });
                            }
                        }
                        Err(err) => {
                            if rm.was_cancelled_since(cancel_generation) {
                                debug!(
                                    "Transcription operation cancelled after transcription error"
                                );
                                utils::hide_recording_overlay(&ah);
                                set_tray_state(&ah, TrayIconState::Idle);
                                return;
                            }

                            error!("Transcription failed: {}", err);
                            info!(
                                "{}",
                                format_session_outcome(&SessionOutcomeSummary {
                                    binding: binding_id.clone(),
                                    audio_seconds: sample_count as f64 / 16_000.0,
                                    sample_count,
                                    source: "failed",
                                    post_process: if post_process { "failed" } else { "off" },
                                    paste: "error",
                                    stop_to_end_ms: stop_time.elapsed().as_millis(),
                                })
                            );
                            // Surface the failure to the UI (toast). The full
                            // message is also in voxbar.log via the line above.
                            let _ = ah.emit("transcription-error", err.to_string());
                            crate::managers::transcription::emit_overlay_notice(
                                &ah,
                                crate::managers::transcription::NoticeCode::TranscriptionFailed,
                                Some(err.to_string()),
                            );
                            // Save entry with empty text so user can retry
                            if wav_saved {
                                if let Err(save_err) = hm.save_entry(
                                    file_name,
                                    String::new(),
                                    post_process,
                                    None,
                                    None,
                                    Some(stream_model),
                                    None,
                                ) {
                                    error!("Failed to save failed history entry: {}", save_err);
                                }
                            }
                            // Delayed hide so the in-card error stays readable.
                            set_tray_state(&ah, TrayIconState::Idle);
                            utils::hide_recording_overlay_after_error(&ah);
                        }
                    }
                }
            } else {
                debug!("No samples retrieved from recording stop");
                // Tear down any streaming worker so its channel doesn't leak.
                tm.cancel_stream();
                utils::hide_recording_overlay(&ah);
                set_tray_state(&ah, TrayIconState::Idle);
            }
                })
            })
            .expect("failed to spawn the stop pipeline thread");

        debug!(
            "TranscribeAction::stop completed in {:?}",
            stop_time.elapsed()
        );
    }
}

// Cancel Action
struct CancelAction;

impl ShortcutAction for CancelAction {
    fn start(&self, app: &AppHandle, _binding_id: &str, _shortcut_str: &str) {
        utils::cancel_current_operation(app);
    }

    fn stop(&self, _app: &AppHandle, _binding_id: &str, _shortcut_str: &str) {
        // Nothing to do on stop for cancel
    }
}

// Delete Last Word Action
struct DeleteLastWordAction;

impl ShortcutAction for DeleteLastWordAction {
    fn start(&self, app: &AppHandle, _binding_id: &str, _shortcut_str: &str) {
        if !get_settings(app).delete_last_word_enabled {
            debug!("Delete-last-word action disabled by its settings toggle");
            return;
        }

        // Session-scoped by design: delete-last-word edits THIS dictation's
        // accumulated transcript only. While no recording session is live
        // there is no dictation buffer to edit, and injecting the
        // word-delete chord into the focused app would edit text the
        // operator never dictated here - the idle fallback that did that
        // was revoked. A press with no live session is a logged no-op.
        let Some(coordinator) = app.try_state::<TranscriptionCoordinator>() else {
            debug!("Delete-last-word pressed with no coordinator; ignoring");
            return;
        };
        if !coordinator.is_recording_session() {
            debug!("Delete-last-word pressed with no live dictation session; ignoring");
            // Not silent: the usage log showed idle presses the operator
            // believed were editing text. One info notice explains the
            // session-scoped rule (the frontend rate-limits repeats).
            crate::managers::transcription::emit_overlay_notice(
                app,
                crate::managers::transcription::NoticeCode::DeleteLastWordNoSession,
                None,
            );
            return;
        }

        let tm = app.state::<Arc<TranscriptionManager>>();
        if !tm.apply_session_buffer_word_deletion() {
            // A recording is live but has no stream buffer to edit (a
            // batch/non-streaming model, or the stream has not begun).
            // There is nothing in the buffer to delete and keystroke
            // injection would hit the wrong text, so this is a deliberate
            // no-op.
            debug!("Delete-last-word skipped: recording session active but no live buffer to edit");
            crate::managers::transcription::emit_overlay_notice(
                app,
                crate::managers::transcription::NoticeCode::DeleteLastWordNoBuffer,
                None,
            );
        }
    }

    fn stop(&self, _app: &AppHandle, _binding_id: &str, _shortcut_str: &str) {
        // One-shot action: nothing to do on release
    }
}

// Undo Action
struct UndoAction;

impl ShortcutAction for UndoAction {
    fn start(&self, app: &AppHandle, _binding_id: &str, _shortcut_str: &str) {
        if !get_settings(app).undo_enabled {
            debug!("Undo action disabled by its settings toggle");
            return;
        }

        // Operator rule: the ONLY always-on binding is the transcribe
        // trigger. Undo is a dictation-flow key: it may fire while a
        // recording session is LIVE, never after it ends. In a live
        // session it clears the accumulated dictation buffer (a key-based
        // start-over). There is intentionally no post-paste undo.
        let Some(coordinator) = app.try_state::<TranscriptionCoordinator>() else {
            debug!("Undo pressed with no coordinator; ignoring");
            return;
        };
        if !coordinator.is_undo_active() {
            debug!("Undo pressed with no live dictation session; ignoring");
            // Same treatment as delete-last-word: the revocation made this
            // key session-scoped, and the press explains itself once.
            crate::managers::transcription::emit_overlay_notice(
                app,
                crate::managers::transcription::NoticeCode::UndoNoSession,
                None,
            );
            return;
        }

        let tm = app.state::<Arc<TranscriptionManager>>();
        if !tm.clear_session_buffer() {
            // A recording is live but has no stream buffer to clear (a
            // batch/non-streaming model, or the stream has not begun).
            debug!("Undo skipped: recording session active but no live buffer to clear");
            crate::managers::transcription::emit_overlay_notice(
                app,
                crate::managers::transcription::NoticeCode::UndoNoBuffer,
                None,
            );
        }
    }

    fn stop(&self, _app: &AppHandle, _binding_id: &str, _shortcut_str: &str) {
        // One-shot action: nothing to do on release
    }
}

// Test Action
struct TestAction;

impl ShortcutAction for TestAction {
    fn start(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str) {
        log::info!(
            "Shortcut ID '{}': Started - {} (App: {})", // Changed "Pressed" to "Started" for consistency
            binding_id,
            shortcut_str,
            app.package_info().name
        );
    }

    fn stop(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str) {
        log::info!(
            "Shortcut ID '{}': Stopped - {} (App: {})", // Changed "Released" to "Stopped" for consistency
            binding_id,
            shortcut_str,
            app.package_info().name
        );
    }
}

// Static Action Map
pub static ACTION_MAP: Lazy<HashMap<String, Arc<dyn ShortcutAction>>> = Lazy::new(|| {
    let mut map = HashMap::new();
    for binding_id in ["transcribe", "transcribe_with_post_process"] {
        let post_process =
            transcribe_action_config(binding_id).expect("known transcribe binding id");
        map.insert(
            binding_id.to_string(),
            Arc::new(TranscribeAction { post_process }) as Arc<dyn ShortcutAction>,
        );
    }
    map.insert(
        "cancel".to_string(),
        Arc::new(CancelAction) as Arc<dyn ShortcutAction>,
    );
    map.insert(
        "delete_last_word".to_string(),
        Arc::new(DeleteLastWordAction) as Arc<dyn ShortcutAction>,
    );
    map.insert(
        "undo".to_string(),
        Arc::new(UndoAction) as Arc<dyn ShortcutAction>,
    );
    map.insert(
        "test".to_string(),
        Arc::new(TestAction) as Arc<dyn ShortcutAction>,
    );
    map
});

#[cfg(test)]
mod tests {
    use super::{
        complete_unless_cancelled, is_blank_transcription, local_engine_availability,
        post_process_output_schema, should_use_streaming_overlay, strip_think_block,
        uses_local_engine, TRANSCRIPTION_FIELD,
    };
    use crate::settings::OverlayStyle;
    use std::future;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn blank_transcription_is_detected() {
        assert!(is_blank_transcription(""));
        assert!(is_blank_transcription("   "));
        assert!(is_blank_transcription("\t\n  \r\n"));
    }

    #[test]
    fn non_blank_transcription_is_kept() {
        assert!(!is_blank_transcription("hello"));
        assert!(!is_blank_transcription("  hello  "));
    }

    #[test]
    fn completed_operation_returns_its_output() {
        let result = tauri::async_runtime::block_on(complete_unless_cancelled(
            future::ready("done"),
            || false,
        ));

        assert_eq!(result, Some("done"));
    }

    /// The per-session outcome summary line: one line, every field present,
    /// stable order. This is the line the stop pipeline info!s at its true
    /// end, so the shape is pinned here.
    #[test]
    fn session_outcome_line_shape() {
        use super::{format_session_outcome, SessionOutcomeSummary};

        let line = format_session_outcome(&SessionOutcomeSummary {
            binding: "transcribe_with_post_process".to_string(),
            audio_seconds: 12.5,
            sample_count: 200_000,
            source: "stream-finalize",
            post_process: "processed",
            paste: "ok",
            stop_to_end_ms: 3210,
        });
        assert_eq!(
            line,
            "dictation outcome: binding=transcribe_with_post_process audio=12.50s \
             samples=200000 source=stream-finalize post_process=processed paste=ok \
             stop_to_end=3210ms"
        );
        assert!(!line.contains('\n'), "one line per session");

        // The failure shapes stay single-line too.
        let failed = format_session_outcome(&SessionOutcomeSummary {
            binding: "transcribe".to_string(),
            audio_seconds: 0.0,
            sample_count: 0,
            source: "failed",
            post_process: "off",
            paste: "error",
            stop_to_end_ms: 42,
        });
        assert!(failed.contains("source=failed"));
        assert!(failed.contains("paste=error"));
        assert!(!failed.contains('\n'));
    }

    #[test]
    fn pending_operation_stops_after_cancellation() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancelled_for_thread = Arc::clone(&cancelled);
        let cancel_thread = thread::spawn(move || {
            thread::sleep(Duration::from_millis(10));
            cancelled_for_thread.store(true, Ordering::Release);
        });

        let result = tauri::async_runtime::block_on(complete_unless_cancelled(
            future::pending::<()>(),
            || cancelled.load(Ordering::Acquire),
        ));

        cancel_thread.join().unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn leading_think_block_is_stripped() {
        assert_eq!(
            strip_think_block("<think>pondering...</think>Cleaned text."),
            "Cleaned text."
        );
        assert_eq!(
            strip_think_block("  \n<think>multi\nline</think>\n  Cleaned text."),
            "Cleaned text."
        );
    }

    #[test]
    fn content_without_think_block_is_unchanged() {
        assert_eq!(strip_think_block("Cleaned text."), "Cleaned text.");
        assert_eq!(
            strip_think_block("Mentions <think> mid-sentence."),
            "Mentions <think> mid-sentence."
        );
        // Unclosed block: leave untouched rather than guess
        assert_eq!(
            strip_think_block("<think>never closed"),
            "<think>never closed"
        );
    }

    #[test]
    fn live_overlay_uses_streaming_states_only_for_streaming_models() {
        assert!(should_use_streaming_overlay(OverlayStyle::Live, true));
        assert!(!should_use_streaming_overlay(OverlayStyle::Live, false));
        assert!(!should_use_streaming_overlay(OverlayStyle::Minimal, true));
        assert!(!should_use_streaming_overlay(OverlayStyle::None, true));
    }

    /// The finalize-outcome x unload-timeout decision table: batch stays the
    /// fallback for every ordinary configuration and every state where the
    /// model is still resident; ONLY the Immediately configuration with the
    /// model actually evicted by the finalize returns the quiet empty
    /// result (the silent-tap toast fix).
    #[test]
    fn empty_finalize_decision_table() {
        use super::{empty_finalize_outcome, EmptyFinalizeOutcome};

        // Immediately + model evicted by the finalize: quiet empty.
        assert!(matches!(
            empty_finalize_outcome(true, false),
            EmptyFinalizeOutcome::QuietEmpty
        ));
        // Still Immediately-configured, but the model is resident (e.g. the
        // stream never ran and never unloaded): batch is safe and ordinary.
        assert!(matches!(
            empty_finalize_outcome(true, true),
            EmptyFinalizeOutcome::Batch
        ));
        // Any other unload configuration: always batch, model or not.
        assert!(matches!(
            empty_finalize_outcome(false, false),
            EmptyFinalizeOutcome::Batch
        ));
        assert!(matches!(
            empty_finalize_outcome(false, true),
            EmptyFinalizeOutcome::Batch
        ));
    }

    /// The assignable editing actions must exist in ACTION_MAP (so presses
    /// dispatch to them) and must NOT be transcribe bindings (so they never
    /// route into the recording coordinator or collide with the PTT trigger
    /// by sharing its lifecycle).
    #[test]
    fn editing_actions_are_mapped_and_not_recording_bindings() {
        use crate::actions::ACTION_MAP;

        assert!(ACTION_MAP.contains_key("delete_last_word"));
        assert!(ACTION_MAP.contains_key("undo"));
        assert!(!crate::transcription_coordinator::is_transcribe_binding(
            "delete_last_word"
        ));
        assert!(!crate::transcription_coordinator::is_transcribe_binding(
            "undo"
        ));
    }

    /// Routing safety for the command-mode redesign: the command binding
    /// has NO entry in the action map (it is a during-dictation modifier
    /// routed to the coordinator's `send_command_modifier`, never a
    /// recording action), while the dictation bindings route only to
    /// dictation. This is the single table the ACTION_MAP is built from, so
    /// the property holds for every dispatch path (shortcut handler,
    /// coordinator effects): nothing can start a command capture recording.
    #[test]
    fn command_mode_binding_has_no_recording_action() {
        use super::transcribe_action_config;
        use crate::actions::ACTION_MAP;

        assert_eq!(transcribe_action_config("transcribe"), Some(false));
        assert_eq!(
            transcribe_action_config("transcribe_with_post_process"),
            Some(true)
        );
        assert_eq!(
            transcribe_action_config("transcribe_commands"),
            None,
            "command mode must never route to a recording action"
        );
        assert!(
            !ACTION_MAP.contains_key("transcribe_commands"),
            "the command binding dispatches through the coordinator's modifier path, not ACTION_MAP"
        );
        assert_eq!(transcribe_action_config("delete_last_word"), None);
        assert_eq!(transcribe_action_config("undo"), None);
        assert_eq!(transcribe_action_config("unknown"), None);
    }

    /// T28: provider routing. The local provider routes to the on-device
    /// engine; every API provider (including custom and openai, the off
    /// path) keeps today's llm_client behavior.
    #[test]
    fn provider_routing_separates_local_from_api_paths() {
        assert!(uses_local_engine(crate::settings::LOCAL_LLM_PROVIDER_ID));
        assert!(uses_local_engine("local"));
        assert!(!uses_local_engine("openai"));
        assert!(!uses_local_engine("custom"));
        assert!(!uses_local_engine("anthropic"));
        assert!(!uses_local_engine(
            crate::settings::APPLE_INTELLIGENCE_PROVIDER_ID
        ));
        assert!(!uses_local_engine(""));
    }

    /// T28: the local branch's availability decision. Not downloaded ->
    /// skip with download_missing (raw transcript, never blocks dictation,
    /// never auto-downloads); downloaded -> no skip, the engine runs.
    #[test]
    fn local_branch_skips_only_when_model_not_downloaded() {
        assert_eq!(
            local_engine_availability(false).map(|(r, _)| r),
            Some(crate::local_llm::SkipReason::DownloadMissing)
        );
        assert!(local_engine_availability(false).unwrap().1.is_some());
        assert_eq!(local_engine_availability(true), None);
    }

    /// T28: the extracted schema is byte-identical to the literal the API
    /// path used before the extraction, so both engines answer to the
    /// exact same structured-output contract.
    #[test]
    fn post_process_schema_matches_the_api_literal_verbatim() {
        let expected = serde_json::json!({
            "type": "object",
            "properties": {
                (TRANSCRIPTION_FIELD): {
                    "type": "string",
                    "description": "The cleaned and processed transcription text"
                }
            },
            "required": [TRANSCRIPTION_FIELD],
            "additionalProperties": false
        });
        assert_eq!(post_process_output_schema(), expected);
        // And the grammar renderer accepts it (the local branch feeds its
        // serialization straight into json_schema_to_grammar).
        let rendered = llama_cpp_2::json_schema_to_grammar(&expected.to_string());
        assert!(rendered.is_ok(), "{:?}", rendered.err());
        assert!(rendered.unwrap().contains("transcription"));
    }

    /// The in-session command contract, applier half: a blank or
    /// unrecognized delta edits nothing, so a mis-held modifier with no
    /// recognizable speech leaves the live buffer untouched.
    #[test]
    fn blank_or_unrecognized_command_delta_edits_nothing() {
        let matrix = crate::audio_toolkit::command_matrix::default_compiled_matrix();
        let mut buffer = "hello world".to_string();
        crate::audio_toolkit::apply_command_delta_to_buffer(&mut buffer, "", &matrix);
        crate::audio_toolkit::apply_command_delta_to_buffer(&mut buffer, "   ", &matrix);
        crate::audio_toolkit::apply_command_delta_to_buffer(&mut buffer, "\n\t", &matrix);
        crate::audio_toolkit::apply_command_delta_to_buffer(
            &mut buffer,
            "um nothing here",
            &matrix,
        );
        assert_eq!(buffer, "hello world");
    }

    // ---- The pp: lifecycle with a mock cloud seam ----

    use super::{run_cloud_lifecycle, CloudCompletionSeam, PostProcessOutputMode};

    /// A scripted cloud seam: each `send` pops the next canned result. The
    /// recorded calls let tests pin which attempt shape ran (structured vs
    /// legacy) and how many attempts fired.
    struct MockCloud {
        results: Vec<super::CloudSendResult>,
        calls: std::sync::Mutex<Vec<bool>>, // true = carried a schema
    }

    impl MockCloud {
        fn new(results: Vec<super::CloudSendResult>) -> Self {
            Self {
                results,
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl CloudCompletionSeam for MockCloud {
        fn send(
            &mut self,
            _user_content: String,
            _system_prompt: Option<String>,
            json_schema: Option<serde_json::Value>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = super::CloudSendResult> + Send + '_>>
        {
            self.calls.lock().unwrap().push(json_schema.is_some());
            let result = self.results.pop().expect("no scripted response left");
            Box::pin(async move { result })
        }
    }

    fn begin_cloud_run() -> u64 {
        crate::post_process_runs::runs().begin(
            None,
            crate::post_process_runs::RunRequestMeta {
                binding: "transcribe_with_post_process".to_string(),
                engine: crate::post_process_runs::PostProcessEngineKind::Cloud,
                provider_id: "openai".to_string(),
                model: "gpt-4o-mini".to_string(),
                prompt_id: Some("clean".to_string()),
                prompt_name: Some("Clean up".to_string()),
                prompt_version: None,
                template_language: None,
                chars_in: 42,
            },
        )
    }

    /// One happy cloud run produces the full requested -> engine ->
    /// generation -> outcome line sequence with matching run ids, and the
    /// validated text is returned.
    #[test]
    fn cloud_lifecycle_emits_the_full_phase_sequence() {
        let run_id = begin_cloud_run();
        let mut seam = MockCloud::new(vec![Ok(Some(
            "{\"transcription\":\"Hello, world.\"}".to_string(),
        ))]);
        let notices: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        let text = tauri::async_runtime::block_on(run_cloud_lifecycle(
            None,
            &mut seam,
            run_id,
            "um hello world",
            Some("clean it".to_string()),
            true,
            "clean: um hello world".to_string(),
            |code, _detail| notices.lock().unwrap().push(code.as_str().to_string()),
        ));
        assert_eq!(text.as_deref(), Some("Hello, world."));

        let record = crate::post_process_runs::runs().snapshot(run_id).unwrap();
        assert_eq!(
            record
                .log_lines
                .iter()
                .map(|l| l.split("phase=").nth(1).unwrap().split(' ').next().unwrap())
                .collect::<Vec<_>>(),
            vec!["requested", "engine", "generation", "outcome"],
            "phases in order: {:?}",
            record.log_lines
        );
        assert!(record
            .log_lines
            .iter()
            .all(|l| l.starts_with(&format!("pp: run={run_id} "))));
        assert_eq!(
            record.outcome,
            Some(crate::post_process_runs::PostProcessOutcome::Applied)
        );
        assert_eq!(record.chars_out, Some(13));
        // One structured attempt, no retry.
        assert_eq!(*seam.calls.lock().unwrap(), vec![true]);
        assert!(notices.lock().unwrap().is_empty(), "no notice on success");
    }

    /// A cloud 401 (structured rejected, legacy retry also rejected with
    /// 401) classifies as failed(auth) and returns None: the raw
    /// transcript is what gets pasted.
    #[test]
    fn cloud_401_is_failed_auth_and_raw_transcript() {
        let run_id = begin_cloud_run();
        // The seam pops from the END: legacy attempt first, structured second.
        let mut seam = MockCloud::new(vec![
            Err("API request failed with status 401: unauthorized".to_string()), // legacy
            Err("API request failed with status 401: unauthorized".to_string()), // structured
        ]);
        let notices: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        let text = tauri::async_runtime::block_on(run_cloud_lifecycle(
            None,
            &mut seam,
            run_id,
            "hello world",
            Some("clean it".to_string()),
            true,
            "clean: hello world".to_string(),
            |code, _detail| notices.lock().unwrap().push(code.as_str().to_string()),
        ));
        assert_eq!(text, None, "the raw transcript is used, never model text");

        let record = crate::post_process_runs::runs().snapshot(run_id).unwrap();
        assert_eq!(
            record.outcome,
            Some(crate::post_process_runs::PostProcessOutcome::Failed {
                class: crate::llm_client::PostProcessFailureClass::Auth
            })
        );
        // The structured failure counted as one retry before legacy
        // (calls record in order: structured with schema, then legacy).
        assert_eq!(record.phases_generation.retries, Some(1));
        assert_eq!(*seam.calls.lock().unwrap(), vec![true, false]);
        assert_eq!(
            notices.lock().unwrap().as_slice(),
            ["post_process_cloud_failed"],
            "cloud failures reach the notice channel"
        );
    }

    /// The never-lose-the-transcript rule, cloud half: a structured
    /// response whose JSON cannot be parsed (or is missing the field) used
    /// to paste the raw model content; now it fails validation, classifies
    /// output_invalid, notifies, and returns None.
    #[test]
    fn unparseable_cloud_output_never_replaces_the_transcript() {
        for bad in [
            "not json at all",
            "{\"wrong\": \"shape\"}",
            "{\"transcription\": \"\"}",
        ] {
            let run_id = begin_cloud_run();
            let mut seam = MockCloud::new(vec![Ok(Some(bad.to_string()))]);
            let notices: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
            let text = tauri::async_runtime::block_on(run_cloud_lifecycle(
                None,
                &mut seam,
                run_id,
                "hello world this is a longer transcript",
                Some("clean it".to_string()),
                true,
                "clean: hello world".to_string(),
                |code, _detail| notices.lock().unwrap().push(code.as_str().to_string()),
            ));
            assert_eq!(text, None, "raw model content must not paste: {bad}");
            let record = crate::post_process_runs::runs().snapshot(run_id).unwrap();
            assert_eq!(
                record.outcome,
                Some(crate::post_process_runs::PostProcessOutcome::Failed {
                    class: crate::llm_client::PostProcessFailureClass::OutputInvalid
                }),
                "bad output: {bad}"
            );
            assert_eq!(
                notices.lock().unwrap().as_slice(),
                ["post_process_output_invalid"],
                "the fallback is visible: {bad}"
            );
        }
    }

    /// The shared validator, local-half parity: the same function the
    /// cloud paths use also guards the local engine's structured output
    /// and the free-text engines, with the skip vocabulary the local toasts
    /// key off (engine_failed / length_guard).
    #[test]
    fn shared_validator_covers_both_output_modes() {
        use super::validate_post_process_output;
        use crate::local_llm::SkipReason;

        // Structured: happy path extracts and strips.
        assert_eq!(
            validate_post_process_output(
                "hello world",
                "{\"transcription\":\"Hello, world.\"}",
                PostProcessOutputMode::StructuredJson,
            )
            .unwrap(),
            "Hello, world."
        );
        // Structured: unparseable and field-missing fold to engine_failed.
        for bad in ["nope", "{}"] {
            let failure = validate_post_process_output(
                "hello world",
                bad,
                PostProcessOutputMode::StructuredJson,
            )
            .unwrap_err();
            assert_eq!(failure.skip_reason, SkipReason::EngineFailed, "bad: {bad}");
        }
        // Fidelity collapse (either mode) is length_guard.
        let long = "word ".repeat(40);
        let failure = validate_post_process_output(
            long.trim(),
            "{\"transcription\":\"gone\"}",
            PostProcessOutputMode::StructuredJson,
        )
        .unwrap_err();
        assert_eq!(failure.skip_reason, SkipReason::LengthGuard);
        let failure =
            validate_post_process_output(long.trim(), "gone", PostProcessOutputMode::FreeText)
                .unwrap_err();
        assert_eq!(failure.skip_reason, SkipReason::LengthGuard);
        // Free text: happy path passes through stripped.
        assert_eq!(
            validate_post_process_output(
                "hello world",
                "Hello, world.",
                PostProcessOutputMode::FreeText
            )
            .unwrap(),
            "Hello, world."
        );
        // Empty free text never validates.
        let failure =
            validate_post_process_output("hello world", "  ", PostProcessOutputMode::FreeText)
                .unwrap_err();
        assert_eq!(failure.skip_reason, SkipReason::EngineFailed);
    }
}
