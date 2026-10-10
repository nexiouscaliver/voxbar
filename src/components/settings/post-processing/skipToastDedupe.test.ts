import assert from "node:assert/strict";
import { shouldToast, skipToastKey } from "./skipToastDedupe";
import type { PostProcessFailureClass, SkipReason } from "@/bindings";
import { failureClassToastKey } from "./skipToastDedupe";

// T31: each skip reason toasts at most once per app session; a second
// event with the same reason is suppressed, a different reason still
// toasts.
const seen = new Set<SkipReason>();
assert.equal(
  shouldToast(seen, "memory_gate"),
  true,
  "first memory_gate toasts",
);
assert.equal(
  shouldToast(seen, "memory_gate"),
  false,
  "second memory_gate is deduped",
);
assert.equal(
  shouldToast(seen, "engine_failed"),
  true,
  "a different reason still toasts",
);
assert.equal(shouldToast(seen, "engine_failed"), false);
assert.equal(shouldToast(seen, "timeout"), true);
assert.equal(shouldToast(seen, "timeout"), false);
assert.equal(seen.size, 3, "the set records exactly the toasted reasons");

// Every wire reason maps onto the i18n key segment under
// toast.postProcessSkip, and the mapping is injective.
const reasons: SkipReason[] = [
  "memory_gate",
  "download_missing",
  "engine_failed",
  "timeout",
  "length_guard",
  "too_long",
];
const keys = reasons.map(skipToastKey);
assert.deepEqual(keys, [
  "memoryGate",
  "downloadMissing",
  "engineFailed",
  "timeout",
  "lengthGuard",
  "tooLong",
]);
assert.equal(new Set(keys).size, keys.length, "keys must be unique");

console.log("skipToastDedupe: all assertions passed");

// The pp: lifecycle's failure-class toasts (WS3): the same once-per-token
// dedupe, and every wire class maps onto the toast.postProcessFailure key
// segment injectively.
const toastedFailures = new Set<PostProcessFailureClass>();
assert.equal(shouldToast(toastedFailures, "auth"), true);
assert.equal(shouldToast(toastedFailures, "auth"), false);
assert.equal(shouldToast(toastedFailures, "network"), true);

const classes: PostProcessFailureClass[] = [
  "auth",
  "network",
  "timeout",
  "context_length",
  "output_invalid",
  "oom",
  "cancelled",
];
const failureKeys = classes.map(failureClassToastKey);
assert.deepEqual(failureKeys, [
  "auth",
  "network",
  "timeout",
  "contextLength",
  "outputInvalid",
  "oom",
  "cancelled",
]);
assert.equal(new Set(failureKeys).size, failureKeys.length);

console.log("failureClassToastKey: all assertions passed");
