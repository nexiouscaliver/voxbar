//! "Add model from Hugging Face" (Settings, Models): the paste-a-repo flow.
//!
//! Pipeline: parse the pasted input ([`parse_hf_input`]), list the repo's
//! GGUF files with sizes through the Hub metadata API ([`resolve_hf_repo`]),
//! suggest a quant ([`preferred_gguf`]), then [`ModelManager::add_hf_model`]
//! registers a provisional registry entry, downloads it through the ordinary
//! [`ModelManager::download_model`] path (shared progress events,
//! cancellation token, stall watchdog) and gates registration on the GGUF
//! architecture probe: an architecture the engines cannot load is refused,
//! the downloaded blob is deleted, and the error names the model's
//! architecture and the supported families.
//!
//! Persistence follows the HF cache discovery contract: the registry entry id
//! is `"{repo_id}/{filename}"` with metadata field-identical to what
//! `discover_hf_cache_models_in` derives, so restarts and rescans re-derive
//! the same entry instead of duplicating it.
//!
//! v1 is public repos only: no HF token is sent anywhere (the download path
//! deliberately clears tokens), so repos that cannot be read anonymously
//! surface a clear [`HfModelError::Inaccessible`] instead of a confusing
//! download failure.

use super::{hf_cached_path, local_caps, probed_display_name};
use crate::managers::model::{EngineType, ModelInfo, ModelManager, ModelSource};
use crate::managers::model_capabilities::{
    CapabilityProbe, CapabilityProber, Compatibility, GgufHeaderProber, KNOWN_ARCHES, LLM_ARCHES,
};
use hf_hub::api::tokio::ApiBuilder;
use hf_hub::{Repo, RepoType};
use log::{info, warn};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::time::Duration;
use tauri::Emitter;

/// Bound on one Hub metadata API round trip (the repo listing). The metadata
/// request is small, so a wedged connection means the network is gone; the
/// user gets a network error instead of a hung "Resolving..." status.
const HF_METADATA_TIMEOUT: Duration = Duration::from_secs(20);

/// Quantization preference when the user did not name a file and the repo has
/// several GGUFs: the first token present (matched inside the filename,
/// case-insensitively), else the largest file.
const QUANT_PREFERENCE: &[&str] = &["Q8_0", "Q6_K", "Q5_K_M", "Q4_K_M"];

/// What the user pasted, reduced to the parts the flow needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedHfInput {
    /// `owner/name` on the Hub.
    pub repo_id: String,
    /// A file within the repo (a subfolder path is kept as-is), when the
    /// input named one.
    pub filename: Option<String>,
    /// A revision named by the input (branch, tag, or commit), when present.
    pub revision: Option<String>,
}

/// Parse pasted Hugging Face input. Accepted forms:
///
/// - a full URL: `https://huggingface.co/owner/repo/blob/REV/file.gguf`,
///   the `/resolve/REV/...` and `/raw/REV/...` download variants, a
///   `/tree/REV/path` listing (a trailing `.gguf` segment counts as the
///   file, anything else falls back to the whole repo), or a bare repo URL
/// - `owner/repo/file.gguf` (subfolder paths preserved)
/// - a bare `owner/repo`
///
/// Scheme-less `huggingface.co/...` / `hf.co/...` prefixes are accepted too.
/// Explicit files must end in `.gguf`; only GGUF models can load.
pub fn parse_hf_input(raw: &str) -> Result<ParsedHfInput, String> {
    let input = raw.trim();
    if input.is_empty() {
        return Err("the input is empty".to_string());
    }
    let without_fragment = input.split(['?', '#']).next().unwrap_or(input).to_string();

    let path = if let Some(rest) = without_fragment
        .strip_prefix("https://")
        .or_else(|| without_fragment.strip_prefix("http://"))
    {
        let (host, path) = rest.split_once('/').ok_or("URL has no repo path")?;
        let host = host.to_ascii_lowercase();
        if !matches!(
            host.as_str(),
            "huggingface.co" | "www.huggingface.co" | "hf.co" | "www.hf.co"
        ) {
            return Err(format!(
                "not a huggingface.co URL (host: {}); paste a huggingface.co link, owner/repo, or owner/repo/file.gguf",
                host
            ));
        }
        path.to_string()
    } else {
        // Scheme-less pastes: strip a bare host prefix, keep original case.
        let lower = without_fragment.to_ascii_lowercase();
        [
            "huggingface.co/",
            "www.huggingface.co/",
            "hf.co/",
            "www.hf.co/",
        ]
        .iter()
        .find_map(|host| {
            lower
                .starts_with(host)
                .then(|| without_fragment[host.len()..].to_string())
        })
        .unwrap_or(without_fragment)
    };

    parse_hf_path(&path)
}

/// Parse the path part (everything after the host, or the bare input).
fn parse_hf_path(path: &str) -> Result<ParsedHfInput, String> {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return Err("no repository id found".to_string());
    }
    for segment in &segments {
        if !valid_repo_segment(segment) {
            return Err(format!("invalid path segment: {:?}", segment));
        }
    }

    // A URL-style file view or download link: owner/repo/blob/REV/[file...].
    if segments.len() >= 3 && matches!(segments[2], "blob" | "resolve" | "raw" | "tree") {
        let repo_id = format!("{}/{}", segments[0], segments[1]);
        let revision = segments.get(3).map(|r| r.to_string());
        let file = segments[4..].join("/");
        if file.is_empty() {
            return Ok(ParsedHfInput {
                repo_id,
                filename: None,
                revision,
            });
        }
        if !file.to_ascii_lowercase().ends_with(".gguf") {
            // A tree view of a folder names no single file: keep the repo
            // (and revision) and let the picker list every GGUF. For the
            // explicit file views a non-GGUF target is a user error.
            if segments[2] == "tree" {
                return Ok(ParsedHfInput {
                    repo_id,
                    filename: None,
                    revision,
                });
            }
            return Err(format!(
                "{:?} is not a .gguf file; only GGUF models are supported",
                file
            ));
        }
        return Ok(ParsedHfInput {
            repo_id,
            filename: Some(file),
            revision,
        });
    }

    // "owner/repo/file.gguf" (with optional subfolders).
    if segments.len() >= 3
        && segments[segments.len() - 1]
            .to_ascii_lowercase()
            .ends_with(".gguf")
    {
        return Ok(ParsedHfInput {
            repo_id: format!("{}/{}", segments[0], segments[1]),
            filename: Some(segments[2..].join("/")),
            revision: None,
        });
    }

    // Bare repo: exactly owner/name.
    if segments.len() == 2 {
        return Ok(ParsedHfInput {
            repo_id: format!("{}/{}", segments[0], segments[1]),
            filename: None,
            revision: None,
        });
    }

    Err("expected owner/repo, owner/repo/file.gguf, or a huggingface.co URL".to_string())
}

/// Loose validity for one path segment: no traversal, no whitespace, no
/// scheme or Windows-separator smuggled in. Real Hub ids and filenames are
/// `[A-Za-z0-9._-]`, but staying loose costs nothing and matches what the
/// metadata round trip rejects anyway.
fn valid_repo_segment(segment: &str) -> bool {
    segment
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' '))
        && !segment.starts_with('.')
        && !segment.ends_with('.')
        && !segment.contains("..")
}

/// One `.gguf` file inside a Hugging Face repo, as listed by the metadata API.
#[derive(Debug, Clone, Serialize, Type)]
pub struct HfRepoFile {
    /// Path within the repo (subfolders included).
    pub filename: String,
    /// Blob size in bytes when the API reported it.
    pub size_bytes: Option<u64>,
}

/// The repo listing handed to the UI when the user's input resolves.
#[derive(Debug, Clone, Serialize, Type)]
pub struct HfModelResolution {
    pub repo_id: String,
    /// Commit sha the listing was read at. Pinning the download to it keeps
    /// the fetched bytes identical to the listed sizes; `None` falls back to
    /// `main`.
    pub revision: Option<String>,
    /// Every `.gguf` in the repo, sorted by filename for display.
    pub files: Vec<HfRepoFile>,
    /// The file to download when the user does not choose one: the input's
    /// own file, else the quant preference below.
    pub suggested_filename: String,
}

/// Structured failure kinds for the add-from-Hugging-Face flow, so the
/// frontend can localize each instead of showing raw error strings.
#[derive(Debug, Clone, Serialize, Type)]
pub enum HfModelError {
    /// The pasted text is not a repo URL, owner/name, or owner/name/file.gguf.
    InvalidInput {
        detail: String,
    },
    RepoNotFound {
        repo_id: String,
    },
    /// The repo cannot be read anonymously: to an unauthenticated caller the
    /// Hub answers 401 identically for a repo that does not exist and one
    /// that is private or gated, so the two cannot be told apart. v1 supports
    /// public repos only (no token is ever sent).
    Inaccessible {
        repo_id: String,
    },
    FileNotFound {
        repo_id: String,
        filename: String,
    },
    NoGgufFiles {
        repo_id: String,
    },
    Network {
        detail: String,
    },
    DownloadFailed {
        detail: String,
    },
    /// The user cancelled the download (the partial is kept for resume).
    Cancelled,
    /// The downloaded GGUF's `general.architecture` is not one the engines
    /// support; nothing was registered and the blob was deleted.
    UnsupportedArchitecture {
        architecture: Option<String>,
        supported: Vec<String>,
    },
}

/// Which file to download from a repo the user did not narrow down: the first
/// preference token present in a filename (largest file winning within that
/// token), else the largest file overall. Ties break by filename so the
/// choice is deterministic. Sizes the API did not report count as 0.
pub fn preferred_gguf<'a>(files: &'a [HfRepoFile]) -> Option<&'a HfRepoFile> {
    preferred_gguf_by(files, QUANT_PREFERENCE)
}

/// Quantization preference for the POST-PROCESS add flow: smaller default
/// quants first (the swap engine is sized for small models), unlike the voice
/// flow's accuracy-first Q8_0 ordering.
const LLM_QUANT_PREFERENCE: &[&str] = &["Q4_K_M", "Q6_K", "Q5_K_M", "Q8_0"];

/// [`preferred_gguf`]'s engine-specific sibling for post-process LLM repos.
pub fn preferred_llm_gguf<'a>(files: &'a [HfRepoFile]) -> Option<&'a HfRepoFile> {
    preferred_gguf_by(files, LLM_QUANT_PREFERENCE)
}

/// The shared first-token-then-largest rule behind both preferences.
fn preferred_gguf_by<'a>(files: &'a [HfRepoFile], preference: &[&str]) -> Option<&'a HfRepoFile> {
    let size_of = |f: &HfRepoFile| f.size_bytes.unwrap_or(0);
    let mut candidates: Vec<&HfRepoFile> = files.iter().collect();
    candidates.sort_by(|a, b| {
        size_of(b)
            .cmp(&size_of(a))
            .then_with(|| a.filename.cmp(&b.filename))
    });

    for token in preference {
        if let Some(best) = candidates
            .iter()
            .copied()
            .find(|f| f.filename.to_ascii_uppercase().contains(token))
        {
            return Some(best);
        }
    }
    candidates.first().copied()
}

/// The `?blobs=true` model-info payload, parsed locally because hf-hub's own
/// `RepoInfo` does not carry file sizes.
#[derive(Deserialize)]
struct HfApiModelInfo {
    #[serde(default)]
    sha: Option<String>,
    #[serde(default)]
    siblings: Vec<HfApiSibling>,
}

#[derive(Deserialize)]
struct HfApiSibling {
    rfilename: String,
    #[serde(default)]
    size: Option<u64>,
}

/// List a repo's `.gguf` files (with sizes) plus the commit sha the listing
/// was read at, through the Hub metadata API. Uses hf-hub's own client and
/// endpoint resolution (`HF_ENDPOINT` honored, token cleared) so listing and
/// downloading always agree on where the Hub is.
async fn list_repo_ggufs(
    repo_id: &str,
    revision: &str,
) -> Result<(Option<String>, Vec<HfRepoFile>), HfModelError> {
    let api = ApiBuilder::from_env()
        .with_token(None)
        .with_progress(false)
        .build()
        .map_err(|e| HfModelError::Network {
            detail: format!("failed to init Hugging Face API client: {}", e),
        })?;
    let repo = api.repo(Repo::with_revision(
        repo_id.to_string(),
        RepoType::Model,
        revision.to_string(),
    ));
    // blobs=true makes each sibling carry its size.
    let request = repo.info_request().query(&[("blobs", "true")]);

    let response = tokio::time::timeout(HF_METADATA_TIMEOUT, request.send())
        .await
        .map_err(|_| HfModelError::Network {
            detail: format!(
                "no response from Hugging Face within {}s",
                HF_METADATA_TIMEOUT.as_secs()
            ),
        })?
        .map_err(|e| HfModelError::Network {
            detail: format!("{:?}", e),
        })?;

    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        // The Hub deliberately answers 401 the same way for a missing repo
        // and a private one when the caller is anonymous, so both land here.
        return Err(HfModelError::Inaccessible {
            repo_id: repo_id.to_string(),
        });
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(HfModelError::RepoNotFound {
            repo_id: repo_id.to_string(),
        });
    }
    if !status.is_success() {
        return Err(HfModelError::Network {
            detail: format!("Hugging Face API returned HTTP {}", status),
        });
    }

    let info: HfApiModelInfo = tokio::time::timeout(HF_METADATA_TIMEOUT, response.json())
        .await
        .map_err(|_| HfModelError::Network {
            detail: "timed out reading the repo listing".to_string(),
        })?
        .map_err(|e| HfModelError::Network {
            detail: format!("{:?}", e),
        })?;

    let mut files: Vec<HfRepoFile> = info
        .siblings
        .into_iter()
        .filter(|s| s.rfilename.to_ascii_lowercase().ends_with(".gguf"))
        .map(|s| HfRepoFile {
            filename: s.rfilename,
            size_bytes: s.size,
        })
        .collect();
    files.sort_by(|a, b| a.filename.cmp(&b.filename));
    Ok((info.sha, files))
}

/// Resolve pasted input against the Hub: parse it, list the repo's GGUF files
/// with sizes, and suggest one. Errors are structured ([`HfModelError`]) so
/// the UI can localize them.
pub async fn resolve_hf_repo(raw: &str) -> Result<HfModelResolution, HfModelError> {
    let parsed = parse_hf_input(raw).map_err(|detail| HfModelError::InvalidInput { detail })?;
    let revision = parsed
        .revision
        .clone()
        .unwrap_or_else(|| "main".to_string());
    let (sha, files) = list_repo_ggufs(&parsed.repo_id, &revision).await?;
    if files.is_empty() {
        return Err(HfModelError::NoGgufFiles {
            repo_id: parsed.repo_id,
        });
    }
    if let Some(wanted) = parsed.filename.as_deref() {
        if !files.iter().any(|f| f.filename == wanted) {
            return Err(HfModelError::FileNotFound {
                repo_id: parsed.repo_id,
                filename: wanted.to_string(),
            });
        }
    }
    let suggested_filename = parsed
        .filename
        .clone()
        .or_else(|| preferred_gguf(&files).map(|f| f.filename.clone()))
        .unwrap_or_else(|| files[0].filename.clone());
    Ok(HfModelResolution {
        repo_id: parsed.repo_id,
        revision: sha,
        files,
        suggested_filename,
    })
}

/// Registry entry for a user-added Hugging Face repo file. Field-for-field
/// the shape `discover_hf_cache_models_in` derives for an unknown repo
/// (minus the probe, which only runs after download), so the entry created
/// here and the one a later discovery scan re-derives are interchangeable:
/// same id, same metadata, no duplicates across rescans and restarts.
pub(super) fn user_added_hf_model_info(
    repo_id: &str,
    revision: &str,
    filename: &str,
    probe: Option<&CapabilityProbe>,
) -> ModelInfo {
    let caps = probe.map(local_caps);
    let display = probe
        .and_then(probed_display_name)
        .unwrap_or_else(|| filename.trim_end_matches(".gguf").to_string());
    let (
        supports_translation,
        supported_languages,
        supports_language_selection,
        supports_streaming,
        supports_language_detection,
    ) = match caps {
        Some(c) => (
            c.supports_translation,
            c.supported_languages,
            c.supports_language_selection,
            c.supports_streaming,
            c.supports_language_detection,
        ),
        None => (false, Vec::new(), false, false, false),
    };
    ModelInfo {
        id: format!("{}/{}", repo_id, filename),
        name: display,
        description: format!("From Hugging Face cache: {}", repo_id),
        filename: filename.to_string(),
        source: ModelSource::HuggingFace {
            repo_id: repo_id.to_string(),
            revision: revision.to_string(),
        },
        size_mb: 0,
        is_downloaded: false,
        is_downloading: false,
        partial_size: 0,
        is_directory: false,
        engine_type: EngineType::TranscribeCpp,
        accuracy_score: 0.0,
        speed_score: 0.0,
        supports_translation,
        is_recommended: false,
        supported_languages,
        supports_language_selection,
        is_custom: false,
        supports_streaming,
        supports_language_detection,
    }
}

/// The post-download gate: refuse to register anything whose GGUF
/// architecture is not in [`KNOWN_ARCHES`], the authoritative list of
/// architectures the transcription engines can load. The error names the
/// model's architecture and the supported families.
fn verify_supported_architecture(probe: &CapabilityProbe) -> Result<(), HfModelError> {
    if probe.verdict == Compatibility::Compatible {
        return Ok(());
    }
    Err(HfModelError::UnsupportedArchitecture {
        architecture: probe.architecture.clone(),
        supported: KNOWN_ARCHES.iter().map(|s| s.to_string()).collect(),
    })
}

/// The LLM flow's architecture gate: the mirror image of
/// [`verify_supported_architecture`]. A GGUF must declare an architecture the
/// llama.cpp worker can load ([`LLM_ARCHES`]) - which, because the two tables
/// are disjoint, simultaneously guarantees the voice flow would refuse it.
fn verify_supported_llm_architecture(probe: &CapabilityProbe) -> Result<(), HfModelError> {
    let ok = probe
        .architecture
        .as_deref()
        .is_some_and(|arch| LLM_ARCHES.contains(&arch));
    if ok {
        return Ok(());
    }
    Err(HfModelError::UnsupportedArchitecture {
        architecture: probe.architecture.clone(),
        supported: LLM_ARCHES.iter().map(|s| s.to_string()).collect(),
    })
}

/// The model's own declared context length (`{arch}.context_length` in the
/// GGUF header), when present and sane. Clamped to the swap engine's
/// operating band: the token budgets in `local_llm::forecast` are designed
/// around 4096-token contexts, so an advertised 128k native window is NOT a
/// reason to allocate one.
pub(super) fn probe_llm_context_tokens(path: &std::path::Path) -> Option<u32> {
    use crate::managers::model_capabilities::read_header_metadata_for;
    const KEY_ARCH: &str = "general.architecture";
    let probe = GgufHeaderProber.probe_file(path);
    let arch = probe.architecture?;
    let key = format!("{arch}.context_length");
    let meta = read_header_metadata_for(path, &[KEY_ARCH, key.as_str()]).ok()?;
    let ctx = meta.get_u32(&key)?;
    if (256..=32 * 1024).contains(&ctx) {
        Some(ctx)
    } else {
        None
    }
}

/// Registry entry for a user-added post-process LLM repo file: the
/// `EngineType::LocalLlm` sibling of [`user_added_hf_model_info`],
/// field-compatible with what `discover_llm_cache_models` derives, so the
/// entry created here and the one a later scan re-derives are the same id
/// with the same shape. `is_custom: true` gives it the vanish-when-missing
/// semantics a discovery-born entry needs.
pub(super) fn user_added_llm_model_info(
    repo_id: &str,
    revision: &str,
    filename: &str,
    probe: Option<&CapabilityProbe>,
    context_tokens: Option<u32>,
) -> ModelInfo {
    let display = probe
        .and_then(probed_display_name)
        .unwrap_or_else(|| filename.trim_end_matches(".gguf").to_string());
    let info = ModelInfo {
        id: format!("{}/{}", repo_id, filename),
        name: display,
        description: format!("Local post-process model from Hugging Face: {}", repo_id),
        filename: filename.to_string(),
        source: ModelSource::HuggingFace {
            repo_id: repo_id.to_string(),
            revision: revision.to_string(),
        },
        size_mb: 0,
        is_downloaded: false,
        is_downloading: false,
        partial_size: 0,
        is_directory: false,
        engine_type: EngineType::LocalLlm,
        accuracy_score: 0.0,
        speed_score: 0.0,
        supports_translation: false,
        is_recommended: false,
        supported_languages: Vec::new(),
        supports_language_selection: false,
        is_custom: true,
        supports_streaming: false,
        supports_language_detection: false,
    };
    if let Some(ctx) = context_tokens {
        crate::catalog::llm::register_dynamic_context(&info.id, ctx);
    }
    info
}

/// Resolve pasted input against the Hub for the POST-PROCESS flow: same
/// parse + listing pipeline as [`resolve_hf_repo`], but the suggestion uses
/// the smaller-first LLM quant preference.
pub async fn resolve_llm_hf_repo(raw: &str) -> Result<HfModelResolution, HfModelError> {
    let parsed = parse_hf_input(raw).map_err(|detail| HfModelError::InvalidInput { detail })?;
    let revision = parsed
        .revision
        .clone()
        .unwrap_or_else(|| "main".to_string());
    let (sha, files) = list_repo_ggufs(&parsed.repo_id, &revision).await?;
    if files.is_empty() {
        return Err(HfModelError::NoGgufFiles {
            repo_id: parsed.repo_id,
        });
    }
    if let Some(wanted) = parsed.filename.as_deref() {
        if !files.iter().any(|f| f.filename == wanted) {
            return Err(HfModelError::FileNotFound {
                repo_id: parsed.repo_id,
                filename: wanted.to_string(),
            });
        }
    }
    let suggested_filename = parsed
        .filename
        .clone()
        .or_else(|| preferred_llm_gguf(&files).map(|f| f.filename.clone()))
        .unwrap_or_else(|| files[0].filename.clone());
    Ok(HfModelResolution {
        repo_id: parsed.repo_id,
        revision: sha,
        files,
        suggested_filename,
    })
}

impl ModelManager {
    /// Register, download, and architecture-check a user-added Hugging Face
    /// model. Returns the registry id (`"{repo_id}/{filename}"`).
    ///
    /// The download reuses the ordinary HF path, so progress events,
    /// cancellation, the stall watchdog, and the `.sync.part` resume offset
    /// all behave exactly like a catalog download. Afterwards the GGUF header
    /// is probed: an unsupported architecture refuses registration, deletes
    /// the blob this flow downloaded (a file that was already on disk is left
    /// alone), and reports the architecture plus the supported families.
    ///
    /// Nothing here downloads without the user's explicit Add action; the
    /// RAM auto-fallback only ever considers what is already on disk.
    pub async fn add_hf_model(
        &self,
        repo_id: &str,
        filename: &str,
        revision: Option<&str>,
    ) -> Result<String, HfModelError> {
        let revision = revision.unwrap_or("main");
        let model_id = format!("{}/{}", repo_id, filename);

        // Was the file already on disk (another tool, or an earlier add)?
        // The download then short-circuits, and a later refusal must not
        // delete bytes this flow never fetched.
        let preexisting = hf_cached_path(repo_id, revision, filename).is_some()
            || self.models_dir.join(filename).exists();

        // Register the provisional entry unless the registry already knows
        // the id (a catalog entry, an earlier discovery, or a previous add).
        let inserted_by_us = if self.get_model_info(&model_id).is_none() {
            let mut info = user_added_hf_model_info(repo_id, revision, filename, None);
            // The card shows "downloading" from the moment it appears; the
            // download path sets the flag again once it starts.
            info.is_downloading = true;
            self.available_models
                .lock()
                .unwrap()
                .insert(model_id.clone(), info);
            let _ = self.app_handle.emit("models-updated", ());
            true
        } else {
            false
        };

        if let Err(e) = self.download_model(&model_id).await {
            let detail = e.to_string();
            info!(
                "Add from Hugging Face failed to download {}: {}",
                model_id, detail
            );
            self.remove_added_entry(&model_id, inserted_by_us);
            // A gated repo can pass the metadata listing but 401/403 the
            // file bytes; hf-hub surfaces that inside the reqwest error text.
            if detail.contains("401") || detail.contains("403") {
                return Err(HfModelError::Inaccessible {
                    repo_id: repo_id.to_string(),
                });
            }
            let _ = self.app_handle.emit(
                "model-download-failed",
                serde_json::json!({ "model_id": &model_id, "error": &detail }),
            );
            return Err(HfModelError::DownloadFailed { detail });
        }

        // download_model returns Ok on user cancel (partial kept), so verify
        // the file actually landed before probing.
        let path = match self.get_model_path(&model_id) {
            Ok(path) => path,
            Err(_) => {
                self.remove_added_entry(&model_id, inserted_by_us);
                return Err(HfModelError::Cancelled);
            }
        };

        let probe = GgufHeaderProber.probe_file(&path);
        if let Err(error) = verify_supported_architecture(&probe) {
            warn!(
                "Refusing to add {}: unsupported architecture {:?}",
                model_id, probe.architecture
            );
            self.remove_added_entry(&model_id, inserted_by_us);
            if !preexisting {
                Self::delete_hf_cache_file(repo_id, revision, filename);
            }
            let summary = match probe.architecture.as_deref() {
                Some(arch) => format!(
                    "architecture {} is not supported (supported: {})",
                    arch,
                    KNOWN_ARCHES.join(", ")
                ),
                None => "file could not be read as a GGUF model".to_string(),
            };
            let _ = self.app_handle.emit(
                "model-download-failed",
                serde_json::json!({ "model_id": &model_id, "error": summary }),
            );
            let _ = self.app_handle.emit("models-updated", ());
            return Err(error);
        }

        // Enrich our entry from the probe (display name, capabilities, real
        // size) so it matches what discovery will re-derive later. A
        // pre-existing entry keeps its (richer) catalog metadata.
        if inserted_by_us {
            let mut enriched = user_added_hf_model_info(repo_id, revision, filename, Some(&probe));
            enriched.size_mb = path
                .metadata()
                .map(|m| m.len() / (1024 * 1024))
                .unwrap_or(0);
            enriched.is_downloaded = true;
            self.available_models
                .lock()
                .unwrap()
                .insert(model_id.clone(), enriched);
        }
        let _ = self.app_handle.emit("models-updated", ());
        info!("Added model from Hugging Face: {}", model_id);
        Ok(model_id)
    }

    /// Drop a provisional entry this flow added, unless the file somehow
    /// ended up downloaded after all. Entries that pre-existed (catalog,
    /// discovery) are never touched.
    fn remove_added_entry(&self, model_id: &str, inserted_by_us: bool) {
        if !inserted_by_us {
            return;
        }
        let mut models = self.available_models.lock().unwrap();
        if models.get(model_id).is_none_or(|m| !m.is_downloaded) {
            models.remove(model_id);
        }
    }

    /// The post-process sibling of [`Self::add_hf_model`]: register a
    /// user-added Hugging Face GGUF as an `EngineType::LocalLlm` entry,
    /// download it through the ordinary pipeline (which routes into the
    /// dedicated llm-models cache), and gate registration on the LLM
    /// architecture allowlist - the mirror image of the voice flow's
    /// KNOWN_ARCHES gate, so an ASR GGUF (`whisper`, `parakeet`, ...) can
    /// never register as a post-process model and vice versa.
    pub async fn add_llm_hf_model(
        &self,
        repo_id: &str,
        filename: &str,
        revision: Option<&str>,
    ) -> Result<String, HfModelError> {
        let revision = revision.unwrap_or("main");
        let model_id = format!("{}/{}", repo_id, filename);

        // Was the file already on disk (another tool, or an earlier add)? The
        // download then short-circuits, and a later refusal must not delete
        // bytes this flow never fetched. LocalLlm files live in the dedicated
        // llm cache (with the shared caches as grandfather fallback).
        let preexisting = self
            .get_model_info(&model_id)
            .is_some_and(|m| m.is_downloaded)
            || self.models_dir.join(filename).exists();

        // Register the provisional entry unless the registry already knows
        // the id (an LLM catalog entry, an earlier discovery, or a prior add).
        let inserted_by_us = if self.get_model_info(&model_id).is_none() {
            let mut info = user_added_llm_model_info(repo_id, revision, filename, None, None);
            info.is_downloading = true;
            self.available_models
                .lock()
                .unwrap()
                .insert(model_id.clone(), info);
            let _ = self.app_handle.emit("models-updated", ());
            true
        } else {
            false
        };

        if let Err(e) = self.download_model(&model_id).await {
            let detail = e.to_string();
            info!(
                "Add LLM from Hugging Face failed to download {}: {}",
                model_id, detail
            );
            self.remove_added_entry(&model_id, inserted_by_us);
            if detail.contains("401") || detail.contains("403") {
                return Err(HfModelError::Inaccessible {
                    repo_id: repo_id.to_string(),
                });
            }
            let _ = self.app_handle.emit(
                "model-download-failed",
                serde_json::json!({
                    "model_id": &model_id,
                    "error": &detail,
                    "name": filename.trim_end_matches(".gguf"),
                }),
            );
            return Err(HfModelError::DownloadFailed { detail });
        }

        // download_model returns Ok on user cancel (partial kept), so verify
        // the file actually landed before probing.
        let path = match self.get_model_path(&model_id) {
            Ok(path) => path,
            Err(_) => {
                self.remove_added_entry(&model_id, inserted_by_us);
                return Err(HfModelError::Cancelled);
            }
        };

        let probe = GgufHeaderProber.probe_file(&path);
        if let Err(error) = verify_supported_llm_architecture(&probe) {
            warn!(
                "Refusing to add LLM {}: unsupported architecture {:?}",
                model_id, probe.architecture
            );
            self.remove_added_entry(&model_id, inserted_by_us);
            if !preexisting {
                let _ = self.delete_cached_file_for(&user_added_llm_model_info(
                    repo_id, revision, filename, None, None,
                ));
            }
            let summary = match probe.architecture.as_deref() {
                Some(arch) => format!(
                    "architecture {} is not supported for post-processing (supported: {})",
                    arch,
                    LLM_ARCHES.join(", ")
                ),
                None => "file could not be read as a GGUF model".to_string(),
            };
            let _ = self.app_handle.emit(
                "model-download-failed",
                serde_json::json!({ "model_id": &model_id, "error": &summary }),
            );
            let _ = self.app_handle.emit("models-updated", ());
            return Err(error);
        }

        // Enrich our entry from the probe (display name, real size, the
        // model's own context length) so it matches what the llm-cache
        // discovery re-derives later. A pre-existing entry keeps its
        // (richer) catalog metadata.
        if inserted_by_us {
            let context_tokens = probe_llm_context_tokens(&path);
            let mut enriched = user_added_llm_model_info(
                repo_id,
                revision,
                filename,
                Some(&probe),
                context_tokens,
            );
            enriched.size_mb = path
                .metadata()
                .map(|m| m.len() / (1024 * 1024))
                .unwrap_or(0);
            enriched.is_downloaded = true;
            self.available_models
                .lock()
                .unwrap()
                .insert(model_id.clone(), enriched);
        }
        let _ = self.app_handle.emit("models-updated", ());
        info!("Added post-process LLM from Hugging Face: {}", model_id);
        Ok(model_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;
    use tempfile::TempDir;

    fn parsed(repo_id: &str, filename: Option<&str>, revision: Option<&str>) -> ParsedHfInput {
        ParsedHfInput {
            repo_id: repo_id.to_string(),
            filename: filename.map(str::to_string),
            revision: revision.map(str::to_string),
        }
    }

    fn file(name: &str, size: Option<u64>) -> HfRepoFile {
        HfRepoFile {
            filename: name.to_string(),
            size_bytes: size,
        }
    }

    #[test]
    fn parse_accepts_blob_and_resolve_urls() {
        assert_eq!(
            parse_hf_input("https://huggingface.co/org/repo/blob/main/model-Q8_0.gguf"),
            Ok(parsed("org/repo", Some("model-Q8_0.gguf"), Some("main")))
        );
        assert_eq!(
            parse_hf_input(
                "https://huggingface.co/org/repo/resolve/abc123/sub/dir/model.gguf?download=true"
            ),
            Ok(parsed(
                "org/repo",
                Some("sub/dir/model.gguf"),
                Some("abc123")
            ))
        );
        assert_eq!(
            parse_hf_input("https://hf.co/org/repo/raw/main/m.gguf"),
            Ok(parsed("org/repo", Some("m.gguf"), Some("main")))
        );
    }

    #[test]
    fn parse_accepts_shorthand_and_bare_repo_forms() {
        assert_eq!(
            parse_hf_input("org/repo/file.gguf"),
            Ok(parsed("org/repo", Some("file.gguf"), None))
        );
        assert_eq!(
            parse_hf_input("org/repo/sub/file.gguf"),
            Ok(parsed("org/repo", Some("sub/file.gguf"), None))
        );
        assert_eq!(
            parse_hf_input("org/repo"),
            Ok(parsed("org/repo", None, None))
        );
        // Scheme-less host prefix and trailing slash variants.
        assert_eq!(
            parse_hf_input("huggingface.co/org/repo/"),
            Ok(parsed("org/repo", None, None))
        );
        // URL to the repo itself, no file view.
        assert_eq!(
            parse_hf_input("https://huggingface.co/org/repo"),
            Ok(parsed("org/repo", None, None))
        );
        // tree view of a file behaves like blob; of a folder falls back to
        // the whole repo with the revision kept.
        assert_eq!(
            parse_hf_input("https://huggingface.co/org/repo/tree/dev/m.gguf"),
            Ok(parsed("org/repo", Some("m.gguf"), Some("dev")))
        );
        assert_eq!(
            parse_hf_input("https://huggingface.co/org/repo/tree/dev/subdir"),
            Ok(parsed("org/repo", None, Some("dev")))
        );
    }

    #[test]
    fn parse_rejects_garbage_and_unsupported_files() {
        assert!(parse_hf_input("").is_err());
        assert!(parse_hf_input("   ").is_err());
        // A bare name is not a repo id.
        assert!(parse_hf_input("somename").is_err());
        // Wrong host.
        assert!(parse_hf_input("https://example.com/org/repo/m.gguf").is_err());
        // Non-GGUF explicit files are refused up front.
        assert!(parse_hf_input("https://huggingface.co/org/repo/blob/main/README.md").is_err());
        assert!(parse_hf_input("org/repo/model.bin").is_err());
        // Path traversal never reaches a filesystem join.
        assert!(parse_hf_input("org/repo/../escape.gguf").is_err());
        assert!(parse_hf_input("https://huggingface.co/org/repo/blob/main/../x.gguf").is_err());
    }

    #[test]
    fn parse_accepts_uppercase_gguf_extension() {
        assert_eq!(
            parse_hf_input("org/repo/MODEL.GGUF"),
            Ok(parsed("org/repo", Some("MODEL.GGUF"), None))
        );
    }

    #[test]
    fn quant_preference_follows_the_documented_order() {
        let files = vec![
            file("model-Q3_K_M.gguf", Some(300)),
            file("model-Q4_K_M.gguf", Some(400)),
            file("model-Q6_K.gguf", Some(550)),
            file("model-Q8_0.gguf", Some(700)),
        ];
        assert_eq!(
            preferred_gguf(&files).map(|f| f.filename.as_str()),
            Some("model-Q8_0.gguf")
        );

        let without_q8 = &files[..3];
        assert_eq!(
            preferred_gguf(without_q8).map(|f| f.filename.as_str()),
            Some("model-Q6_K.gguf")
        );

        let only_low = &files[..2];
        assert_eq!(
            preferred_gguf(only_low).map(|f| f.filename.as_str()),
            Some("model-Q4_K_M.gguf")
        );
    }

    #[test]
    fn quant_preference_picks_the_largest_when_no_preferred_quant_exists() {
        let files = vec![
            file("model-IQ2_XS.gguf", Some(120)),
            file("model-F16.gguf", Some(1600)),
            file("model-BF16.gguf", Some(1500)),
        ];
        assert_eq!(
            preferred_gguf(&files).map(|f| f.filename.as_str()),
            Some("model-F16.gguf")
        );
        // Unknown sizes count as 0, so the alphabetical tiebreak decides.
        let unknown = vec![file("b.gguf", None), file("a.gguf", None)];
        assert_eq!(
            preferred_gguf(&unknown).map(|f| f.filename.as_str()),
            Some("a.gguf")
        );
        assert!(preferred_gguf(&[]).is_none());
    }

    #[test]
    fn quant_preference_is_case_insensitive_and_prefers_larger_within_a_token() {
        let files = vec![
            file("model-q8_0-small.gguf", Some(100)),
            file("model-q8_0.gguf", Some(700)),
        ];
        assert_eq!(
            preferred_gguf(&files).map(|f| f.filename.as_str()),
            Some("model-q8_0.gguf")
        );
    }

    fn probe_with_arch(arch: Option<&str>) -> CapabilityProbe {
        match arch {
            Some(arch) => CapabilityProbe {
                verdict: if KNOWN_ARCHES.contains(&arch) {
                    Compatibility::Compatible
                } else {
                    Compatibility::MaybeIncompatible
                },
                architecture: Some(arch.to_string()),
                ..Default::default()
            },
            None => CapabilityProbe {
                verdict: Compatibility::Unsupported,
                ..Default::default()
            },
        }
    }

    #[test]
    fn architecture_gate_accepts_known_and_refuses_unknown_archs() {
        assert!(verify_supported_architecture(&probe_with_arch(Some("whisper"))).is_ok());
        assert!(verify_supported_architecture(&probe_with_arch(Some("parakeet"))).is_ok());

        let err = verify_supported_architecture(&probe_with_arch(Some("llama"))).unwrap_err();
        match err {
            HfModelError::UnsupportedArchitecture {
                architecture,
                supported,
            } => {
                assert_eq!(architecture.as_deref(), Some("llama"));
                assert_eq!(
                    supported,
                    KNOWN_ARCHES
                        .iter()
                        .map(|s| s.to_string())
                        .collect::<Vec<_>>()
                );
                assert!(supported.contains(&"whisper".to_string()));
            }
            other => panic!("expected UnsupportedArchitecture, got {:?}", other),
        }

        // Unreadable files name no architecture at all.
        let err = verify_supported_architecture(&probe_with_arch(None)).unwrap_err();
        match err {
            HfModelError::UnsupportedArchitecture { architecture, .. } => {
                assert_eq!(architecture, None);
            }
            other => panic!("expected UnsupportedArchitecture, got {:?}", other),
        }
    }

    #[test]
    fn user_added_entry_matches_the_discovery_shape() {
        let info = user_added_hf_model_info("org/repo", "main", "model-Q8_0.gguf", None);
        assert_eq!(info.id, "org/repo/model-Q8_0.gguf");
        assert_eq!(info.name, "model-Q8_0");
        assert_eq!(info.description, "From Hugging Face cache: org/repo");
        assert!(matches!(
            &info.source,
            ModelSource::HuggingFace { repo_id, revision }
                if repo_id == "org/repo" && revision == "main"
        ));
        assert!(matches!(info.engine_type, EngineType::TranscribeCpp));
        assert!(!info.is_custom);
        assert!(!info.is_downloaded);
        assert!(!info.supports_streaming);
        assert!(info.supported_languages.is_empty());
    }

    /// The two architecture gates are mirrors: LLM archs (qwen3, llama,
    /// gemma3, ...) pass the LLM path and are refused by the voice path;
    /// ASR-only archs (whisper) pass the voice path and are refused by the
    /// LLM path. Neither flow can ever register a model for the wrong
    /// engine.
    #[test]
    fn llm_architecture_gate_mirrors_the_voice_gate() {
        for arch in ["qwen3", "llama", "gemma3", "phi3", "phi4"] {
            assert!(
                verify_supported_llm_architecture(&probe_with_arch(Some(arch))).is_ok(),
                "{arch} must pass the LLM gate"
            );
            assert!(
                verify_supported_architecture(&probe_with_arch(Some(arch))).is_err(),
                "{arch} must STILL be refused by the voice gate"
            );
        }

        // ASR-only archs: refused on the LLM path, error naming the LLM
        // allowlist.
        let err = verify_supported_llm_architecture(&probe_with_arch(Some("whisper")))
            .expect_err("whisper is not a post-process architecture");
        match err {
            HfModelError::UnsupportedArchitecture {
                architecture,
                supported,
            } => {
                assert_eq!(architecture.as_deref(), Some("whisper"));
                assert_eq!(
                    supported,
                    LLM_ARCHES.iter().map(|s| s.to_string()).collect::<Vec<_>>()
                );
            }
            other => panic!("expected UnsupportedArchitecture, got {:?}", other),
        }
        assert!(verify_supported_architecture(&probe_with_arch(Some("whisper"))).is_ok());

        // Unreadable GGUFs name no architecture and are refused by both.
        assert!(verify_supported_llm_architecture(&probe_with_arch(None)).is_err());
    }

    /// The user-added LLM entry shape: LocalLlm engine, custom flag on (so
    /// it vanishes with its file), HF source pinning the revision - the
    /// same fields the llm-cache discovery re-derives on restart.
    #[test]
    fn user_added_llm_entry_matches_the_llm_discovery_shape() {
        let info = user_added_llm_model_info("org/repo", "main", "model-Q8_0.gguf", None, None);
        assert_eq!(info.id, "org/repo/model-Q8_0.gguf");
        assert_eq!(info.name, "model-Q8_0");
        assert!(matches!(info.engine_type, EngineType::LocalLlm));
        assert!(info.is_custom, "user-added LLMs vanish with their file");
        assert!(!info.is_downloaded);
        assert!(matches!(
            &info.source,
            ModelSource::HuggingFace { repo_id, revision }
                if repo_id == "org/repo" && revision == "main"
        ));
        // A probed context is registered for the runner under the entry's id.
        user_added_llm_model_info("org/repo2", "main", "model.gguf", None, Some(8192));
        assert_eq!(
            crate::catalog::llm::context_tokens_for("org/repo2/model.gguf"),
            8192
        );
    }

    /// The LLM suggestion prefers smaller quants first (the swap engine is
    /// sized for small models), the inverse of the voice flow's Q8_0-first
    /// order.
    #[test]
    fn llm_quant_preference_is_smaller_first() {
        let files = vec![
            file("model-Q3_K_M.gguf", Some(300)),
            file("model-Q4_K_M.gguf", Some(400)),
            file("model-Q6_K.gguf", Some(550)),
            file("model-Q8_0.gguf", Some(700)),
        ];
        assert_eq!(
            preferred_llm_gguf(&files).map(|f| f.filename.as_str()),
            Some("model-Q4_K_M.gguf")
        );
        // Without any preferred quant: largest file wins, same as voice.
        let no_preferred = vec![
            file("model-IQ2_XS.gguf", Some(120)),
            file("model-F16.gguf", Some(1600)),
        ];
        assert_eq!(
            preferred_llm_gguf(&no_preferred).map(|f| f.filename.as_str()),
            Some("model-F16.gguf")
        );
    }

    // --- persistence across rescan/restart -------------------------------

    fn push_gguf_str(out: &mut Vec<u8>, val: &str) {
        out.extend_from_slice(&(val.len() as u64).to_le_bytes());
        out.extend_from_slice(val.as_bytes());
    }

    /// Minimal GGUF whose header carries only `general.architecture`.
    fn write_synthetic_gguf(path: &std::path::Path, arch: &str) {
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(&0x4655_4747u32.to_le_bytes()); // magic "GGUF"
        out.extend_from_slice(&3u32.to_le_bytes()); // version
        out.extend_from_slice(&0u64.to_le_bytes()); // tensor_count
        out.extend_from_slice(&1u64.to_le_bytes()); // kv_count
        push_gguf_str(&mut out, "general.architecture");
        out.extend_from_slice(&8u32.to_le_bytes()); // STRING
        push_gguf_str(&mut out, arch);
        fs::write(path, out).unwrap();
    }

    fn synthetic_cache_with(repo_id: &str, filename: &str, arch: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        let repo_dir = tmp
            .path()
            .join(format!("models--{}", repo_id.replace('/', "--")));
        let snapshot = repo_dir.join("snapshots").join("abc123");
        fs::create_dir_all(&snapshot).unwrap();
        fs::create_dir_all(repo_dir.join("refs")).unwrap();
        fs::write(repo_dir.join("refs").join("main"), "abc123").unwrap();
        write_synthetic_gguf(&snapshot.join(filename), arch);
        tmp
    }

    #[test]
    fn added_entry_survives_a_rescan_without_duplicates() {
        // The file the add flow just downloaded, sitting in the HF cache.
        let cache = synthetic_cache_with("org/custom-asr", "model-Q8_0.gguf", "whisper");

        // Registry state after a successful add: our entry, marked downloaded.
        let mut models = HashMap::new();
        let mut added = user_added_hf_model_info("org/custom-asr", "main", "model-Q8_0.gguf", None);
        added.is_downloaded = true;
        models.insert(added.id.clone(), added);

        // A rescan (or restart) runs the HF cache discovery over the same
        // cache: the file is already represented, so nothing is inserted.
        ModelManager::discover_hf_cache_models_in(cache.path(), &mut models);
        assert_eq!(models.len(), 1, "rescan must not duplicate the entry");
        let entry = models.get("org/custom-asr/model-Q8_0.gguf").unwrap();
        assert!(entry.is_downloaded);
        assert!(matches!(entry.source, ModelSource::HuggingFace { .. }));
    }

    #[test]
    fn discovery_rederives_the_same_id_after_a_restart() {
        let cache = synthetic_cache_with("org/custom-asr", "model-Q8_0.gguf", "whisper");

        // Fresh start (empty registry): discovery finds the cached file and
        // registers exactly the id the add flow created, so the persisted
        // selection and history keep pointing at the same model.
        let mut models = HashMap::new();
        ModelManager::discover_hf_cache_models_in(cache.path(), &mut models);
        let entry = models
            .get("org/custom-asr/model-Q8_0.gguf")
            .expect("rediscovered");
        assert!(entry.is_downloaded);
        assert!(!entry.is_custom);
    }

    #[test]
    fn unsupported_arch_file_is_never_registered_by_discovery() {
        // The refusal contract's other half: even with the blob present in
        // the cache (e.g. another tool downloaded it), discovery ignores it.
        let cache = synthetic_cache_with("org/llama-hub", "llama.gguf", "llama");
        let mut models = HashMap::new();
        ModelManager::discover_hf_cache_models_in(cache.path(), &mut models);
        assert!(models.is_empty(), "non-ASR gguf must be ignored");
    }
}
