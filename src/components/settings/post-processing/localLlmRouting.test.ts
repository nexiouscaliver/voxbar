import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import {
  shouldFetchModels,
  showLocalRow,
  isSwapRefusalError,
  LOCAL_PROVIDER_ID,
  SWAP_REFUSAL_PREFIX,
} from "./localLlmRouting";

// T30: the local on-device provider must never trigger a fetch_models
// call (its models_endpoint is None), and only it shows the local model
// row. API providers are unchanged on both counts.
assert.equal(
  shouldFetchModels(LOCAL_PROVIDER_ID),
  false,
  "local must never fetch models",
);
assert.equal(
  shouldFetchModels("apple_intelligence"),
  false,
  "apple_intelligence keeps its existing no-fetch behavior",
);
assert.equal(showLocalRow(LOCAL_PROVIDER_ID), true, "local shows the row");
assert.equal(showLocalRow("apple_intelligence"), false);
assert.equal(showLocalRow("openai"), false);

// Every API provider keeps fetching (the off path is unchanged).
const apiProviders = [
  "openai",
  "anthropic",
  "google",
  "mistral",
  "groq",
  "deepseek",
  "xai",
  "custom",
];
for (const providerId of apiProviders) {
  assert.equal(
    shouldFetchModels(providerId),
    true,
    `${providerId} must keep fetching models`,
  );
  assert.equal(
    showLocalRow(providerId),
    false,
    `${providerId} must not show the local row`,
  );
}

// Unknown provider ids default to the API path (fetch allowed), matching
// the hook's pre-existing behavior for ids it does not special-case.
assert.equal(shouldFetchModels("some_future_provider"), true);
assert.equal(showLocalRow("some_future_provider"), false);

// The delete-error classification: only the transient swap refusal (L3)
// gets the "try again in a moment" copy; every other error (disk error,
// missing file) is a real failure and must show its actual cause instead
// of a false "a run is in progress" claim.
assert.equal(
  isSwapRefusalError(
    "post-process-swap-in-progress: post-processing is in progress, try again in a moment",
  ),
  true,
  "the backend refusal string must classify as the transient refusal",
);
assert.equal(
  isSwapRefusalError("failed to delete model file: No such file or directory"),
  false,
  "an IO error must NOT classify as the transient refusal",
);
assert.equal(
  isSwapRefusalError("post-processing is in progress, try again in a moment"),
  false,
  "the refusal sentence WITHOUT the stable prefix is treated as a real failure",
);

// The prefix is a cross-language contract: the frontend classifier breaks
// silently if the backend constant drifts, so pin both copies together.
const backendMod = fs.readFileSync(
  path.join(
    import.meta.dirname,
    "..",
    "..",
    "..",
    "..",
    "src-tauri",
    "src",
    "local_llm",
    "mod.rs",
  ),
  "utf8",
);
const backendMatch = backendMod.match(
  /pub const SWAP_REFUSAL_PREFIX: &str = "([^"]+)";/,
);
assert.ok(backendMatch, "SWAP_REFUSAL_PREFIX must exist in the backend");
assert.equal(
  backendMatch[1],
  SWAP_REFUSAL_PREFIX,
  "frontend and backend SWAP_REFUSAL_PREFIX must stay in sync",
);

console.log("localLlmRouting: all assertions passed");
