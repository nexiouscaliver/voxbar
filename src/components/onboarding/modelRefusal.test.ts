import assert from "node:assert/strict";
import type { ModelStateEvent } from "../../lib/types/events";
import {
  formatMarginMb,
  formatMemoryAmount,
  parseRefusal,
  refusalActions,
} from "./modelRefusal";

// parseRefusal accepts only loading_failed events with the structured
// memory_gate payload; every other loading failure is not a gate refusal.
assert.equal(
  parseRefusal({ event_type: "loading_started" } as ModelStateEvent),
  null,
);
assert.equal(
  parseRefusal({
    event_type: "loading_failed",
    error: "Model not downloaded",
  } as ModelStateEvent),
  null,
);
const refusal = parseRefusal({
  event_type: "loading_failed",
  model_id: "whisper-tiny-q8",
  model_name: "Whisper Tiny",
  error: "Not enough free memory for Whisper Tiny: ...",
  memory_gate: {
    forecast_bytes: 45088768,
    free_bytes: 1087373312,
    headroom_bytes: 0,
  },
});
assert.ok(refusal, "a structured refusal must parse");
assert.equal(refusal.forecastBytes, 45088768);
assert.equal(refusal.freeBytes, 1087373312);
assert.equal(refusal.headroomBytes, 0);

// Actions: retry and switch-model always; disable-guard only while the
// guard is on; continue-deferred always.
assert.deepEqual(refusalActions(refusal, true), [
  "retry",
  "switch-model",
  "disable-guard",
  "continue-deferred",
]);
assert.deepEqual(refusalActions(refusal, false), [
  "retry",
  "switch-model",
  "continue-deferred",
]);

// The formatters mirror the backend's refusal-message rule exactly.
assert.equal(formatMemoryAmount(45088768), "43 MB");
assert.equal(formatMemoryAmount(766509056), "731 MB");
assert.equal(formatMemoryAmount(1610612736), "1.5 GB");
assert.equal(formatMarginMb(1610612736), "1536 MB");
// min 1 mirrors the backend rule (the margin clause only renders when the
// margin is non-zero, so this floor never shows in practice).
assert.equal(formatMarginMb(0), "1 MB");

console.log("modelRefusal: all assertions passed");
