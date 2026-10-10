import assert from "node:assert/strict";
import { mapCommandResult } from "./settingsWriteOutcome";

// A backend error result fails, rolls the optimistic write back, and carries
// the backend's reason.
assert.deepEqual(
  mapCommandResult(
    { status: "error", error: "updater disabled" },
    "update_checks_enabled",
  ),
  {
    ok: false,
    rollback: true,
    error: "updater disabled",
  },
);

// An error result without a reason still fails with a key-named fallback so
// the toast is never empty.
const noReason = mapCommandResult({ status: "error" }, "vad_backend");
assert.equal(noReason.ok, false);
assert.equal(noReason.rollback, true);
assert.equal(
  typeof noReason.error,
  "string",
  "fallback error must be a string",
);
assert.ok(
  noReason.error!.includes("vad_backend"),
  "fallback error names the setting key",
);

// Ok results pass through untouched.
assert.deepEqual(mapCommandResult({ status: "ok" }, "theme"), {
  ok: true,
  rollback: false,
  error: undefined,
});
assert.deepEqual(
  mapCommandResult({ status: "ok", data: { anything: true } }, "theme"),
  { ok: true, rollback: false, error: undefined },
);

// Non-Result shapes (commands that return raw values, or nothing at all)
// are treated as success: only an explicit error status is a failure.
assert.deepEqual(mapCommandResult(null, "theme"), {
  ok: true,
  rollback: false,
  error: undefined,
});
assert.deepEqual(mapCommandResult(undefined, "theme"), {
  ok: true,
  rollback: false,
  error: undefined,
});
assert.deepEqual(mapCommandResult(true, "theme"), {
  ok: true,
  rollback: false,
  error: undefined,
});

console.log("settingsWriteOutcome: all assertions passed");
