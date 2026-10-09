//! The local on-device post-process engine.
//!
//! Post-processing (cleaning up a finished transcript) runs on a small local
//! LLM instead of a remote OpenAI-compatible API. The LLM lives in a
//! dedicated child worker process (the same executable relaunched with
//! `--llm-worker`, the engine_supervisor precedent), so a native crash or
//! hang can never take the app down and process exit is the only unload the
//! OS guarantees returns every page.
//!
//! The core invariant (spec L2): the voice model and the local LLM are NEVER
//! in RAM together. Every post-process runs as an exclusive swap: unload
//! voice (waited), load LLM, generate, unload LLM (waited), restore voice.
//! The pure half of that machine lives in [`planner`]; the executor in
//! `manager` (a later commit); the worker mainloop in `worker`.
//!
//! - [`protocol`]: line-oriented request/response frames for the worker.
//! - [`forecast`]: CJK-aware token/unit budgets and the memory forecast.
//! - [`planner`]: the pure swap state machine (states, signals, actions).

pub mod forecast;
pub mod manager;
pub mod planner;
pub mod protocol;
pub mod worker;

/// The one and only v1 post-process model: Qwen3-0.6B Q8_0 (Apache-2.0,
/// 609.8 MB, fits the operator's sub-800 MB budget). The id doubles as the
/// ModelManager registry id and encodes the HF repo path + file so the
/// built-in descriptor and the settings default can never drift apart.
pub const LOCAL_LLM_MODEL_ID: &str = "Qwen/Qwen3-0.6B-GGUF/Qwen3-0.6B-Q8_0.gguf";

/// Pinned content hash, verified ONCE at download completion (the download
/// command wrapper), never re-hashed per use: a 610 MB hash on every
/// dictation would add seconds.
pub const LOCAL_LLM_MODEL_SHA256: &str =
    "9465e63a22add5354d9bb4b99e90117043c7124007664907259bd16d043bb031";

/// Registry display size (609.8 MB rounds to 610). Used for the memory-gate
/// file-size forecast base and the settings row.
pub const LOCAL_LLM_MODEL_SIZE_MB: u64 = 610;

/// Pinned HF revision so the bytes behind the sha256 can never move.
pub const LOCAL_LLM_MODEL_REVISION: &str = "23749fefcc72300e3a2ad315e1317431b06b590a";

/// Exact on-disk byte length of the pinned Q8_0 file (HF tree API). The
/// worker's Load does a cheap length check against this (the full sha256
/// is verified once, at download completion).
pub const LOCAL_LLM_MODEL_SIZE_BYTES: u64 = 639_446_688;

/// Display name for the pinned model. Matches the ModelInfo descriptor
/// (managers/model.rs) and LOCAL_LLM_MODEL_NAME in the frontend routing
/// module. Sent with `model-download-failed` events because this model is
/// filtered out of the frontend store's model list: without it the shared
/// toast falls back to the raw registry id.
pub const LOCAL_LLM_MODEL_NAME: &str = "Qwen3 0.6B (post-process)";

/// Why a post-process pass fell back to the raw transcript. Carried on the
/// skip event so the frontend toast can name the cause; `memory_gate`
/// includes the formatted refusal numbers in the event detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    MemoryGate,
    DownloadMissing,
    EngineFailed,
    Timeout,
    LengthGuard,
    TooLong,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pinned constants are load-bearing: the sha256 is the download
    /// trust anchor, the revision pins the bytes it hashes, and the size
    /// feeds the memory gate. A accidental edit here must fail a test, not
    /// a user's download.
    #[test]
    fn pinned_model_constants_are_stable() {
        assert_eq!(
            LOCAL_LLM_MODEL_ID,
            "Qwen/Qwen3-0.6B-GGUF/Qwen3-0.6B-Q8_0.gguf"
        );
        assert_eq!(
            LOCAL_LLM_MODEL_SHA256,
            "9465e63a22add5354d9bb4b99e90117043c7124007664907259bd16d043bb031"
        );
        assert_eq!(LOCAL_LLM_MODEL_SIZE_MB, 610);
        assert_eq!(
            LOCAL_LLM_MODEL_REVISION,
            "23749fefcc72300e3a2ad315e1317431b06b590a"
        );
        assert_eq!(
            LOCAL_LLM_MODEL_NAME,
            crate::managers::model::local_llm_model_info().name
        );
    }
}
