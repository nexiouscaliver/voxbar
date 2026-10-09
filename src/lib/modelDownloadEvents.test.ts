import assert from "node:assert/strict";
import {
  downloadFailedModelName,
  type ModelDownloadFailedEvent,
} from "./modelDownloadEvents";

// The event's display name must win: the local post-process model is
// filtered out of the store's model list (it is not an ASR model), so the
// raw registry id is all the store could offer without it.
const localModelEvent: ModelDownloadFailedEvent = {
  model_id: "Qwen/Qwen3-0.6B-GGUF/Qwen3-0.6B-Q8_0.gguf",
  error: "network dropped mid-download",
  name: "Qwen3 0.6B (post-process)",
};
assert.equal(
  downloadFailedModelName(localModelEvent, []),
  "Qwen3 0.6B (post-process)",
  "the event name must win when the store cannot resolve the model",
);

// Voice-model events carry no name: fall back to the registry name.
const registryModels = [
  { id: "whisper-large-v3-turbo", name: "Whisper Large v3 Turbo" },
];
assert.equal(
  downloadFailedModelName(
    { model_id: "whisper-large-v3-turbo", error: "x" },
    registryModels,
  ),
  "Whisper Large v3 Turbo",
  "events without a name keep resolving through the store",
);

// Unknown id and no name: the raw id remains the last resort.
assert.equal(
  downloadFailedModelName(
    { model_id: "custom/model", error: "x" },
    registryModels,
  ),
  "custom/model",
);

console.log("modelDownloadEvents tests passed");
