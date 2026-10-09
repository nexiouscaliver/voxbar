use crate::managers::model::{
    resolve_hf_repo, HfModelError, HfModelResolution, ModelInfo, ModelManager,
};
use crate::managers::transcription::{asr_load_refusal, ModelStateEvent, TranscriptionManager};
use crate::settings::{get_settings, write_settings, AppSettings, ModelUnloadTimeout};
use log::error;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};

/// Stable prefix marking the selection refusal (the post-process LLM is not
/// an ASR model). The settings UI matches this prefix to word the row-level
/// error as a localized explanation instead of the raw refusal text, the
/// same convention as [`crate::local_llm::SWAP_REFUSAL_PREFIX`].
pub const SELECTION_REFUSAL_PREFIX: &str = "not-an-asr-model";

/// Pure selection guard, shared by every command that persists an ASR model
/// selection: the local post-process LLM must never become the selected ASR
/// model. With `unload_timeout = Immediately` the switch command persists
/// the selection WITHOUT loading, so without this guard the GGUF only blows
/// up at the next hotkey press (asr_load_refusal fires mid-session and the
/// speech is lost). Reusing the load path's own predicate keeps selection
/// and loading ruled by one table.
fn selection_guard_error(model_info: &ModelInfo) -> Option<String> {
    asr_load_refusal(&model_info.engine_type, &model_info.id).map(|msg| {
        format!(
            "{}: {} (it powers post-processing, not transcription)",
            SELECTION_REFUSAL_PREFIX, msg
        )
    })
}

#[tauri::command]
#[specta::specta]
pub async fn get_available_models(
    model_manager: State<'_, Arc<ModelManager>>,
) -> Result<Vec<ModelInfo>, String> {
    Ok(model_manager.get_available_models())
}

#[tauri::command]
#[specta::specta]
pub async fn get_model_info(
    model_manager: State<'_, Arc<ModelManager>>,
    model_id: String,
) -> Result<Option<ModelInfo>, String> {
    Ok(model_manager.get_model_info(&model_id))
}

/// Re-scan local sources (custom models dir + shared HF cache) for models added
/// since launch
#[tauri::command]
#[specta::specta]
pub async fn rescan_local_models(
    model_manager: State<'_, Arc<ModelManager>>,
) -> Result<(), String> {
    let mm = model_manager.inner().clone();
    tokio::task::spawn_blocking(move || mm.rescan_local_models())
        .await
        .map_err(|e| format!("rescan task panicked: {e}"))?
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn download_model(
    app_handle: AppHandle,
    model_manager: State<'_, Arc<ModelManager>>,
    model_id: String,
) -> Result<(), String> {
    let result = model_manager
        .download_model(&model_id)
        .await
        .map_err(|e| e.to_string());

    if let Err(ref error) = result {
        // Log as well as emit: the toast is transient, and failed downloads have
        // historically been undiagnosable because logs showed nothing (#1579).
        error!("Model download failed for {}: {}", model_id, error);
        let _ = app_handle.emit(
            "model-download-failed",
            serde_json::json!({ "model_id": &model_id, "error": error }),
        );
    }

    result
}

#[tauri::command]
#[specta::specta]
pub async fn delete_model(
    app_handle: AppHandle,
    model_manager: State<'_, Arc<ModelManager>>,
    transcription_manager: State<'_, Arc<TranscriptionManager>>,
    llm_manager: State<'_, Arc<crate::local_llm::manager::LlmManager>>,
    model_id: String,
) -> Result<(), String> {
    // L3: a post-process swap may be about to restore the very model file
    // this delete would remove. A transient, retryable refusal; a cheap
    // non-blocking probe, never a wait (waiting would freeze the settings
    // UI for up to a minute behind the swap).
    if llm_manager.swap_in_progress() {
        // The stable prefix lets the settings UI tell this transient
        // refusal apart from real delete failures (see SWAP_REFUSAL_PREFIX).
        return Err(format!(
            "{}: post-processing is in progress, try again in a moment",
            crate::local_llm::SWAP_REFUSAL_PREFIX
        ));
    }

    // If deleting the active model, unload it and clear the setting
    let settings = get_settings(&app_handle);
    if settings.selected_model == model_id {
        // Waits for the worker to exit (behind any transcription in progress)
        // before the file is deleted, so keep it off the async workers.
        let tm = Arc::clone(&transcription_manager);
        tauri::async_runtime::spawn_blocking(move || tm.unload_model())
            .await
            .map_err(|e| format!("Failed to unload model: {}", e))?
            .map_err(|e| format!("Failed to unload model: {}", e))?;

        let mut settings = get_settings(&app_handle);
        settings.selected_model = String::new();
        write_settings(&app_handle, settings);
    }

    model_manager
        .delete_model(&model_id)
        .map_err(|e| e.to_string())
}

/// Shared logic for switching the active model, used by both the Tauri command
/// and the tray menu handler.
///
/// Validates the model, updates the persisted setting, and loads the model
/// unless the unload timeout is set to "Immediately" (in which case the model
/// will be loaded on-demand during the next transcription).
pub fn switch_active_model(app: &AppHandle, model_id: &str) -> Result<(), String> {
    let model_manager = app.state::<Arc<ModelManager>>();
    let transcription_manager = app.state::<Arc<TranscriptionManager>>();

    // Atomically claim the loading slot - prevents concurrent model loads
    // from tray double-clicks or overlapping commands. The guard resets the
    // flag on drop (including early returns, errors, and panics).
    let _loading_guard = transcription_manager
        .try_start_loading()
        .ok_or_else(|| "Model load already in progress".to_string())?;

    // Check if model exists and is available
    let model_info = model_manager
        .get_model_info(model_id)
        .ok_or_else(|| format!("Model not found: {}", model_id))?;

    if !model_info.is_downloaded {
        return Err(format!("Model not downloaded: {}", model_id));
    }

    // Selection guard: reject the post-process LLM BEFORE anything is
    // persisted. Nothing was written, so nothing needs reverting; the error
    // carries the stable prefix the settings UI localizes against.
    if let Some(error_msg) = selection_guard_error(&model_info) {
        return Err(error_msg);
    }

    let settings = get_settings(app);
    let unload_timeout = settings.model_unload_timeout;
    let old_model = settings.selected_model.clone();
    let old_onboarding_completed = settings.onboarding_completed;

    // Persist the new selection early so the frontend sees the correct model
    // when it reacts to events emitted by load_model.
    let mut settings = settings;
    settings.selected_model = model_id.to_string();
    settings.onboarding_completed = true;

    write_settings(app, settings);

    // Skip eager loading if unload is set to "Immediately" - the model
    // will be loaded on-demand during the next transcription.
    if unload_timeout == ModelUnloadTimeout::Immediately {
        // Notify frontend - load_model won't be called so no events
        // would otherwise be emitted.
        let _ = app.emit(
            "model-state-changed",
            ModelStateEvent {
                event_type: "selection_changed".to_string(),
                model_id: Some(model_id.to_string()),
                model_name: Some(model_info.name.clone()),
                error: None,
                memory_gate: None,
            },
        );
        log::info!(
            "Model selection changed to {} (not loading - unload set to Immediately).",
            model_id
        );
        return Ok(());
    }

    // Load the model. On failure, revert the persisted selection.
    if let Err(e) = transcription_manager.load_model(model_id) {
        let mut settings = get_settings(app);
        settings.selected_model = old_model;
        settings.onboarding_completed = old_onboarding_completed;
        write_settings(app, settings);
        return Err(e.to_string());
    }

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn set_active_model(
    app_handle: AppHandle,
    _model_manager: State<'_, Arc<ModelManager>>,
    _transcription_manager: State<'_, Arc<TranscriptionManager>>,
    model_id: String,
) -> Result<(), String> {
    switch_active_model(&app_handle, &model_id)
}

/// The deferred selection's settings effect, pure so it is unit-testable:
/// the model becomes the persisted selection and onboarding completes,
/// with nothing loaded. Contrast `switch_active_model`'s failure path,
/// which reverts BOTH fields to their pre-attempt values - the state the
/// v1.0.2 first-run dead end was stuck in.
fn apply_deferred_selection(settings: &mut AppSettings, model_id: &str) {
    settings.selected_model = model_id.to_string();
    settings.onboarding_completed = true;
}

/// Persist the model selection WITHOUT loading it: the recovery path for a
/// first-run memory-gate refusal. Onboarding can complete with a downloaded
/// but unloaded model, and the first hotkey press loads it on demand (the
/// same on-demand load the "Immediately" unload timeout uses). Validations
/// mirror `switch_active_model`; on an unknown or undownloaded model the
/// settings are never touched. Nothing loads, so no loading-slot claim is
/// needed.
#[tauri::command]
#[specta::specta]
pub async fn set_active_model_deferred(
    app_handle: AppHandle,
    model_manager: State<'_, Arc<ModelManager>>,
    model_id: String,
) -> Result<(), String> {
    let model_info = model_manager
        .get_model_info(&model_id)
        .ok_or_else(|| format!("Model not found: {}", model_id))?;

    if !model_info.is_downloaded {
        return Err(format!("Model not downloaded: {}", model_id));
    }

    // Same guard as switch_active_model: the deferred path persists without
    // loading too, so the GGUF must be refused before the store is touched.
    if let Some(error_msg) = selection_guard_error(&model_info) {
        return Err(error_msg);
    }

    let mut settings = get_settings(&app_handle);
    apply_deferred_selection(&mut settings, &model_id);
    write_settings(&app_handle, settings);

    let _ = app_handle.emit(
        "model-state-changed",
        ModelStateEvent {
            event_type: "selection_changed".to_string(),
            model_id: Some(model_id.clone()),
            model_name: Some(model_info.name.clone()),
            error: None,
            memory_gate: None,
        },
    );
    log::info!(
        "Model selection changed to {} (deferred; it loads on the next dictation).",
        model_id
    );
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn get_current_model(app_handle: AppHandle) -> Result<String, String> {
    let settings = get_settings(&app_handle);
    Ok(settings.selected_model)
}

#[tauri::command]
#[specta::specta]
pub async fn get_transcription_model_status(
    transcription_manager: State<'_, Arc<TranscriptionManager>>,
) -> Result<Option<String>, String> {
    Ok(transcription_manager.get_current_model())
}

#[tauri::command]
#[specta::specta]
pub async fn is_model_loading(
    transcription_manager: State<'_, Arc<TranscriptionManager>>,
) -> Result<bool, String> {
    // Check if transcription manager has a loaded model
    let current_model = transcription_manager.get_current_model();
    Ok(current_model.is_none())
}

#[tauri::command]
#[specta::specta]
pub async fn cancel_download(
    model_manager: State<'_, Arc<ModelManager>>,
    model_id: String,
) -> Result<(), String> {
    model_manager
        .cancel_download(&model_id)
        .map_err(|e| e.to_string())
}

/// Resolve pasted Hugging Face input (a URL, `owner/repo`, or
/// `owner/repo/file.gguf`) into the repo's GGUF file list with sizes plus a
/// suggested file. Public repos only in v1: repos that cannot be read
/// anonymously come back as a structured error the UI can localize.
#[tauri::command]
#[specta::specta]
pub async fn resolve_hf_model(input: String) -> Result<HfModelResolution, HfModelError> {
    resolve_hf_repo(&input).await
}

/// Download a specific file from a Hugging Face repo and register it, gated
/// on the GGUF architecture probe: an unsupported architecture is refused,
/// the blob deleted, and the error names the architecture and supported
/// families. Downloads nothing outside this explicit user action.
#[tauri::command]
#[specta::specta]
pub async fn add_hf_model(
    model_manager: State<'_, Arc<ModelManager>>,
    repo_id: String,
    filename: String,
    revision: Option<String>,
) -> Result<String, HfModelError> {
    model_manager
        .add_hf_model(&repo_id, &filename, revision.as_deref())
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managers::model::{local_llm_model_info, EngineType};
    use crate::settings::get_default_settings;

    /// The deferred selection persists the model and completes onboarding
    /// WITHOUT loading: the recovery a first-run memory-gate refusal offers
    /// ("continue without loading now"). The first hotkey press loads the
    /// model on demand. This is deliberately the opposite of
    /// `switch_active_model`'s failure path, which reverts both fields -
    /// the state the v1.0.2 first-run dead end was stuck in.
    #[test]
    fn deferred_selection_persists_selection_and_completes_onboarding() {
        let mut settings = get_default_settings();
        settings.selected_model = String::new();
        settings.onboarding_completed = false;
        apply_deferred_selection(&mut settings, "whisper-tiny-q8");
        assert_eq!(settings.selected_model, "whisper-tiny-q8");
        assert!(settings.onboarding_completed);
    }

    /// The selection guard rejects the local post-process LLM (the Qwen GGUF)
    /// at selection time with the stable prefix the settings UI matches, so
    /// the Immediately branch of `switch_active_model` (persist-without-load)
    /// can never persist it and blow up at the next hotkey press instead.
    #[test]
    fn selection_guard_rejects_the_post_process_llm_up_front() {
        let llm = local_llm_model_info();
        let refusal = selection_guard_error(&llm)
            .expect("the post-process GGUF must be refused at selection time");
        assert!(
            refusal.starts_with(SELECTION_REFUSAL_PREFIX),
            "the refusal must carry the stable prefix, got: {refusal}"
        );
        assert!(refusal.contains(&llm.id), "the refusal names the model id");
    }

    /// Every real ASR engine type passes the guard: only the LocalLlm engine
    /// is refused, so no legitimate model selection is blocked by it.
    #[test]
    fn selection_guard_accepts_every_real_asr_engine() {
        let engines = [
            EngineType::TranscribeCpp,
            EngineType::Parakeet,
            EngineType::Moonshine,
            EngineType::MoonshineStreaming,
            EngineType::SenseVoice,
            EngineType::GigaAM,
            EngineType::Canary,
            EngineType::Cohere,
        ];
        for engine in engines {
            let mut info = local_llm_model_info();
            info.engine_type = engine.clone();
            assert!(
                selection_guard_error(&info).is_none(),
                "{engine:?} is a real ASR engine and must be selectable"
            );
        }
    }
}
