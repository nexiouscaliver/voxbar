import assert from "node:assert/strict";
import {
  shouldFetchModels,
  showLocalRow,
  LOCAL_PROVIDER_ID,
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

console.log("localLlmRouting: all assertions passed");
