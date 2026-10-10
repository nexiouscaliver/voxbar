use crate::managers::transcription::TranscriptionManager;
use crate::settings::{
    get_settings, write_settings, ModelUnloadTimeout, MODEL_UNLOAD_CUSTOM_MAX_SECONDS,
    MODEL_UNLOAD_CUSTOM_MIN_SECONDS, POST_PROCESS_TIMEOUT_MAX_SECONDS,
    POST_PROCESS_TIMEOUT_MIN_SECONDS,
};
use serde::Serialize;
use specta::Type;
use std::sync::Arc;
use tauri::{AppHandle, State};

#[derive(Serialize, Type)]
pub struct ModelLoadStatus {
    is_loaded: bool,
    current_model: Option<String>,
}

#[tauri::command]
#[specta::specta]
pub fn set_model_unload_timeout(app: AppHandle, timeout: ModelUnloadTimeout) {
    let mut settings = get_settings(&app);
    settings.model_unload_timeout = timeout;
    write_settings(&app, settings);

    // KB-200: the timeout rides MenuInputs (the "Unload After" submenu's
    // checkmark and Custom… hint) - re-sync so the tray reflects the new
    // value now instead of after the next unrelated rebuild (the KB-031/
    // 034/188 rule every other menu-visible setter follows).
    crate::tray::update_tray_menu(&app);
}

/// Set the total-request timeout for cloud post-process calls, in seconds.
/// Rejects out-of-range values instead of clamping so a UI bug can't
/// silently write a 1-second or 10-minute timeout the user never saw.
#[tauri::command]
#[specta::specta]
pub fn set_post_process_timeout(app: AppHandle, seconds: u64) -> Result<(), String> {
    if !(POST_PROCESS_TIMEOUT_MIN_SECONDS..=POST_PROCESS_TIMEOUT_MAX_SECONDS).contains(&seconds) {
        return Err(format!(
            "Post-process timeout must be between {} and {} seconds (got {})",
            POST_PROCESS_TIMEOUT_MIN_SECONDS, POST_PROCESS_TIMEOUT_MAX_SECONDS, seconds
        ));
    }
    let mut settings = get_settings(&app);
    settings.post_process_timeout_secs = seconds;
    write_settings(&app, settings);
    Ok(())
}

/// Set the per-provider override of the post-process timeout, in seconds.
/// Same inclusive bounds as the global setting (rejecting out-of-range
/// values instead of clamping); the provider's requests then run under
/// this value instead of its class default. The reset command below
/// removes the override; a stored 0 also resolves to the class default.
#[tauri::command]
#[specta::specta]
pub fn set_post_process_timeout_for_provider(
    app: AppHandle,
    provider_id: String,
    seconds: u64,
) -> Result<(), String> {
    if !(POST_PROCESS_TIMEOUT_MIN_SECONDS..=POST_PROCESS_TIMEOUT_MAX_SECONDS).contains(&seconds) {
        return Err(format!(
            "Post-process timeout must be between {} and {} seconds (got {})",
            POST_PROCESS_TIMEOUT_MIN_SECONDS, POST_PROCESS_TIMEOUT_MAX_SECONDS, seconds
        ));
    }
    let mut settings = get_settings(&app);
    if !settings
        .post_process_providers
        .iter()
        .any(|provider| provider.id == provider_id)
    {
        return Err(format!("unknown post-process provider: {provider_id}"));
    }
    settings.post_process_timeouts.insert(provider_id, seconds);
    write_settings(&app, settings);
    Ok(())
}

/// Remove the per-provider post-process timeout override: the provider's
/// requests resolve through its class default again (Groq/Cerebras 30s,
/// the others 60s), which is what the settings row shows as the reset
/// target.
#[tauri::command]
#[specta::specta]
pub fn reset_post_process_timeout_for_provider(app: AppHandle, provider_id: String) {
    let mut settings = get_settings(&app);
    settings.post_process_timeouts.remove(&provider_id);
    write_settings(&app, settings);
}

/// Set the idle-unload timeout to a custom seconds value (the Settings
/// numeric field and any future UI that speaks seconds). Rejects
/// out-of-range values instead of clamping so a UI bug can't silently write
/// a 3-second or 30-day timeout the user never saw.
#[tauri::command]
#[specta::specta]
pub fn set_model_unload_timeout_custom_seconds(app: AppHandle, seconds: u64) -> Result<(), String> {
    if !(MODEL_UNLOAD_CUSTOM_MIN_SECONDS..=MODEL_UNLOAD_CUSTOM_MAX_SECONDS).contains(&seconds) {
        return Err(format!(
            "Custom unload timeout must be between {MODEL_UNLOAD_CUSTOM_MIN_SECONDS} and \
             {MODEL_UNLOAD_CUSTOM_MAX_SECONDS} seconds (got {seconds})"
        ));
    }
    let mut settings = get_settings(&app);
    settings.model_unload_timeout = ModelUnloadTimeout::Custom { seconds };
    write_settings(&app, settings);

    // KB-200: same rule as set_model_unload_timeout - the Custom value
    // drives the "Unload After" checkmark/hint through MenuInputs.
    crate::tray::update_tray_menu(&app);

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn get_model_load_status(
    transcription_manager: State<Arc<TranscriptionManager>>,
) -> Result<ModelLoadStatus, String> {
    Ok(ModelLoadStatus {
        is_loaded: transcription_manager.is_model_loaded(),
        current_model: transcription_manager.get_current_model(),
    })
}

/// KB-220: the settings-side manual unload carries the same refusal as the
/// tray's "Unload model" - unloading while a dictation session is live
/// (recording or the stop pipeline still working) kills the stream and
/// leaves the batch fallback to fail. The Err carries the stable prefix so
/// a settings surface can classify it as "try again after the dictation"
/// instead of a real failure.
#[tauri::command]
#[specta::specta]
pub fn unload_model_manually(
    app: AppHandle,
    transcription_manager: State<Arc<TranscriptionManager>>,
) -> Result<(), String> {
    if crate::commands::models::dictation_session_live(&app) {
        return Err(crate::commands::models::session_refusal_error());
    }
    transcription_manager.request_unload();
    Ok(())
}
