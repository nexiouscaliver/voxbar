use crate::actions::process_transcription_output;
use crate::managers::{
    history::{HistoryManager, PaginatedHistory},
    transcription::TranscriptionManager,
};
use std::sync::Arc;
use tauri::{AppHandle, State};

#[tauri::command]
#[specta::specta]
pub async fn get_history_entries(
    _app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    cursor: Option<i64>,
    limit: Option<usize>,
) -> Result<PaginatedHistory, String> {
    history_manager
        .get_history_entries(cursor, limit)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn toggle_history_entry_saved(
    _app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    id: i64,
) -> Result<(), String> {
    history_manager
        .toggle_saved_status(id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn get_audio_file_path(
    _app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    file_name: String,
) -> Result<String, String> {
    let path = history_manager.get_audio_file_path(&file_name);
    path.to_str()
        .ok_or_else(|| "Invalid file path".to_string())
        .map(|s| s.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn delete_history_entry(
    _app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    id: i64,
) -> Result<(), String> {
    history_manager
        .delete_entry(id)
        .await
        .map_err(|e| e.to_string())
}

/// KB-109 (privacy): whether a history retry runs the LLM post-process
/// leg. The per-entry snapshot alone must not decide - with the
/// post-process master toggle off, a retry that re-ran the leg would ship
/// the stored transcript to the CURRENT provider/model (possibly paid
/// cloud), which the entry was not necessarily recorded against. The
/// master toggle gates the leg exactly as it gates the dictation hotkey's
/// registration (binding_is_active), so off means the retry re-transcribes
/// and updates the raw transcript only. Pure so the gate is unit-testable;
/// pinning the retry to the entry's recorded provider is a separate,
/// larger fix.
fn retry_runs_post_process(
    entry_requested: bool,
    settings: &crate::settings::AppSettings,
) -> bool {
    entry_requested && settings.post_process_enabled
}

#[tauri::command]
#[specta::specta]
pub async fn retry_history_entry_transcription(
    app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    transcription_manager: State<'_, Arc<TranscriptionManager>>,
    id: i64,
) -> Result<(), String> {
    let entry = history_manager
        .get_entry_by_id(id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("History entry {} not found", id))?;

    let audio_path = history_manager.get_audio_file_path(&entry.file_name);
    let samples = crate::audio_toolkit::read_wav_samples(&audio_path)
        .map_err(|e| format!("Failed to load audio: {}", e))?;

    if samples.is_empty() {
        return Err("Recording has no audio samples".to_string());
    }

    transcription_manager.initiate_model_load();

    let tm = Arc::clone(&transcription_manager);
    let (transcription, used_model) =
        tauri::async_runtime::spawn_blocking(move || tm.transcribe_with_model(samples))
            .await
            .map_err(|e| format!("Transcription task panicked: {}", e))?
            .map_err(|e| e.to_string())?;

    if transcription.is_empty() {
        return Err("Recording contains no speech".to_string());
    }

    // The history retry owns no cancel generation (no recording session is
    // live); the swap runner still has its own abort signals (pending
    // press, recording started) and its total deadline. KB-109: the pp leg
    // is gated on the master toggle as well as the entry's snapshot.
    let post_process = retry_runs_post_process(
        entry.post_process_requested,
        &crate::settings::get_settings(&app),
    );
    let processed = process_transcription_output(
        &app,
        "history_retry",
        &transcription,
        post_process,
        None,
    )
    .await;
    history_manager
        .update_transcription(
            id,
            transcription,
            processed.post_processed_text,
            processed.post_process_prompt,
            Some(used_model),
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn update_history_limit(
    app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    limit: usize,
) -> Result<(), String> {
    let mut settings = crate::settings::get_settings(&app);
    settings.history_limit = limit;
    crate::settings::write_settings(&app, settings);

    history_manager
        .cleanup_old_entries()
        .map_err(|e| e.to_string())?;

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn update_recording_retention_period(
    app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    period: String,
) -> Result<(), String> {
    use crate::settings::RecordingRetentionPeriod;

    let retention_period = match period.as_str() {
        "never" => RecordingRetentionPeriod::Never,
        "preserve_limit" => RecordingRetentionPeriod::PreserveLimit,
        "days3" => RecordingRetentionPeriod::Days3,
        "weeks2" => RecordingRetentionPeriod::Weeks2,
        "months3" => RecordingRetentionPeriod::Months3,
        _ => return Err(format!("Invalid retention period: {}", period)),
    };

    let mut settings = crate::settings::get_settings(&app);
    settings.recording_retention_period = retention_period;
    crate::settings::write_settings(&app, settings);

    history_manager
        .cleanup_old_entries()
        .map_err(|e| e.to_string())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::retry_runs_post_process;
    use crate::settings::get_default_settings;

    /// KB-109: the retry's post-process leg needs BOTH the per-entry
    /// snapshot and the master toggle. With the layer off, a snapshot
    /// recorded with it on must not re-run the leg - that would ship the
    /// stored transcript to the CURRENT (possibly paid cloud) provider the
    /// entry was never recorded against; the retry re-transcribes and
    /// updates the raw transcript instead.
    #[test]
    fn retry_post_process_requires_snapshot_and_master_toggle() {
        let mut settings = get_default_settings();

        settings.post_process_enabled = false;
        assert!(
            !retry_runs_post_process(true, &settings),
            "the master toggle off means no post-processing on retry, regardless of the snapshot"
        );

        settings.post_process_enabled = true;
        assert!(
            retry_runs_post_process(true, &settings),
            "snapshot on plus master toggle on runs the leg"
        );
        assert!(
            !retry_runs_post_process(false, &settings),
            "an entry recorded without post-processing never gains the leg on retry"
        );
    }
}
