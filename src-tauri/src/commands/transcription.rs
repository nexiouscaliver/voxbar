use crate::managers::transcription::TranscriptionManager;
use crate::settings::{
    get_settings, write_settings, ModelUnloadTimeout, MODEL_UNLOAD_CUSTOM_MAX_SECONDS,
    MODEL_UNLOAD_CUSTOM_MIN_SECONDS,
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

#[tauri::command]
#[specta::specta]
pub fn unload_model_manually(
    transcription_manager: State<Arc<TranscriptionManager>>,
) -> Result<(), String> {
    transcription_manager.request_unload();
    Ok(())
}
