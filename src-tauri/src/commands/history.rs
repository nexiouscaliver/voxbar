use crate::actions::process_transcription_output;
use crate::managers::{
    history::{HistoryManager, PaginatedHistory},
    transcription::TranscriptionManager,
};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};

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

/// KB-202: whether a history setter may persist its change, given the
/// outcome of the pre-persist cleanup that already ran against the
/// would-be value. The old inverse shape (persist, then cleanup) returned
/// the cleanup's Err to the UI - which rolls its control back (KB-203's
/// per-key rollback) - while the store kept the new value, so disk and
/// the reported outcome disagreed. Pure so the contract is pinned without
/// an app handle; the DB cleanup itself is AppHandle-bound
/// (HistoryManager pins tauri::AppHandle).
fn should_persist_after_cleanup(cleanup: &anyhow::Result<()>) -> bool {
    cleanup.is_ok()
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

    // KB-202: run the cleanup against the WOULD-BE limit first (retention
    // is this command's untouched field, so the store's current value
    // rides along) and persist only when it succeeded, so the store never
    // holds a value whose cleanup the caller was told failed.
    let cleanup =
        history_manager.cleanup_old_entries_with(settings.recording_retention_period, limit);
    if !should_persist_after_cleanup(&cleanup) {
        return Err(cleanup.unwrap_err().to_string());
    }

    crate::settings::write_settings(&app, settings);

    // Same convergence signal the other settings setters emit (the
    // start_hidden/debug_mode rule): every listening webview re-reads the
    // store, which now agrees with the Ok the caller just got.
    let _ = app.emit(
        "settings-changed",
        serde_json::json!({
            "setting": "history_limit",
            "value": limit
        }),
    );

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

    // KB-202: the same cleanup-first shape as update_history_limit - the
    // would-be retention picks the pass (Never: nothing; a time period:
    // by-time against it), and the store's current limit rides along.
    let cleanup =
        history_manager.cleanup_old_entries_with(retention_period, settings.history_limit);
    if !should_persist_after_cleanup(&cleanup) {
        return Err(cleanup.unwrap_err().to_string());
    }

    crate::settings::write_settings(&app, settings);

    let _ = app.emit(
        "settings-changed",
        serde_json::json!({
            "setting": "recording_retention_period",
            "value": period
        }),
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{retry_runs_post_process, should_persist_after_cleanup};
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

    /// KB-202: the history setters run their cleanup BEFORE persisting and
    /// gate the write on its outcome, so a failed cleanup blocks the
    /// persist (the UI's rollback matches the untouched store) while a
    /// successful one lets it through. The old inverse shape wrote first,
    /// so a cleanup failure rolled the UI back against a store that
    /// already held the new value.
    #[test]
    fn cleanup_outcome_gates_whether_the_setter_persists() {
        assert!(
            should_persist_after_cleanup(&Ok(())),
            "cleanup succeeded: the new value may reach the store"
        );
        assert!(
            !should_persist_after_cleanup(&Err(anyhow::anyhow!(
                "database is locked"
            ))),
            "cleanup failed: the store must keep the previous value so disk matches the Err the UI rolls back to"
        );
    }
}
