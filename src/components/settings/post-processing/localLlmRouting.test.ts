import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import {
  shouldFetchModels,
  showLocalRow,
  isSwapRefusalError,
  isNotAPostProcessModelError,
  showPinnedOnlyRow,
  splitLlmModels,
  LOCAL_PROVIDER_ID,
  LOCAL_LLM_MODEL_ID,
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

// The selection-refusal classification: only the stable prefix marks the
// "this is a transcription model, not a post-process model" refusal; a
// real error (not downloaded) must show its own message.
assert.equal(
  isNotAPostProcessModelError(
    "not-a-post-process-model: small powers transcription, not post-processing",
  ),
  true,
);
assert.equal(
  isNotAPostProcessModelError("Model not downloaded: org/repo/m.gguf"),
  false,
);

// Card helper: model shape for the pure predicates.
const card = (id: string, is_downloaded: boolean, selected = false) => ({
  info: { id, is_downloaded },
  selected,
});

// The off path: until anything beyond the pinned model is downloaded (or
// selected), the section must keep rendering the original pinned row.
assert.equal(
  showPinnedOnlyRow([], LOCAL_LLM_MODEL_ID),
  true,
  "no entries yet still reads as the pinned-only off path",
);
assert.equal(
  showPinnedOnlyRow([card(LOCAL_LLM_MODEL_ID, false)], LOCAL_LLM_MODEL_ID),
  true,
  "only the pinned entry (not even downloaded) is the off path",
);
assert.equal(
  showPinnedOnlyRow([card(LOCAL_LLM_MODEL_ID, true)], LOCAL_LLM_MODEL_ID),
  true,
  "the downloaded pinned model alone is still the off path",
);
assert.equal(
  showPinnedOnlyRow(
    [card(LOCAL_LLM_MODEL_ID, true), card("org/other/m.gguf", false)],
    LOCAL_LLM_MODEL_ID,
  ),
  true,
  "a second catalog entry that is NOT downloaded keeps the off path",
);
assert.equal(
  showPinnedOnlyRow(
    [card(LOCAL_LLM_MODEL_ID, true), card("org/other/m.gguf", true)],
    LOCAL_LLM_MODEL_ID,
  ),
  false,
  "a downloaded second model switches to the full section",
);
assert.equal(
  showPinnedOnlyRow([card(LOCAL_LLM_MODEL_ID, true)], "org/other/m.gguf"),
  false,
  "a moved selection switches to the full section",
);

// The catalog entry point: the browse affordance under the pinned row must
// open the grid even in the exact states the off path covers (fresh
// install, pinned downloaded). Without the reveal flag the section is a
// closed loop - downloading a second model or moving the selection is only
// possible inside the suppressed grid.
assert.equal(
  showPinnedOnlyRow([], LOCAL_LLM_MODEL_ID, true),
  false,
  "a revealed catalog renders the grid even before any entry loads",
);
assert.equal(
  showPinnedOnlyRow(
    [card(LOCAL_LLM_MODEL_ID, false)],
    LOCAL_LLM_MODEL_ID,
    true,
  ),
  false,
  "a revealed catalog renders the grid on a fresh install",
);
assert.equal(
  showPinnedOnlyRow([card(LOCAL_LLM_MODEL_ID, true)], LOCAL_LLM_MODEL_ID, true),
  false,
  "a revealed catalog renders the grid with only the pinned model downloaded",
);
assert.equal(
  showPinnedOnlyRow(
    [card(LOCAL_LLM_MODEL_ID, true)],
    LOCAL_LLM_MODEL_ID,
    false,
  ),
  true,
  "the unrevealed state keeps the pinned row (the off path is unchanged)",
);

// The section split: downloaded (plus the selected entry) vs the rest.
const split = splitLlmModels([
  card("a", false),
  card("b", true),
  card("c", false, true), // selected but not downloaded must not happen in
  // practice (selection requires downloaded), but the split keeps it
  // visible rather than silently dropping it.
  card("d", false),
]);
assert.deepEqual(
  split.downloaded.map((m) => m.info.id),
  ["b", "c"],
);
assert.deepEqual(
  split.available.map((m) => m.info.id),
  ["a", "d"],
);

console.log("localLlmRouting: all assertions passed");
