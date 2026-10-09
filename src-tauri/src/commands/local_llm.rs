//! Thin commands for the local post-process model (spec 9.1). The model is
//! managed by the same ModelManager lifecycle as voice models (download
//! with progress events, cancel, delete); the only additions are the
//! download-time sha256 verification (performed ONCE, here) and the
//! delete-time swap lease check.

use crate::local_llm::manager::LlmManager;
use crate::local_llm::{LOCAL_LLM_MODEL_ID, LOCAL_LLM_MODEL_SHA256, LOCAL_LLM_MODEL_SIZE_MB};
use crate::managers::model::ModelManager;
use log::{error, info, warn};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};

/// Status snapshot for the settings row (spec 6.2).
#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct LocalLlmModelStatus {
    pub downloaded: bool,
    pub downloading: bool,
    pub size_mb: u64,
    /// 0 to 100 while downloading (partial bytes over the total); 0 or 100
    /// otherwise.
    pub progress: f64,
}

#[tauri::command]
#[specta::specta]
pub async fn get_local_llm_model_status(
    model_manager: State<'_, Arc<ModelManager>>,
) -> Result<LocalLlmModelStatus, String> {
    let info = model_manager
        .get_model_info(LOCAL_LLM_MODEL_ID)
        .ok_or_else(|| "the local post-process model entry is missing".to_string())?;
    let total_bytes = info.size_mb.saturating_mul(1024 * 1024);
    let progress = if info.is_downloading && total_bytes > 0 {
        (info.partial_size as f64 / total_bytes as f64 * 100.0).clamp(0.0, 100.0)
    } else if info.is_downloaded {
        100.0
    } else {
        0.0
    };
    Ok(LocalLlmModelStatus {
        downloaded: info.is_downloaded,
        downloading: info.is_downloading,
        size_mb: info.size_mb,
        progress,
    })
}

/// Download the pinned post-process model through the standard
/// ModelManager pipeline (progress events included), then verify the
/// sha256 ONCE against the pinned constant (spec 6.1, reviewer finding
/// R12): a mismatch deletes the file and errors so the entry reads
/// not-downloaded again (delete + re-download repairs any corruption).
#[tauri::command]
#[specta::specta]
pub async fn download_local_llm_model(
    app_handle: AppHandle,
    model_manager: State<'_, Arc<ModelManager>>,
) -> Result<(), String> {
    model_manager
        .download_model(LOCAL_LLM_MODEL_ID)
        .await
        .map_err(|e| e.to_string())?;

    let path = match model_manager.get_model_path(LOCAL_LLM_MODEL_ID) {
        Ok(path) => path,
        Err(e) => {
            // Download reported success but the file does not resolve:
            // treat as a corrupted cache and clean up like a mismatch.
            error!("local model path unresolved after download: {}", e);
            let _ = model_manager.delete_model(LOCAL_LLM_MODEL_ID);
            return Err(format!("downloaded file is unusable: {}", e));
        }
    };

    let mut file = std::fs::File::open(&path).map_err(|e| {
        format!(
            "failed to open the downloaded model for verification: {}",
            e
        )
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| format!("failed while reading the downloaded model: {}", e))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = format!("{:x}", hasher.finalize());
    if digest != LOCAL_LLM_MODEL_SHA256 {
        warn!(
            "local post-process model sha256 mismatch (expected {}, got {}); deleting the file",
            LOCAL_LLM_MODEL_SHA256, digest
        );
        let _ = model_manager.delete_model(LOCAL_LLM_MODEL_ID);
        let _ = app_handle.emit(
            "model-download-failed",
            serde_json::json!({
                "model_id": LOCAL_LLM_MODEL_ID,
                "error": "checksum mismatch after download"
            }),
        );
        return Err(
            "the downloaded model failed its checksum; it was removed. Try downloading again"
                .to_string(),
        );
    }

    info!(
        "local post-process model downloaded and verified (sha256 {}, {} MB)",
        &digest[..12],
        LOCAL_LLM_MODEL_SIZE_MB
    );
    Ok(())
}

/// Delete the local post-process model. Refuses with a transient error
/// while a post-process swap is running (L3): the swap may be about to
/// restore a voice model file this delete would remove.
///
/// DEVIATION 2 (flagged in the plan): unlike the voice-model delete path,
/// there is no unload-with-wait here. The LLM worker exists only inside a
/// swap, and the lease check above has already excluded swaps; outside a
/// swap there is never a resident LLM engine to unload. The comment
/// documents this instead of adding an unload call that can never find a
/// worker.
#[tauri::command]
#[specta::specta]
pub async fn delete_local_llm_model(
    llm_manager: State<'_, Arc<LlmManager>>,
    model_manager: State<'_, Arc<ModelManager>>,
) -> Result<(), String> {
    if llm_manager.swap_in_progress() {
        return Err("post-processing is in progress, try again in a moment".to_string());
    }
    model_manager
        .delete_model(LOCAL_LLM_MODEL_ID)
        .map_err(|e| e.to_string())
}
