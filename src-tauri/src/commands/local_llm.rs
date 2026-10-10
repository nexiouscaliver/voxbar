//! Thin commands for the local post-process models (spec 9.1). The models
//! are managed by the same ModelManager lifecycle as voice models (download
//! with progress events, cancel, delete); the only additions are the
//! download-time sha256 verification (performed ONCE, here), the
//! delete-time swap lease check, and the selection command the settings
//! section and the tray submenu share.
//!
//! Every wrapper takes the registry id. `None` means the pinned builtin
//! (`LOCAL_LLM_MODEL_ID`), which keeps the pre-catalog call sites (the
//! single pinned row) working unchanged - the off path.

use crate::local_llm::manager::{selected_llm_model_id, LlmManager};
use crate::local_llm::{
    LOCAL_LLM_MODEL_ID, LOCAL_LLM_MODEL_NAME, LOCAL_LLM_MODEL_SHA256, LOCAL_LLM_MODEL_SIZE_MB,
    SWAP_REFUSAL_PREFIX,
};
use crate::managers::model::{
    resolve_llm_hf_repo, EngineType, HfModelError, HfModelResolution, ModelInfo, ModelManager,
};
use crate::settings::{get_settings, write_settings};
use log::{error, info, warn};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};

/// One post-process model as the settings section and tray see it: the
/// shared [`ModelInfo`] plus the LLM-specific card fields the ASR shape does
/// not carry.
#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct LlmModelEntry {
    pub info: ModelInfo,
    /// Quantization of the surfaced file ("Q4_K_M"), when known.
    pub quant: String,
    /// Context window the swap runner allocates for this model.
    pub context_tokens: u32,
    /// Display publisher ("Qwen", "bartowski"), when known.
    pub publisher: String,
    /// Whether this is the model the post-process engine runs.
    pub selected: bool,
}

/// Quantization label for a GGUF filename: the last dash-separated segment
/// shaped like a quant token (Q8_0, Q4_K_M, ...), when there is one.
fn quant_from_filename(filename: &str) -> Option<String> {
    let stem = filename.strip_suffix(".gguf")?;
    stem.rsplit('-')
        .find(|seg| {
            let mut chars = seg.chars();
            chars.next() == Some('Q')
                && chars.next().is_some_and(|c| c.is_ascii_digit())
                && seg.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
        .map(str::to_string)
}

/// Emit the shared `model-download-failed` event for a post-process model,
/// so the app-wide toast listener (the model store) fires for local-model
/// downloads exactly as it does for voice-model downloads. The payload
/// carries the display name because these models are filtered out of the
/// store's model list: without it the toast falls back to the raw registry
/// id.
fn emit_download_failed<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    model_id: &str,
    name: &str,
    error: &str,
) {
    let _ = app.emit(
        "model-download-failed",
        serde_json::json!({
            "model_id": model_id,
            "error": error,
            "name": name,
        }),
    );
}

/// The expected download sha256 for a post-process model: the pinned
/// constant for the pinned builtin, the catalog's trust anchor for catalog
/// entries, `None` for user-added repos (their first download defines the
/// bytes; the digest is still computed and logged for parity).
fn expected_sha256(model_id: &str) -> Option<&'static str> {
    if model_id == LOCAL_LLM_MODEL_ID {
        Some(LOCAL_LLM_MODEL_SHA256)
    } else {
        crate::catalog::llm::expected_sha256_for(model_id)
    }
}

/// Status snapshot for one post-process model row (spec 6.2). `model_id:
/// None` reads the pinned builtin, the shape the original settings row was
/// built on.
#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct LocalLlmModelStatus {
    pub downloaded: bool,
    pub downloading: bool,
    pub size_mb: u64,
    /// 0 to 100 while downloading (partial bytes over the total); 0 or 100
    /// otherwise.
    pub progress: f64,
}

fn status_for(info: &ModelInfo) -> LocalLlmModelStatus {
    let total_bytes = info.size_mb.saturating_mul(1024 * 1024);
    let progress = if info.is_downloading && total_bytes > 0 {
        (info.partial_size as f64 / total_bytes as f64 * 100.0).clamp(0.0, 100.0)
    } else if info.is_downloaded {
        100.0
    } else {
        0.0
    };
    LocalLlmModelStatus {
        downloaded: info.is_downloaded,
        downloading: info.is_downloading,
        size_mb: info.size_mb,
        progress,
    }
}

#[tauri::command]
#[specta::specta]
pub async fn get_local_llm_model_status(
    model_manager: State<'_, Arc<ModelManager>>,
    model_id: Option<String>,
) -> Result<LocalLlmModelStatus, String> {
    let model_id = model_id.unwrap_or_else(|| LOCAL_LLM_MODEL_ID.to_string());
    let info = model_manager
        .get_model_info(&model_id)
        .ok_or_else(|| format!("the local post-process model entry is missing: {model_id}"))?;
    Ok(status_for(&info))
}

/// Every post-process model (catalog + pinned + user-added) with its card
/// metadata and the selection flag. The settings section and the tray
/// submenu both read this; nothing else should (ASR surfaces keep using
/// `get_available_models`, which filters LocalLlm entries out).
#[tauri::command]
#[specta::specta]
pub async fn get_available_llm_models(
    app_handle: AppHandle,
    model_manager: State<'_, Arc<ModelManager>>,
) -> Result<Vec<LlmModelEntry>, String> {
    let selected = selected_llm_model_id(&app_handle);
    let entries = model_manager
        .get_available_llm_models()
        .into_iter()
        .map(|info| {
            let quant = match &info.source {
                crate::managers::model::ModelSource::HuggingFace { repo_id, .. } => {
                    crate::catalog::llm::file_in_llm_catalog(&info.filename, Some(repo_id))
                        .map(|(_, f)| f.quant.clone())
                }
                _ => None,
            }
            .or_else(|| quant_from_filename(&info.filename))
            .unwrap_or_default();
            let publisher = crate::catalog::llm::publisher_for(&info.id)
                .map(str::to_string)
                .unwrap_or_else(|| {
                    info.id
                        .split('/')
                        .next()
                        .unwrap_or("Hugging Face")
                        .to_string()
                });
            LlmModelEntry {
                context_tokens: crate::catalog::llm::context_tokens_for(&info.id),
                selected: info.id == selected,
                quant,
                publisher,
                info,
            }
        })
        .collect();
    Ok(entries)
}

/// Download a post-process model through the standard ModelManager pipeline
/// (progress events included, into the dedicated llm-models cache), then
/// verify the sha256 ONCE against the trust anchor (spec 6.1, reviewer
/// finding R12): a mismatch deletes the file and errors so the entry reads
/// not-downloaded again (delete + re-download repairs any corruption).
/// `model_id: None` downloads the pinned builtin, exactly as before.
#[tauri::command]
#[specta::specta]
pub async fn download_local_llm_model(
    app_handle: AppHandle,
    model_manager: State<'_, Arc<ModelManager>>,
    model_id: Option<String>,
) -> Result<(), String> {
    let model_id = model_id.unwrap_or_else(|| LOCAL_LLM_MODEL_ID.to_string());
    let display_name = model_manager
        .get_model_info(&model_id)
        .map(|i| i.name)
        .unwrap_or_else(|| LOCAL_LLM_MODEL_NAME.to_string());

    let result = model_manager
        .download_model(&model_id)
        .await
        .map_err(|e| e.to_string());

    if let Err(ref error) = result {
        // Log as well as emit, mirroring the voice-model wrapper
        // (commands/models.rs): without this event the app-wide toast
        // listener stays silent, and a failed multi-GB download looks like
        // nothing happened (the row just reverts to "Not downloaded").
        error!("local post-process model download failed: {}", error);
        emit_download_failed(&app_handle, &model_id, &display_name, error);
    }

    result?;

    let path = match model_manager.get_model_path(&model_id) {
        Ok(path) => path,
        Err(e) => {
            // Download reported success but the file does not resolve:
            // treat as a corrupted cache and clean up like a mismatch.
            error!("local model path unresolved after download: {}", e);
            let _ = model_manager.delete_model(&model_id);
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

    match expected_sha256(&model_id) {
        Some(expected) if digest != expected => {
            warn!(
                "local post-process model sha256 mismatch for {} (expected {}, got {}); deleting the file",
                model_id, expected, digest
            );
            let _ = model_manager.delete_model(&model_id);
            emit_download_failed(
                &app_handle,
                &model_id,
                &display_name,
                "checksum mismatch after download",
            );
            return Err(
                "the downloaded model failed its checksum; it was removed. Try downloading again"
                    .to_string(),
            );
        }
        expected => {
            // Matched the anchor, or a user-added repo (None): log the
            // digest either way so a later audit has the pinned value.
            info!(
                "local post-process model {} downloaded and verified (sha256 {}, {} MB)",
                model_id,
                &digest[..12],
                path.metadata()
                    .map(|m| m.len() / (1024 * 1024))
                    .unwrap_or(LOCAL_LLM_MODEL_SIZE_MB)
            );
            let _ = expected;
        }
    }

    Ok(())
}

/// Delete a post-process model. Refuses with a transient error while a
/// post-process swap is running (L3): the swap may be about to restore a
/// voice model file this delete would remove. Deleting the SELECTED model
/// resets the selection to the pinned builtin so the engine never points at
/// a missing file.
///
/// DEVIATION 2 (flagged in the plan): unlike the voice-model delete path,
/// there is no unload-with-wait here. The LLM worker exists only inside a
/// swap, and the lease check above has already excluded swaps; outside a
/// swap there is never a resident LLM engine to unload.
#[tauri::command]
#[specta::specta]
pub async fn delete_local_llm_model(
    app_handle: AppHandle,
    llm_manager: State<'_, Arc<LlmManager>>,
    model_manager: State<'_, Arc<ModelManager>>,
    model_id: Option<String>,
) -> Result<(), String> {
    let model_id = model_id.unwrap_or_else(|| LOCAL_LLM_MODEL_ID.to_string());
    if llm_manager.swap_in_progress() {
        // The stable prefix lets the settings UI tell this transient
        // refusal apart from real delete failures (see SWAP_REFUSAL_PREFIX).
        return Err(swap_refusal_error());
    }
    if selected_llm_model_id(&app_handle) == model_id {
        let mut settings = get_settings(&app_handle);
        settings.post_process_local_model_id = LOCAL_LLM_MODEL_ID.to_string();
        write_settings(&app_handle, settings);
    }
    model_manager
        .delete_model(&model_id)
        .map_err(|e| e.to_string())
}

/// The transient refusal error the delete command returns while a swap is
/// running (L3): the stable prefix lets the settings UI classify it as
/// "try again in a moment" instead of a real failure. Pure so the wording
/// contract is pinnable.
pub(crate) fn swap_refusal_error() -> String {
    format!(
        "{}: post-processing is in progress, try again in a moment",
        SWAP_REFUSAL_PREFIX
    )
}

/// Pure selection guard shared by the command and the tray handler: the
/// entry must exist, be a LocalLlm model, and be on disk (an undownloaded
/// selection would silently skip every post-process with
/// download_missing). `None` = unknown id.
fn validate_llm_selection(info: Option<&ModelInfo>) -> Result<(), String> {
    let Some(info) = info else {
        return Err("Model not found".to_string());
    };
    if !matches!(info.engine_type, EngineType::LocalLlm) {
        return Err(format!(
            "not-a-post-process-model: {} powers transcription, not post-processing",
            info.id
        ));
    }
    if !info.is_downloaded {
        return Err(format!("Model not downloaded: {}", info.id));
    }
    Ok(())
}

/// Shared persistence for the selection command and the tray handler:
/// validate, persist, notify. The settings section and the tray submenu
/// both land here, so the two surfaces can never disagree.
pub fn set_post_process_local_model_impl(app: &AppHandle, model_id: &str) -> Result<(), String> {
    let model_manager = app.state::<Arc<ModelManager>>();
    let info = model_manager.get_model_info(model_id);
    validate_llm_selection(info.as_ref())?;
    let mut settings = get_settings(app);
    settings.post_process_local_model_id = model_id.to_string();
    write_settings(app, settings);
    info!("Post-process model set to {}", model_id);
    let _ = app.emit("post-process-model-changed", model_id);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn set_post_process_local_model(
    app_handle: AppHandle,
    model_id: String,
) -> Result<(), String> {
    set_post_process_local_model_impl(&app_handle, &model_id)
}

/// Resolve pasted Hugging Face input for the POST-PROCESS add flow (same
/// listing shape as the voice flow; the suggested file prefers the smaller
/// LLM quants).
#[tauri::command]
#[specta::specta]
pub async fn resolve_llm_hf_model(input: String) -> Result<HfModelResolution, HfModelError> {
    resolve_llm_hf_repo(&input).await
}

/// Download a specific GGUF from a Hugging Face repo and register it as a
/// post-process model, gated on the LLM architecture allowlist: an
/// unsupported architecture is refused, the blob deleted, and the error
/// names the architecture and supported families.
#[tauri::command]
#[specta::specta]
pub async fn add_llm_hf_model(
    model_manager: State<'_, Arc<ModelManager>>,
    repo_id: String,
    filename: String,
    revision: Option<String>,
) -> Result<String, HfModelError> {
    model_manager
        .add_llm_hf_model(&repo_id, &filename, revision.as_deref())
        .await
}

#[cfg(test)]
mod tests {
    use super::emit_download_failed;
    use super::{
        quant_from_filename, validate_llm_selection, LOCAL_LLM_MODEL_ID, LOCAL_LLM_MODEL_NAME,
    };
    use std::sync::mpsc;
    use tauri::Listener;

    /// Regression test for the invisible-download-failure bug: a failed
    /// download of the pinned model must emit the shared
    /// `model-download-failed` event (the one the app-wide toast listener
    /// keys on), carrying the pinned id, the raw error, and the display
    /// name (this model never appears in the store's model list, so the
    /// toast cannot resolve a friendly name without it).
    #[test]
    fn download_failed_event_carries_id_name_and_error() {
        let app = tauri::test::mock_app();
        let (tx, rx) = mpsc::channel();
        let app_handle = app.handle().clone();
        app.listen("model-download-failed", move |event| {
            if let Ok(payload) = serde_json::from_str::<serde_json::Value>(event.payload()) {
                let _ = tx.send(payload);
            }
        });

        emit_download_failed(
            &app_handle,
            LOCAL_LLM_MODEL_ID,
            LOCAL_LLM_MODEL_NAME,
            "network dropped mid-download",
        );

        let payload = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the model-download-failed event must be emitted");
        assert_eq!(payload["model_id"], LOCAL_LLM_MODEL_ID);
        assert_eq!(payload["name"], LOCAL_LLM_MODEL_NAME);
        assert_eq!(payload["error"], "network dropped mid-download");
    }

    /// The card's quant label parses out of real GGUF filenames; non-quant
    /// files yield None instead of a misleading label.
    #[test]
    fn quant_label_parses_from_filenames() {
        assert_eq!(
            quant_from_filename("Qwen3-1.7B-Q4_K_M.gguf").as_deref(),
            Some("Q4_K_M")
        );
        assert_eq!(
            quant_from_filename("Llama-3.2-1B-Instruct-Q8_0.gguf").as_deref(),
            Some("Q8_0")
        );
        assert_eq!(
            quant_from_filename("gemma-3-4b-it-Q4_K_M.gguf").as_deref(),
            Some("Q4_K_M")
        );
        assert_eq!(quant_from_filename("some-model.gguf"), None);
        assert_eq!(quant_from_filename("not-a-gguf.bin"), None);
    }

    /// The selection guard accepts a downloaded LocalLlm entry and refuses
    /// every invalid one BEFORE the store is touched: unknown ids, ASR
    /// engine types (the settings UI and the tray share this one command,
    /// so neither can select a transcription model), and undownloaded
    /// entries (which would silently skip every post-process run).
    #[test]
    fn selection_guard_accepts_only_downloaded_llm_entries() {
        use crate::managers::model::local_llm_model_info;

        let mut llm = local_llm_model_info();
        llm.is_downloaded = true;
        assert!(validate_llm_selection(Some(&llm)).is_ok());

        let undownloaded = local_llm_model_info();
        let err = validate_llm_selection(Some(&undownloaded)).unwrap_err();
        assert!(err.contains("not downloaded"), "{err}");

        let mut asr = local_llm_model_info();
        asr.is_downloaded = true;
        asr.engine_type = crate::managers::model::EngineType::TranscribeCpp;
        let err = validate_llm_selection(Some(&asr)).unwrap_err();
        assert!(err.starts_with("not-a-post-process-model"), "{err}");

        assert!(validate_llm_selection(None).is_err());
    }

    /// L2/L3 contract: the delete refusal while a swap runs carries the
    /// stable prefix the settings UI classifies on, and it stays in sync
    /// with the backend constant the frontend pins.
    #[test]
    fn swap_refusal_error_carries_the_stable_prefix() {
        let error = super::swap_refusal_error();
        assert!(
            error.starts_with(crate::local_llm::SWAP_REFUSAL_PREFIX),
            "the refusal must carry the stable prefix, got: {error}"
        );
        assert!(error.contains("try again in a moment"));
    }
}
