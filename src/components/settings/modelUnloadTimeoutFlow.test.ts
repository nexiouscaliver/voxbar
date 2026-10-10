import assert from "node:assert/strict";
import {
  resolveUnloadTimeoutOutcome,
  resolveUnloadTimeoutFocusGate,
} from "./modelUnloadTimeoutFlow";

// AUD-06: the "Unload After → Custom…" flow decides what to do after
// commands.setModelUnloadTimeoutCustomSeconds() resolves. The Tauri binding
// resolves backend failures as Result values ({status:"error", error}) rather
// than throwing (src/bindings.ts:1289-1295), so today ModelUnloadTimeout.tsx
// (:65 focus path, :132 numeric field) awaits the command, ignores an error
// status, and optimistically updateSetting()s the custom seconds anyway.
// model_unload_timeout has no settingUpdaters entry (settingsStore.ts:103-253),
// so updateSetting's own mapCommandResult(null) short-circuit returns ok
// (settingsStore.ts:396, settingsWriteOutcome.ts:39-40) and the store keeps a
// custom 90s the backend rejected - contradicting the fail-closed standard at
// settingsStore.ts:405-420. resolveUnloadTimeoutOutcome is the pure decision
// extracted from that flow: an error result must keep the previous stored
// value and carry a surfaceable reason; only an ok result may apply the
// attempted seconds.

// 1. Backend rejected the switch: the store must keep the previous preset,
//    never the rejected custom 90, and the backend's reason must survive so
//    the standard failure path (console.error + toast) can show it.
assert.deepEqual(
  resolveUnloadTimeoutOutcome(
    { status: "error", error: "model is loading" },
    90,
    "min_5",
  ),
  { apply: false, keep: "min_5", error: "model is loading" },
  "error result keeps the previous value and surfaces the backend reason",
);

// 2. A failed re-switch from an existing custom value keeps that custom
//    value (the numeric-field path at :132 refines an already-custom value).
assert.deepEqual(
  resolveUnloadTimeoutOutcome(
    { status: "error", error: "unload busy" },
    300,
    { custom: { seconds: 300 } },
  ),
  { apply: false, keep: { custom: { seconds: 300 } }, error: "unload busy" },
  "error result keeps the previous custom seconds",
);

// 3. An error result without a reason still fails closed with a non-empty
//    fallback naming the setting - same convention as mapCommandResult's
//    fallback (settingsWriteOutcome.ts:31-36) so the toast is never empty.
const noReason = resolveUnloadTimeoutOutcome({ status: "error" }, 90, "min_5");
assert.equal(noReason.apply, false);
assert.equal(noReason.keep, "min_5");
assert.ok(
  typeof noReason.error === "string" && noReason.error.length > 0,
  "fallback error must be a non-empty string",
);
assert.ok(
  noReason.error.includes("model_unload_timeout"),
  "fallback error names the setting key",
);

// 4. Ok result: apply the attempted seconds (the tray focus path seeds 90,
//    KB-185's switch-before-focus behavior preserved).
assert.deepEqual(
  resolveUnloadTimeoutOutcome({ status: "ok", data: null }, 90, "min_5"),
  { apply: true, value: { custom: { seconds: 90 } } },
  "ok result applies the attempted custom seconds",
);

// 5. The attempted seconds flow through - the numeric field commits values
//    other than the 90s seed.
assert.deepEqual(
  resolveUnloadTimeoutOutcome({ status: "ok", data: null }, 300, {
    custom: { seconds: 90 },
  }),
  { apply: true, value: { custom: { seconds: 300 } } },
  "ok result applies the attempted seconds, not a hard-coded 90",
);

// 6. Non-Result shapes (a command that returned nothing) stay successes,
//    mirroring mapCommandResult's tolerance (settingsWriteOutcome.ts:46-50):
//    only an explicit error status is a failure.
assert.deepEqual(
  resolveUnloadTimeoutOutcome(null, 90, "min_5"),
  { apply: true, value: { custom: { seconds: 90 } } },
  "an absent result is not an error",
);

// AUD-06 (cold-window hydration race): the tray focus event can arrive while
// the settings store is still null (refreshSettings in initialize() has not
// landed). Today the focus handler treats that exactly like "a preset is
// stored" and relies on updateSetting, whose optimistic set keeps settings
// null (settingsStore.ts:391-392) - so the field never renders, the ref is
// null, and the focus silently does nothing even though the backend committed.
// resolveUnloadTimeoutFocusGate separates the three states the handler must
// distinguish: defer until the store hydrates (then re-evaluate, keeping the
// focus behavior), switch a stored preset to custom, or just focus an
// already-custom value.

// 7. Not hydrated (getSetting returns undefined because settings is null):
//    defer - never optimistic-flip on an unhydrated store.
assert.deepEqual(
  resolveUnloadTimeoutFocusGate(undefined, false),
  { action: "defer" },
  "an unhydrated store defers the switch, it is not a preset",
);

// 8. Hydrated with a preset stored: switch (KB-185 preserved).
assert.deepEqual(
  resolveUnloadTimeoutFocusGate("min_5", true),
  { action: "switch" },
);

// 9. Hydrated and already custom: focus only - no redundant backend write.
assert.deepEqual(
  resolveUnloadTimeoutFocusGate({ custom: { seconds: 120 } }, true),
  { action: "focus" },
);

console.log("modelUnloadTimeoutFlow: all assertions passed");
