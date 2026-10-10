//! The bundled post-process LLM catalog - the voice catalog's sibling.
//!
//! `llm_catalog.json` mirrors the trust-anchor shape of `catalog.json` (a
//! pinned HF revision per repo, one entry per quantization file carrying
//! `sha256` + `size_bytes`, the same [`QuantFile`] type deserialized straight
//! from the JSON) but describes the GGUFs the local post-process engine's
//! llama.cpp worker can load instead of ASR models. Two fields the ASR
//! catalog has no counterpart for, because the LLM runner needs them:
//!
//! - `context_tokens`: the per-model n_ctx the swap runner allocates
//!   ([`crate::local_llm::protocol::WORKER_N_CTX`] is the default).
//! - `publisher`: a display string for the model card ("Qwen", "bartowski").
//!
//! Registration lives in [`crate::managers::model::ModelManager`]: every
//! catalog entry becomes an `EngineType::LocalLlm` [`ModelInfo`] in the shared
//! registry (download / progress / cancel / delete for free), and the
//! `get_available_models` filter keeps them out of every ASR surface exactly
//! like the pinned builtin.

use once_cell::sync::Lazy;
use serde::Deserialize;

use crate::managers::model::{
    canonicalize_supported_languages, default_quant_file, DiskStatus, EngineType, ModelInfo,
    ModelSource, QuantFile,
};

/// Context window the swap runner allocates when a model carries no explicit
/// `context_tokens`. Re-exported so the catalog and the runner can never
/// disagree on the default.
pub use crate::local_llm::protocol::WORKER_N_CTX as DEFAULT_CONTEXT_TOKENS;

#[derive(Deserialize)]
struct LlmCatalogRoot {
    models: Vec<LlmCatalogModel>,
}

/// One post-process model as written in `llm_catalog.json`.
#[derive(Deserialize)]
pub struct LlmCatalogModel {
    /// HF repo id, e.g. `unsloth/Qwen3-1.7B-GGUF`.
    pub id: String,
    pub name: String,
    pub description: String,
    /// Display publisher for the card ("Qwen", "bartowski", ...).
    pub publisher: String,
    /// GGUF `general.architecture` (must pass [`crate::managers::model_capabilities::LLM_ARCHES`]).
    pub architecture: String,
    /// Inert display metadata: the languages the model handles.
    pub languages: Vec<String>,
    /// Per-model n_ctx for the worker's Load frame. Defaults to
    /// [`DEFAULT_CONTEXT_TOKENS`] (4096), the budget the forecast's
    /// input/output token math is designed around.
    #[serde(default = "default_context_tokens")]
    pub context_tokens: u32,
    /// Pinned 40-hex commit sha: the bytes behind every file's sha256 can
    /// never move, exactly like the ASR catalog's pins.
    pub revision: String,
    pub files: Vec<QuantFile>,
    pub default_quant: Option<String>,
}

fn default_context_tokens() -> u32 {
    DEFAULT_CONTEXT_TOKENS
}

impl LlmCatalogModel {
    /// The default download file (declared `default_quant`, else the first).
    pub fn default_file(&self) -> Option<&QuantFile> {
        default_quant_file(&self.files, self.default_quant.as_deref())
    }

    /// Registry id: `"{repo_id}/{default filename}"` - the same folding the
    /// ASR catalog uses, so the Qwen3-0.6B entry's id equals the pinned
    /// builtin's `LOCAL_LLM_MODEL_ID` and registration dedups onto it.
    pub fn registry_id(&self) -> String {
        format!(
            "{}/{}",
            self.id,
            self.default_file()
                .map(|f| f.filename.as_str())
                .unwrap_or_default()
        )
    }

    /// Render the frontend-facing [`ModelInfo`] for this model's default
    /// quant, combined with live disk `status`. `EngineType::LocalLlm` keeps
    /// the entry out of every ASR consumer (the registry filter, not the
    /// shape, is what separates the two worlds).
    pub fn to_model_info(&self, status: &DiskStatus) -> ModelInfo {
        self.render(self.default_file(), status)
    }

    /// [`ModelInfo`] for one specific quant `file` - how an alternate-quant
    /// LLM file found on disk surfaces with full catalog metadata instead of
    /// as an anonymous custom. Mirrors the ASR descriptor's rule: the default
    /// quant keeps the plain name, any other quant appends it so two quants
    /// of one model stay tellable apart, and identity stays
    /// `"{repo_id}/{filename}"`.
    pub fn to_model_info_for_file(&self, file: &QuantFile, status: &DiskStatus) -> ModelInfo {
        self.render(Some(file), status)
    }

    fn render(&self, file: Option<&QuantFile>, status: &DiskStatus) -> ModelInfo {
        let is_default = match (file, self.default_file()) {
            (Some(f), Some(d)) => f.filename == d.filename,
            _ => true,
        };
        let languages = canonicalize_supported_languages(self.languages.clone());
        ModelInfo {
            id: format!(
                "{}/{}",
                self.id,
                file.map(|f| f.filename.as_str()).unwrap_or_default()
            ),
            name: if is_default {
                self.name.clone()
            } else {
                format!(
                    "{} ({})",
                    self.name,
                    file.map(|f| f.quant.as_str()).unwrap_or("")
                )
            },
            description: self.description.clone(),
            filename: file.map(|f| f.filename.clone()).unwrap_or_default(),
            source: ModelSource::HuggingFace {
                repo_id: self.id.clone(),
                revision: self.revision.clone(),
            },
            size_mb: file.map(|f| f.size_bytes / (1024 * 1024)).unwrap_or(0),
            is_downloaded: status.is_downloaded,
            is_downloading: status.is_downloading,
            partial_size: status.partial_size,
            is_directory: false,
            engine_type: EngineType::LocalLlm,
            accuracy_score: 0.0,
            speed_score: 0.0,
            supports_translation: false,
            is_recommended: false,
            supports_language_selection: false,
            supported_languages: languages,
            is_custom: false,
            supports_streaming: false,
            supports_language_detection: false,
        }
    }
}

/// The bundled LLM catalog, parsed once.
pub static LLM_CATALOG: Lazy<Vec<LlmCatalogModel>> = Lazy::new(|| {
    serde_json::from_str::<LlmCatalogRoot>(include_str!("llm_catalog.json"))
        .expect("bundled llm_catalog.json is valid JSON matching the LLM catalog schema")
        .models
});

/// The catalog entry whose registry id is `model_id`, if any.
pub fn find(model_id: &str) -> Option<&'static LlmCatalogModel> {
    LLM_CATALOG.iter().find(|m| m.registry_id() == model_id)
}

/// The catalog entry + specific `files[]` entry owning `filename`, matched
/// across every listed quant (not just the default) - the LLM sibling of
/// [`crate::catalog::file_in_catalog`], used by the alternate-quant delete
/// semantics.
pub fn file_in_llm_catalog(
    filename: &str,
    repo_id: Option<&str>,
) -> Option<(&'static LlmCatalogModel, &'static QuantFile)> {
    LLM_CATALOG.iter().find_map(|m| {
        if let Some(repo) = repo_id {
            if m.id != repo {
                return None;
            }
        }
        m.files
            .iter()
            .find(|f| f.filename == filename)
            .map(|f| (m, f))
    })
}

/// Context lengths probed from user-added GGUFs at add/discover time,
/// keyed by registry id. Process-local (re-probed on every launch by the
/// llm-cache scan); catalog entries never consult this.
static DYNAMIC_CONTEXT: Lazy<std::sync::RwLock<std::collections::HashMap<String, u32>>> =
    Lazy::new(|| std::sync::RwLock::new(std::collections::HashMap::new()));

/// Record a probed context length for a user-added model.
pub fn register_dynamic_context(model_id: &str, context_tokens: u32) {
    DYNAMIC_CONTEXT
        .write()
        .expect("LLM context registry lock poisoned")
        .insert(model_id.to_string(), context_tokens);
}

/// The context window to allocate for `model_id`: the catalog value for
/// catalog entries, a probed value for user-added models registered this
/// launch, else the protocol default (the pinned builtin included).
pub fn context_tokens_for(model_id: &str) -> u32 {
    if let Some(model) = find(model_id) {
        return model.context_tokens;
    }
    if let Some(ctx) = DYNAMIC_CONTEXT
        .read()
        .expect("LLM context registry lock poisoned")
        .get(model_id)
    {
        return *ctx;
    }
    DEFAULT_CONTEXT_TOKENS
}

/// The download trust anchor for `model_id`: the catalog sha256 for catalog
/// entries, `None` otherwise (the pinned builtin's constant lives in
/// [`crate::local_llm`]; user-added models have no pre-known hash).
pub fn expected_sha256_for(model_id: &str) -> Option<&'static str> {
    find(model_id).and_then(|m| m.default_file().and_then(|f| f.sha256.as_deref()))
}

/// The exact expected on-disk byte length for `model_id` from the catalog
/// (the parent-side cheap integrity check at load time).
pub fn expected_size_bytes_for(model_id: &str) -> Option<u64> {
    find(model_id).map(|m| m.default_file().map(|f| f.size_bytes).unwrap_or(0))
}

/// The display publisher for `model_id`, when it is a catalog entry.
pub fn publisher_for(model_id: &str) -> Option<&'static str> {
    find(model_id).map(|m| m.publisher.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_llm::{
        LOCAL_LLM_MODEL_ID, LOCAL_LLM_MODEL_REVISION, LOCAL_LLM_MODEL_SHA256,
        LOCAL_LLM_MODEL_SIZE_BYTES,
    };
    use crate::managers::model_capabilities::LLM_ARCHES;
    use std::collections::BTreeSet;

    /// Mirrors catalog_parses_and_is_nonempty: the bundled catalog must
    /// contain the curated seed set, not an empty or malformed file.
    #[test]
    fn llm_catalog_parses_and_is_nonempty() {
        assert!(
            LLM_CATALOG.len() >= 6,
            "the seed catalog should carry the six curated post-process models, got {}",
            LLM_CATALOG.len()
        );
    }

    /// Registry ids must be unique: two entries folding onto the same
    /// "{repo}/{default file}" id would silently overwrite each other at
    /// registration.
    #[test]
    fn registry_ids_are_unique() {
        let mut ids: Vec<String> = LLM_CATALOG.iter().map(|m| m.registry_id()).collect();
        ids.sort();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "LLM catalog registry ids must be unique");
    }

    /// Every file entry carries the full trust anchor: a 64-hex sha256 (the
    /// download verification and any future mirror fallback key off it), a
    /// positive exact byte size (progress totals + the load-time length
    /// check), and the repo revision it is pinned to is a 40-hex commit sha.
    #[test]
    fn every_file_is_fully_pinned() {
        for model in LLM_CATALOG.iter() {
            assert!(
                model.revision.len() == 40 && model.revision.chars().all(|c| c.is_ascii_hexdigit()),
                "{}: revision {:?} must be a 40-hex commit sha",
                model.id,
                model.revision
            );
            assert!(!model.files.is_empty(), "{}: no files", model.id);
            for file in &model.files {
                assert_eq!(
                    file.sha256.as_deref().map(str::len),
                    Some(64),
                    "{}/{}: sha256 must be present and 64-hex",
                    model.id,
                    file.filename
                );
                assert!(
                    file.sha256
                        .as_deref()
                        .is_some_and(|s| s.chars().all(|c| c.is_ascii_hexdigit())),
                    "{}/{}: sha256 must be hex",
                    model.id,
                    file.filename
                );
                assert!(
                    file.size_bytes > 0,
                    "{}/{}: size_bytes must be positive",
                    model.id,
                    file.filename
                );
            }
            // The default quant must resolve to a real file.
            assert!(
                model.default_file().is_some(),
                "{}: default_quant {:?} matches no file",
                model.id,
                model.default_quant
            );
        }
    }

    /// Every catalog architecture must pass the LLM allowlist: an entry the
    /// worker cannot load would download fine and then fail every swap.
    #[test]
    fn llm_catalog_architectures_pass_the_llm_allowlist() {
        for model in LLM_CATALOG.iter() {
            assert!(
                LLM_ARCHES.contains(&model.architecture.as_str()),
                "{}: architecture {:?} is not in LLM_ARCHES {:?}",
                model.id,
                model.architecture,
                LLM_ARCHES
            );
        }
        // And the allowlist refuses ASR-only archs (the voice path's table
        // and the LLM table stay disjoint).
        let disjoint: BTreeSet<&str> = LLM_ARCHES
            .iter()
            .copied()
            .filter(|a| crate::managers::model_capabilities::KNOWN_ARCHES.contains(a))
            .collect();
        // granite appears in both: transcribe-cpp has a granite ASR family
        // AND llama.cpp loads granite LLMs. It is the one deliberate overlap;
        // engine routing is by EngineType, never by architecture alone.
        assert!(
            disjoint.iter().all(|a| *a == "granite"),
            "only granite may appear in both allowlists, got {:?}",
            disjoint
        );
    }

    /// The pinned model keeps its identity: the catalog's Qwen3-0.6B entry
    /// must reproduce the pinned constants exactly (same repo/file id, same
    /// revision, same sha256, same byte size), so registering the catalog can
    /// never drift the built-in entry the settings default points at.
    #[test]
    fn qwen3_06b_entry_reproduces_the_pinned_constants() {
        let model = find(LOCAL_LLM_MODEL_ID)
            .expect("the catalog must contain the pinned Qwen3-0.6B-Q8_0 entry");
        assert_eq!(model.id, "Qwen/Qwen3-0.6B-GGUF");
        assert_eq!(model.revision, LOCAL_LLM_MODEL_REVISION);
        let file = model
            .default_file()
            .expect("the pinned entry has a default file");
        assert_eq!(file.filename, "Qwen3-0.6B-Q8_0.gguf");
        assert_eq!(file.sha256.as_deref(), Some(LOCAL_LLM_MODEL_SHA256));
        assert_eq!(file.size_bytes, LOCAL_LLM_MODEL_SIZE_BYTES);
        // Rendered shape: the same id the pinned builtin registers under, so
        // registration dedups instead of duplicating.
        let info = model.to_model_info(&DiskStatus::default());
        assert_eq!(info.id, LOCAL_LLM_MODEL_ID);
        assert!(matches!(info.engine_type, EngineType::LocalLlm));
    }

    /// Context tokens: every entry declares a sane value and the per-model
    /// lookup falls back to the protocol default for anything not in the
    /// catalog (the pinned builtin, user-added models).
    #[test]
    fn context_tokens_are_sane_and_default_falls_back() {
        for model in LLM_CATALOG.iter() {
            assert!(
                model.context_tokens >= 1024 && model.context_tokens <= 32 * 1024,
                "{}: context_tokens {} outside the 1k-32k operating band",
                model.id,
                model.context_tokens
            );
            assert_eq!(
                context_tokens_for(&model.registry_id()),
                model.context_tokens
            );
        }
        assert_eq!(
            context_tokens_for(LOCAL_LLM_MODEL_ID),
            DEFAULT_CONTEXT_TOKENS,
            "the pinned builtin allocates the protocol default"
        );
        assert_eq!(
            context_tokens_for("org/unknown-llm/model.gguf"),
            DEFAULT_CONTEXT_TOKENS
        );
    }

    /// The trust-anchor lookups resolve per model id: catalog entries carry a
    /// sha + exact size, unknown ids carry neither, and the pinned id's
    /// catalog sha agrees with the pinned constant (the command layer still
    /// prefers the constant for the builtin; both describe the same bytes).
    #[test]
    fn trust_anchor_lookups_resolve_by_id() {
        let id = "unsloth/Qwen3-1.7B-GGUF/Qwen3-1.7B-Q4_K_M.gguf";
        let sha = expected_sha256_for(id).expect("catalog entry carries a sha");
        assert_eq!(sha.len(), 64);
        assert_eq!(
            expected_size_bytes_for(id),
            Some(1_107_409_472),
            "the exact pinned byte size flows through"
        );
        assert!(publisher_for(id).is_some());
        assert_eq!(
            expected_sha256_for(LOCAL_LLM_MODEL_ID),
            Some(LOCAL_LLM_MODEL_SHA256),
            "the pinned id's catalog entry and the pinned constant describe the same bytes"
        );
        assert_eq!(expected_size_bytes_for("org/none/x.gguf"), None);
    }
}
