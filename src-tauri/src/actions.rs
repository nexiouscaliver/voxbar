#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use crate::apple_intelligence;
use crate::audio_feedback::{play_feedback_sound, play_feedback_sound_blocking, SoundType};
use crate::audio_toolkit::{is_microphone_access_denied, is_no_input_device_error, VadPolicy};
use crate::managers::audio::{AudioRecordingManager, CaptureSource};
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
    /// Where this binding's audio comes from: the local cpal microphone, or
    /// a companion device pushing over the LAN. Everything else about the
    /// session (model gate, tray, overlay, streaming, chime, transcription,
    /// post-process, paste) is shared verbatim for both sources.
    source: CaptureSource,
}

/// Pure routing table from a transcribe binding id to its
/// [`TranscribeAction`] configuration: `post_process` and the capture
/// source.
///
/// Extracted from the ACTION_MAP literals so a test can pin the safety
/// property the operator relies on: only the dictation bindings
/// ("transcribe", "transcribe_with_post_process", "transcribe_companion")
/// route here. The command-mode binding ("transcribe_commands") is NOT a
/// recording action at all - it is a during-dictation modifier routed to
/// the coordinator's `send_command_modifier`, so no binding id can ever
/// start a command capture recording.
fn transcribe_action_config(binding_id: &str) -> Option<(bool, CaptureSource)> {
    match binding_id {
        "transcribe" => Some((false, CaptureSource::Local)),
        "transcribe_with_post_process" => Some((true, CaptureSource::Local)),
        "transcribe_companion" => Some((false, CaptureSource::Remote)),
        _ => None,
    }
}

// THE shared output validator lives in post_process_runs.rs (pure,
// colocated with the lifecycle that reports its failures) and is re-exported
// here: every engine's success path (cloud structured, cloud legacy, Apple
// Intelligence, the local swap runner) validates through the one function,
// so local and cloud enforce identical rules (empty, length ratio both
// ways, language sanity).
pub(crate) use crate::post_process_runs::{
    strip_invisible_chars, strip_think_block, validate_post_process_output, PostProcessOutputMode,
    TRANSCRIPTION_FIELD,
};

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
/// network, a provider, or a Tauri app. Failures are the structured
/// [`PostProcessError`] (class + detail + retry count); success carries the
/// endpoint's content plus how many bounded network retries it took (the
/// pp: generation phase reports the total).
pub(crate) type CloudSendResult =
    Result<crate::llm_client::PostProcessCompletion, crate::llm_client::PostProcessError>;

pub(crate) trait CloudCompletionSeam: Send {
    fn send(
        &mut self,
        user_content: String,
        system_prompt: Option<String>,
        json_schema: Option<serde_json::Value>,
    ) -> std::pin::Pin<Box<dyn Future<Output = CloudSendResult> + Send + '_>>;
}

/// The production seam: one `send` per attempt, carrying everything the
/// provider needs (the legacy prompt shape is the schema-less call). The
/// stop path's cancellation closure rides along so llm_client's bounded
/// network retry never re-sends a request the dictation outlived.
struct LlmClientSeam {
    provider: crate::settings::PostProcessProvider,
    api_key: String,
    model: String,
    disable_reasoning: bool,
    timeout_secs: u64,
    is_cancelled: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
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
        let is_cancelled = self.is_cancelled.clone();
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
                is_cancelled
                    .as_ref()
                    .map(|closure| closure.as_ref() as &(dyn Fn() -> bool + Send + Sync)),
            )
            .await
        })
    }
}

/// The cloud/Apple lifecycle after the `requested` phase was already
/// emitted by the caller: engine phase (placeholders; no local swap), one
/// structured attempt with a legacy retry, validation on every success,
/// classified outcome on every failure. Returns the processed text or None
/// (the raw transcript always wins on failure). The generation phase's
/// retry count totals the transport retries (llm_client's bounded
/// network-only wrapper) plus the structured-to-legacy fallback.
async fn run_cloud_lifecycle(
    app: Option<&AppHandle>,
    seam: &mut dyn CloudCompletionSeam,
    run_id: u64,
    transcript: &str,
    system_prompt: Option<String>,
    structured_supported: bool,
    legacy_prompt: String,
    template_language: Option<&str>,
    notify: impl Fn(crate::managers::transcription::NoticeCode, Option<String>),
) -> Option<String> {
    use crate::llm_client::PostProcessCompletion;
    use crate::llm_client::PostProcessFailureClass;
    use crate::post_process_runs::{runs, PostProcessOutcome};
    // Engine phase: cloud engines have no model load, no cache, no swap.
    runs().engine_phase(app, run_id, None, None);

    let generation_started = Instant::now();
    let mut retries: u32 = 0;

    // The terminal report shared by every arm below: one generation line
    // (ms + total retries) and one outcome line.
    macro_rules! conclude_generation {
        ($outcome:expr) => {{
            let ms = generation_started.elapsed().as_millis() as u64;
            runs().generation_phase(app, run_id, Some(ms), Some(retries));
            runs().finish(app, run_id, $outcome, None);
        }};
    }

    // A success carries content plus its transport retries; validation
    // decides whether the content pastes. Shared by both attempt shapes.
    macro_rules! handle_completion {
        ($completion:expr, $mode:expr) => {{
            let completion: PostProcessCompletion = $completion;
            retries += completion.transport_retries;
            match completion.content {
                Some(content) => {
                    // THE shared validator: nothing reaches the paste
                    // without passing it. A validation failure falls back
                    // to the raw transcript and says so through the notice
                    // channel.
                    return match validate_post_process_output(
                        transcript,
                        &content,
                        $mode,
                        template_language,
                    ) {
                        Ok(result) => {
                            let ms =
                                generation_started.elapsed().as_millis() as u64;
                            runs().generation_phase(app, run_id, Some(ms), Some(retries));
                            runs().finish(
                                app,
                                run_id,
                                PostProcessOutcome::Applied,
                                Some(result.chars().count() as u64),
                            );
                            debug!(
                                "Post-processing succeeded. Output length: {} chars",
                                result.len()
                            );
                            Some(result)
                        }
                        Err(failure) => {
                            let ms =
                                generation_started.elapsed().as_millis() as u64;
                            runs().generation_phase(app, run_id, Some(ms), Some(retries));
                            runs().finish(
                                app,
                                run_id,
                                PostProcessOutcome::Failed {
                                    class: PostProcessFailureClass::OutputInvalid,
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
                None => {
                    warn!("Post-process failed: the API response had no content");
                    notify(
                        crate::managers::transcription::NoticeCode::PostProcessCloudFailed,
                        Some("the API response had no content".to_string()),
                    );
                    conclude_generation!(PostProcessOutcome::Failed {
                        class: PostProcessFailureClass::OutputInvalid,
                    });
                    return None;
                }
            }
        }};
    }

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
            Ok(completion) => {
                handle_completion!(completion, PostProcessOutputMode::StructuredJson);
            }
            Err(e) => {
                // A cancelled dictation never falls back to a second
                // request shape: the user is done waiting.
                if e.class == PostProcessFailureClass::Cancelled {
                    retries += e.retries;
                    warn!("Post-process cancelled mid-request: {e}");
                    conclude_generation!(PostProcessOutcome::Failed {
                        class: PostProcessFailureClass::Cancelled,
                    });
                    return None;
                }
                // Every other failure falls through to the legacy attempt;
                // the fallback counts as one retry (today's behavior, now
                // visible in the record alongside the transport retries).
                retries += 1 + e.retries;
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
        Ok(completion) => {
            handle_completion!(completion, PostProcessOutputMode::FreeText);
        }
        Err(e) => {
            retries += e.retries;
            conclude_generation!(PostProcessOutcome::Failed { class: e.class });
            warn!(
                "LLM post-processing failed ({}): {}. Using the raw transcript.",
                crate::post_process_runs::failure_class_str(e.class),
                e.detail
            );
            notify(
                crate::managers::transcription::NoticeCode::PostProcessCloudFailed,
                Some(e.detail),
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

    post_process_with_prompt(
        Some(app),
        settings,
        binding,
        transcription,
        &prompt_entry,
        is_cancelled,
    )
    .await
}

/// Run ONE resolved template through the engine lifecycle. Shared by the
/// dictation path above (the selected template) and the per-template
/// "test on my last transcript" command, so the pp: lifecycle records both
/// identically. `app` is optional: the engines that need app state (the
/// local swap, Apple Intelligence) record an engine skip without it, which
/// is what the unit tests exercise; the cloud path runs fine without one.
///
/// Never pastes and never writes history: those live in the callers.
async fn post_process_with_prompt(
    app: Option<&AppHandle>,
    settings: &AppSettings,
    binding: &str,
    transcription: &str,
    prompt_entry: &crate::settings::LLMPrompt,
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
        app,
        crate::post_process_runs::RunRequestMeta {
            binding: binding.to_string(),
            engine: engine_kind,
            provider_id: provider.id.clone(),
            model: model.clone(),
            prompt_id: Some(prompt_entry.id.clone()),
            prompt_name: Some(prompt_entry.name.clone()),
            // The template library's version and language tokens: stamped
            // into every run record so the Debug table can tell a user
            // edit of a template from its seeded original.
            prompt_version: Some(prompt_entry.version.to_string()),
            template_language: Some(prompt_entry.language.clone()),
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
        match app {
            Some(app) => {
                run_local_lifecycle(
                    app,
                    run_id,
                    settings,
                    transcription,
                    &prompt_entry.prompt,
                    &prompt_entry.language,
                    is_cancelled,
                )
                .await
            }
            None => {
                debug!("Local post-process engine needs an app handle; skipping");
                crate::post_process_runs::runs().finish(
                    None,
                    run_id,
                    crate::post_process_runs::PostProcessOutcome::Skipped {
                        reason: crate::local_llm::SkipReason::EngineFailed,
                    },
                    None,
                );
                None
            }
        }
    } else if provider.id == APPLE_INTELLIGENCE_PROVIDER_ID {
        match app {
            Some(app) => {
                run_apple_intelligence_lifecycle(
                    app,
                    run_id,
                    transcription,
                    &prompt_entry.prompt,
                    &model,
                    &prompt_entry.language,
                )
                .await
            }
            None => {
                debug!("Apple Intelligence needs an app handle; skipping");
                crate::post_process_runs::runs().finish(
                    None,
                    run_id,
                    crate::post_process_runs::PostProcessOutcome::Skipped {
                        reason: crate::local_llm::SkipReason::EngineFailed,
                    },
                    None,
                );
                None
            }
        }
    } else {
        let system_prompt = if provider.supports_structured_output {
            Some(build_system_prompt(&prompt_entry.prompt))
        } else {
            None
        };
        let legacy_prompt = prompt_entry.prompt.replace("${output}", transcription);
        let notify = |code: crate::managers::transcription::NoticeCode, detail: Option<String>| {
            if let Some(app) = app {
                crate::managers::transcription::emit_overlay_notice(app, code, detail);
            }
        };
        let mut seam = LlmClientSeam {
            provider: provider.clone(),
            api_key,
            model: model.clone(),
            disable_reasoning,
            timeout_secs: settings.post_process_timeout_secs_for(&provider.id),
            is_cancelled: is_cancelled.clone(),
        };
        run_cloud_lifecycle(
            app,
            &mut seam,
            run_id,
            transcription,
            system_prompt,
            provider.supports_structured_output,
            legacy_prompt,
            Some(prompt_entry.language.as_str()),
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

/// Binding tag the per-template tester records its pp: run under: the Debug
/// table's marker separating "the operator pressed Test on a template" from
/// real dictation runs.
pub(crate) const PROMPT_TEST_BINDING: &str = "prompt_test";

/// The outcome of testing one template against the last transcript. `after`
/// is None when the engine failed or skipped (the raw transcript would be
/// kept on the dictation path); `outcome` is the same token the run record
/// and history carry (`applied` | `skipped:<reason>` | `failed:<class>`,
/// or `skipped` for pre-run config states that never mint a run).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, specta::Type)]
pub struct PromptTestOutcome {
    pub before: String,
    pub after: Option<String>,
    pub outcome: String,
    pub latency_ms: u64,
}

/// Structured error for the test command: the tag is the failure the UI
/// keys on (`no_history` when there is nothing to test against,
/// `prompt_not_found` for a dangling template id, `other` for a history
/// store read failure).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TestPromptError {
    NoHistory,
    PromptNotFound { id: String },
    Other { detail: String },
}

impl std::fmt::Display for TestPromptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TestPromptError::NoHistory => {
                write!(f, "no_history: no transcription history to test against")
            }
            TestPromptError::PromptNotFound { id } => {
                write!(f, "prompt_not_found: {id}")
            }
            TestPromptError::Other { detail } => write!(f, "other: {detail}"),
        }
    }
}

/// Run one template over a transcript for the "test on my last transcript"
/// button: the same engine lifecycle a dictation runs (provider, model,
/// validator, pp: record), but without pasting and without writing any
/// history row. With `app` None (the unit-test path) the run is recorded
/// and there is structurally no way to reach the history pipeline.
pub(crate) async fn run_prompt_test(
    app: Option<&AppHandle>,
    settings: &AppSettings,
    prompt: &crate::settings::LLMPrompt,
    transcription: &str,
) -> PromptTestOutcome {
    let attempt = post_process_with_prompt(
        app,
        settings,
        PROMPT_TEST_BINDING,
        transcription,
        prompt,
        None,
    )
    .await;
    match attempt.summary {
        Some(summary) => PromptTestOutcome {
            before: transcription.to_string(),
            after: attempt.text,
            outcome: summary.outcome,
            latency_ms: summary.latency_ms,
        },
        None => PromptTestOutcome {
            before: transcription.to_string(),
            after: attempt.text,
            outcome: "skipped".to_string(),
            latency_ms: 0,
        },
    }
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
    template_language: &str,
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
        template_language: Some(template_language.to_string()),
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
    template_language: &str,
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
                    Some(template_language),
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
        // The local VAD warm-up only pays off for the local capture path;
        // the companion server pre-warms the remote recorder when enabled,
        // so a phone press skips this entirely.
        if matches!(self.source, CaptureSource::Local) {
            let rm_clone = Arc::clone(&rm);
            std::thread::spawn(move || {
                if let Err(e) = rm_clone.preload_vad() {
                    debug!("VAD pre-load failed: {}", e);
                }
            });
        }
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
        match rm.try_start_recording_for(self.source, &binding_id, vad_policy) {
            Ok(readiness) => {
                debug!(
                    "Recording request accepted in {:?}; waiting for first microphone samples",
                    recording_start_time.elapsed()
                );
                if matches!(self.source, CaptureSource::Remote) {
                    crate::companion::on_session_changed(app, true);
                }
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
                if matches!(self.source, CaptureSource::Remote) {
                    // The badge must never outlive a start that did not
                    // begin recording.
                    crate::companion::on_session_changed(app, false);
                }
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

        if matches!(self.source, CaptureSource::Remote) {
            // The phone session ends here; the pipeline below (finalize,
            // transcribe, paste) is the shared one.
            crate::companion::on_session_changed(app, false);
        }

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

// Cycle Post-Process Prompt Action
struct CyclePromptAction;

impl ShortcutAction for CyclePromptAction {
    fn start(&self, app: &AppHandle, _binding_id: &str, _shortcut_str: &str) {
        // Ships unbound and rides the post-process master toggle: inert on
        // stock installs, and off whenever post-processing itself is off.
        if !get_settings(app).post_process_enabled {
            debug!("Cycle-prompt action disabled with post-processing off");
            return;
        }
        if let Err(e) = shortcut::cycle_prompt_and_notify(app) {
            debug!("Cycle post-process prompt refused: {}", e);
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
    for binding_id in [
        "transcribe",
        "transcribe_with_post_process",
        "transcribe_companion",
    ] {
        let (post_process, source) =
            transcribe_action_config(binding_id).expect("known transcribe binding id");
        map.insert(
            binding_id.to_string(),
            Arc::new(TranscribeAction {
                post_process,
                source,
            }) as Arc<dyn ShortcutAction>,
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
        "cycle_post_process_prompt".to_string(),
        Arc::new(CyclePromptAction) as Arc<dyn ShortcutAction>,
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

    /// WS5: a cancelled flag during a cloud call returns within ONE poll
    /// interval of the flip. The Escape binding's mid-post-process escape
    /// depends on the poll loop noticing the cancel at the next 25ms tick,
    /// never on the request's own timeout.
    #[test]
    fn cancelled_cloud_call_returns_within_one_poll_interval() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancelled_for_thread = Arc::clone(&cancelled);
        // A "cloud request" that never resolves on its own: only the
        // cancel poll can end it.
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            let flipped_at = std::time::Instant::now();
            cancelled_for_thread.store(true, Ordering::Release);
            flipped_at
        });

        let started = std::time::Instant::now();
        let result = tauri::async_runtime::block_on(complete_unless_cancelled(
            future::pending::<Option<String>>(),
            || cancelled.load(Ordering::Acquire),
        ));
        let elapsed = started.elapsed();

        assert_eq!(
            result, None,
            "a cancelled cloud call resolves None: the raw transcript is pasted"
        );
        // 30ms flip + at most one poll interval (25ms) + scheduler slack.
        assert!(
            elapsed < Duration::from_millis(30 + 25 + 50),
            "the cancel must land within one poll interval (took {elapsed:?})"
        );
    }

    /// WS5: the cancelled cloud run's observability. The stop pipeline
    /// drops the post-process future and calls
    /// cancel_live_dictation_runs; the live dictation run must conclude
    /// failed(cancelled) so the Debug table never shows a phantom live
    /// run, while a history-retry run stays live (it concludes itself).
    #[test]
    fn cancelled_dictation_concludes_its_run_failed_cancelled() {
        // The pipeline's run, begun the moment the cloud call started. On
        // an ISOLATED registry: the sweep cancels every live dictation
        // run, and the parallel tests hold live runs of their own.
        let registry = crate::post_process_runs::RunsRegistry::isolated_for_test();
        let run_id = registry.begin(
            None,
            crate::post_process_runs::RunRequestMeta {
                binding: "transcribe_with_post_process".to_string(),
                engine: crate::post_process_runs::PostProcessEngineKind::Cloud,
                provider_id: "openai".to_string(),
                model: "gpt-4o-mini".to_string(),
                prompt_id: Some("clean".to_string()),
                prompt_name: None,
                prompt_version: None,
                template_language: None,
                chars_in: 42,
            },
        );
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancelled_for_thread = Arc::clone(&cancelled);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(10));
            cancelled_for_thread.store(true, Ordering::Release);
        });

        let processed = tauri::async_runtime::block_on(complete_unless_cancelled(
            future::pending::<Option<String>>(),
            || cancelled.load(Ordering::Acquire),
        ));

        // The stop pipeline's exact tail on this arm: conclude the run,
        // keep the raw transcript (processed is None).
        assert_eq!(processed, None);
        registry.cancel_live_dictation_runs();
        let record = registry.snapshot(run_id).unwrap();
        assert_eq!(
            record.outcome,
            Some(crate::post_process_runs::PostProcessOutcome::Failed {
                class: crate::llm_client::PostProcessFailureClass::Cancelled
            }),
            "the cancelled run records failed(cancelled)"
        );
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
        assert!(ACTION_MAP.contains_key("cycle_post_process_prompt"));
        assert!(!crate::transcription_coordinator::is_transcribe_binding(
            "delete_last_word"
        ));
        assert!(!crate::transcription_coordinator::is_transcribe_binding(
            "undo"
        ));
        assert!(!crate::transcription_coordinator::is_transcribe_binding(
            "cycle_post_process_prompt"
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
        use crate::managers::audio::CaptureSource;

        assert_eq!(
            transcribe_action_config("transcribe"),
            Some((false, CaptureSource::Local))
        );
        assert_eq!(
            transcribe_action_config("transcribe_with_post_process"),
            Some((true, CaptureSource::Local))
        );
        // The companion trigger is the remote capture source: same shared
        // pipeline, audio from the phone.
        assert_eq!(
            transcribe_action_config("transcribe_companion"),
            Some((false, CaptureSource::Remote))
        );
        assert!(
            ACTION_MAP.contains_key("transcribe_companion"),
            "the companion binding must dispatch through the shared TranscribeAction"
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
        // The cycle binding is an assignable action, never a recording one.
        assert_eq!(transcribe_action_config("cycle_post_process_prompt"), None);
    }

    /// The per-template tester: with a dead endpoint the run is recorded
    /// under the `prompt_test` binding marker (the Debug table's tag), the
    /// prompt id/version/language are stamped into the record, the outcome
    /// is the run's failure class, and the raw transcript comes back as
    /// `before` with no `after`. No history write is possible on this path:
    /// it runs without an app handle and shares nothing with the history
    /// pipeline.
    #[test]
    fn prompt_test_records_a_marker_run_and_returns_before_after_outcome() {
        use super::{run_prompt_test, PROMPT_TEST_BINDING};

        let mut settings = crate::settings::get_default_settings();
        // A cloud provider aimed at a port nothing listens on: the request
        // fails fast with a transport error (network class), deterministically.
        settings.post_process_provider_id = "custom".to_string();
        if let Some(provider) = settings
            .post_process_providers
            .iter_mut()
            .find(|p| p.id == "custom")
        {
            provider.base_url = "http://127.0.0.1:1/v1".to_string();
        }
        settings
            .post_process_models
            .insert("custom".to_string(), "test-model".to_string());

        let prompt = crate::settings::builtin_prompt_seeds()
            .into_iter()
            .find(|p| p.id == "english_casual")
            .unwrap();

        let outcome = tauri::async_runtime::block_on(run_prompt_test(
            None,
            &settings,
            &prompt,
            "hey um world",
        ));

        assert_eq!(outcome.before, "hey um world");
        assert_eq!(outcome.after, None, "a failed run never rewrites the text");
        assert!(
            outcome.outcome.starts_with("failed:"),
            "the transport failure is classified: {}",
            outcome.outcome
        );

        // The run is in the registry under the test marker, with the
        // template's id/version/language stamped (the WS3 fields). Looked
        // up by marker (tests share the process-wide registry).
        let record = crate::post_process_runs::runs()
            .latest(None)
            .into_iter()
            .find(|r| {
                r.binding == PROMPT_TEST_BINDING && r.prompt_id.as_deref() == Some("english_casual")
            })
            .expect("the test run is recorded under the prompt_test marker");
        assert_eq!(record.prompt_version.as_deref(), Some("1"));
        assert_eq!(record.template_language.as_deref(), Some("en"));
    }

    /// The local engine under the tester without an app handle: there is
    /// no model manager to consult, so the run is recorded as an engine
    /// skip and the outcome token says so (the raw transcript is kept).
    #[test]
    fn prompt_test_local_engine_without_app_records_an_engine_skip() {
        use super::{run_prompt_test, PROMPT_TEST_BINDING};

        let mut settings = crate::settings::get_default_settings();
        settings.post_process_provider_id = crate::settings::LOCAL_LLM_PROVIDER_ID.to_string();

        let prompt = crate::settings::builtin_prompt_seeds().remove(0);

        let outcome = tauri::async_runtime::block_on(run_prompt_test(
            None,
            &settings,
            &prompt,
            "hello there",
        ));

        assert_eq!(outcome.before, "hello there");
        assert_eq!(outcome.after, None);
        assert_eq!(outcome.outcome, "skipped:engine_failed");
        assert!(
            crate::post_process_runs::runs()
                .latest(None)
                .into_iter()
                .any(|r| r.binding == PROMPT_TEST_BINDING
                    && r.prompt_id.as_deref() == Some("default_improve_transcriptions")),
            "the skip run is recorded under the prompt_test marker"
        );
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

    /// WS5: the dictation post-process path driven against a scripted
    /// cloud "engine" (a one-shot HTTP server) that answers an English
    /// transcript in Japanese: the shared validator's language-sanity rule
    /// rejects the output, the run records output_invalid, and None comes
    /// back so the raw transcript pastes. Driven through
    /// `post_process_with_prompt` (the engine path the dictation entry
    /// resolves its selected prompt into; the resolution wrapper needs a
    /// Wry app handle, the engine path takes Option).
    #[test]
    fn post_process_transcription_against_a_garbage_engine_falls_back_raw() {
        use crate::actions::post_process_with_prompt;

        // One canned 200 whose content is a Japanese "cleanup".
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let body = format!(
            "{{\"choices\":[{{\"message\":{{\"content\":\"こんにちは、これはテストです。\"}}}}]}}"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                use std::io::{Read, Write};
                let mut request = [0_u8; 8192];
                let _ = stream.read(&mut request);
                let _ = stream.write_all(response.as_bytes());
            }
        });

        let mut settings = crate::settings::get_default_settings();
        settings.post_process_provider_id = "custom".to_string();
        if let Some(provider) = settings
            .post_process_providers
            .iter_mut()
            .find(|p| p.id == "custom")
        {
            provider.base_url = format!("http://{address}/v1");
        }
        settings
            .post_process_models
            .insert("custom".to_string(), "test-model".to_string());
        let seed = crate::settings::builtin_prompt_seeds()
            .into_iter()
            .find(|p| p.id == "english_casual")
            .unwrap();
        settings.post_process_prompts = vec![seed.clone()];
        settings.post_process_selected_prompt_id = Some(seed.id.clone());

        let transcript = "hello um world this is a test transcript with words";
        let attempt = tauri::async_runtime::block_on(post_process_with_prompt(
            None,
            &settings,
            "transcribe_with_post_process",
            transcript,
            &seed,
            None,
        ));

        assert_eq!(
            attempt.text, None,
            "garbage output never replaces the transcript"
        );
        let summary = attempt.summary.expect("the run is recorded");
        assert_eq!(
            summary.outcome, "failed:output_invalid",
            "the script switch classifies output_invalid"
        );
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

    /// A canned success carrying content and transport retries.
    fn cloud_ok(content: &str, transport_retries: u32) -> super::CloudSendResult {
        Ok(crate::llm_client::PostProcessCompletion {
            content: Some(content.to_string()),
            transport_retries,
        })
    }

    /// A canned structured failure.
    fn cloud_err(
        class: crate::llm_client::PostProcessFailureClass,
        detail: &str,
    ) -> super::CloudSendResult {
        Err(crate::llm_client::PostProcessError::new(
            class,
            detail.to_string(),
        ))
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
        let mut seam = MockCloud::new(vec![cloud_ok("{\"transcription\":\"Hello, world.\"}", 0)]);
        let notices: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        let text = tauri::async_runtime::block_on(run_cloud_lifecycle(
            None,
            &mut seam,
            run_id,
            "um hello world",
            Some("clean it".to_string()),
            true,
            "clean: um hello world".to_string(),
            None,
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
    /// transcript is what gets pasted. The retry count totals the
    /// structured-to-legacy fallback (1) plus any transport retries (0:
    /// auth never earns one).
    #[test]
    fn cloud_401_is_failed_auth_and_raw_transcript() {
        let run_id = begin_cloud_run();
        // The seam pops from the END: legacy attempt first, structured second.
        let mut seam = MockCloud::new(vec![
            cloud_err(
                crate::llm_client::PostProcessFailureClass::Auth,
                "API request failed with status 401: unauthorized",
            ),
            cloud_err(
                crate::llm_client::PostProcessFailureClass::Auth,
                "API request failed with status 401: unauthorized",
            ),
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
            None,
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
        // The structured failure counted as one retry before legacy; the
        // auth class earned zero transport retries on either attempt.
        assert_eq!(record.phases_generation.retries, Some(1));
        assert_eq!(*seam.calls.lock().unwrap(), vec![true, false]);
        assert_eq!(
            notices.lock().unwrap().as_slice(),
            ["post_process_cloud_failed"],
            "cloud failures reach the notice channel"
        );
    }

    /// Transport retries ride the pp: generation phase on SUCCESS too: a
    /// request that needed one bounded network retry before answering
    /// reports retries=1 in the record, and the text still pastes.
    #[test]
    fn transport_retries_ride_the_generation_phase_on_success() {
        let run_id = begin_cloud_run();
        let mut seam = MockCloud::new(vec![cloud_ok("{\"transcription\":\"Hello, world.\"}", 1)]);
        let text = tauri::async_runtime::block_on(run_cloud_lifecycle(
            None,
            &mut seam,
            run_id,
            "um hello world",
            Some("clean it".to_string()),
            true,
            "clean: um hello world".to_string(),
            None,
            |_code, _detail| {},
        ));
        assert_eq!(text.as_deref(), Some("Hello, world."));
        let record = crate::post_process_runs::runs().snapshot(run_id).unwrap();
        assert_eq!(
            record.phases_generation.retries,
            Some(1),
            "the transport retry is visible in the generation phase"
        );
        assert_eq!(
            record.outcome,
            Some(crate::post_process_runs::PostProcessOutcome::Applied)
        );
    }

    /// A cancellation mid-request never falls back to the legacy attempt:
    /// exactly one call (the structured one), the run concludes
    /// failed(cancelled), and the raw transcript is used.
    #[test]
    fn cancelled_cloud_run_never_falls_back_and_concludes_cancelled() {
        let run_id = begin_cloud_run();
        let mut seam = MockCloud::new(vec![cloud_err(
            crate::llm_client::PostProcessFailureClass::Cancelled,
            "the dictation was cancelled before the retry ran",
        )
        .map_err(|e| e.with_retries(0))]);
        let text = tauri::async_runtime::block_on(run_cloud_lifecycle(
            None,
            &mut seam,
            run_id,
            "hello world",
            Some("clean it".to_string()),
            true,
            "clean: hello world".to_string(),
            None,
            |_code, _detail| {},
        ));
        assert_eq!(text, None, "a cancelled run pastes the raw transcript");
        assert_eq!(
            *seam.calls.lock().unwrap(),
            vec![true],
            "no legacy attempt after a cancellation"
        );
        let record = crate::post_process_runs::runs().snapshot(run_id).unwrap();
        assert_eq!(
            record.outcome,
            Some(crate::post_process_runs::PostProcessOutcome::Failed {
                class: crate::llm_client::PostProcessFailureClass::Cancelled
            })
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
            let mut seam = MockCloud::new(vec![cloud_ok(bad, 0)]);
            let notices: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
            let text = tauri::async_runtime::block_on(run_cloud_lifecycle(
                None,
                &mut seam,
                run_id,
                "hello world this is a longer transcript",
                Some("clean it".to_string()),
                true,
                "clean: hello world this is a longer transcript".to_string(),
                None,
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
                None,
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
                None,
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
            None,
        )
        .unwrap_err();
        assert_eq!(failure.skip_reason, SkipReason::LengthGuard);
        let failure = validate_post_process_output(
            long.trim(),
            "gone",
            PostProcessOutputMode::FreeText,
            None,
        )
        .unwrap_err();
        assert_eq!(failure.skip_reason, SkipReason::LengthGuard);
        // Free text: happy path passes through stripped.
        assert_eq!(
            validate_post_process_output(
                "hello world",
                "Hello, world.",
                PostProcessOutputMode::FreeText,
                None
            )
            .unwrap(),
            "Hello, world."
        );
        // Empty free text never validates.
        let failure = validate_post_process_output(
            "hello world",
            "  ",
            PostProcessOutputMode::FreeText,
            None,
        )
        .unwrap_err();
        assert_eq!(failure.skip_reason, SkipReason::EngineFailed);
    }
}
