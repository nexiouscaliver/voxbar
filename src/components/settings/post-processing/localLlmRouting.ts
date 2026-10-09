// Pure provider-routing decisions for the post-process settings panel
// (plan Phase B, T30). Extracted so the provider-state hook, the settings
// component, and the colocated test share one source of truth:
// - the local on-device provider must never trigger a fetch_models call
//   (its models_endpoint is None; the backend has no list endpoint for it)
// - only the local provider shows the local model row instead of the API
//   fields and the model dropdown.

export const APPLE_PROVIDER_ID = "apple_intelligence";
export const LOCAL_PROVIDER_ID = "local";

// The pinned post-process model registry id. Must stay in sync with
// LOCAL_LLM_MODEL_ID in src-tauri/src/local_llm/mod.rs; used to filter
// the shared model-download-progress events.
export const LOCAL_LLM_MODEL_ID = "Qwen/Qwen3-0.6B-GGUF/Qwen3-0.6B-Q8_0.gguf";

// Display name matching the backend ModelInfo descriptor (managers/model.rs)
// so the settings row and the model manager agree on what the user sees.
export const LOCAL_LLM_MODEL_NAME = "Qwen3 0.6B (post-process)";

// Pinned size in MB (LOCAL_LLM_MODEL_SIZE_MB in local_llm/mod.rs); the
// row's fallback while the first status snapshot is still in flight.
export const LOCAL_LLM_MODEL_SIZE_MB = 610;

// Stable prefix of the transient delete-refusal error string
// (SWAP_REFUSAL_PREFIX in src-tauri/src/local_llm/mod.rs). Must stay in
// sync: the row matches it to tell the "a post-process run is in
// progress, try again" refusal apart from real delete failures, which
// carry a different message.
export const SWAP_REFUSAL_PREFIX = "post-process-swap-in-progress";

// Whether a delete-command error is the transient swap refusal (L3) the
// UI words as "try again in a moment", rather than a real failure (disk
// error, missing file) that must show its actual cause.
export function isSwapRefusalError(error: string): boolean {
  return error.startsWith(SWAP_REFUSAL_PREFIX);
}

// Providers whose model list can ever be fetched from a remote endpoint.
// Local and Apple Intelligence are on-device engines with no models
// endpoint; API providers are unchanged.
export function shouldFetchModels(providerId: string): boolean {
  return providerId !== LOCAL_PROVIDER_ID && providerId !== APPLE_PROVIDER_ID;
}

// Whether the provider's settings surface is the local model row.
export function showLocalRow(providerId: string): boolean {
  return providerId === LOCAL_PROVIDER_ID;
}
