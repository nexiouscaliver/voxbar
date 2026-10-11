import assert from "node:assert/strict";

// The repo tsconfig sets types: ["node"] (no bun-types), so tsc cannot
// resolve "bun:test"; the expect-error suppresses exactly that, and the
// cast keeps mock's own callsite typed. At runtime bun provides the real
// mock.module.
// @ts-expect-error tsc lacks bun-types; the runtime is bun itself
import { mock as bunMockModule } from "bun:test";
const mock = bunMockModule as unknown as {
  module: (moduleName: string, factory: () => unknown) => void;
};

// W1-L4 store-level tests: the REAL settingsStore, exercised through the
// same `bun <file>` invocation scripts/run-frontend-tests.ts uses for every
// repo test. The store chain cannot be imported UNMOCKED under bun (it
// pulls src/i18n, whose import.meta.glob is a Vite-only transform), so the
// three Tauri-coupled edges are mocked with bun's mock.module BEFORE the
// dynamic import below: @/bindings (commands), ../i18n (toast strings),
// sonner (toast delivery). Everything else - zustand, the store reducer
// itself, settingsWriteOutcome, the hydration queue, the numeric clamps -
// is the real module under test. The queue's pure semantics are separately
// pinned in settingsHydrationQueue.test.ts; this file pins the WIRING:
// the guard at the top of updateSetting, the replay drain inside
// refreshSettings, the failure-path warn, and the setter clamps.

interface RecordedCall {
  method: string;
  args: unknown[];
}

const calls: RecordedCall[] = [];
const toasts: string[] = [];

// The settings getAppSettings hands back; each block flips it to stage the
// hydration state it needs.
let appSettingsResult:
  | { status: "ok"; data: Record<string, unknown> }
  | { status: "error"; error: string } = {
  status: "error",
  error: "unhydrated",
};

// Optional one-shot error injection: the next call to this method resolves
// with an error result, then the entry is consumed.
const failNext: Record<string, string> = {};

const BASE_SETTINGS: Record<string, unknown> = {
  theme: "light",
  app_language: "en",
  hold_threshold_ms: 300,
  paste_delay_ms: 60,
  memory_gate_headroom_mb: 512,
  post_process_model_lists: {},
};

mock.module("@/bindings", () => ({
  commands: new Proxy(
    {},
    {
      get: (_, method: string) => {
        return async (...args: unknown[]) => {
          calls.push({ method, args });
          if (method === "getAppSettings") {
            return appSettingsResult;
          }
          const failWith = failNext[method];
          if (failWith !== undefined) {
            delete failNext[method];
            return { status: "error", error: failWith };
          }
          return { status: "ok", data: null };
        };
      },
    },
  ),
}));

mock.module("../i18n", () => ({
  default: { t: (key: string) => key },
}));

mock.module("sonner", () => ({
  toast: {
    error: (message: string) => {
      toasts.push(message);
    },
  },
}));

const countCalls = (method: string) =>
  calls.filter((call) => call.method === method).length;

const { useSettingsStore } = await import("./settingsStore");
const { SETTING_NUMERIC_CLAMPS, clampSettingValue } = await import(
  "./settingsStore"
);

const store = () => useSettingsStore.getState();

// --- Brief arm 1: a pre-hydration updateSetting does nothing observable --

{
  assert.equal(store().settings, null, "a fresh store starts unhydrated");
  assert.equal(store().isLoading, true, "a fresh store is still loading");

  await store().updateSetting("theme", "dark");

  assert.equal(
    store().settings,
    null,
    "a pre-hydration updateSetting must leave settings null (no optimistic write)",
  );
  // NOTE: assert.deepEqual's `asserts actual is T` signature would narrow
  // `calls` to never[] for every later block, so the empty check is a
  // length equality instead.
  assert.equal(
    calls.length,
    0,
    "a pre-hydration updateSetting must issue ZERO backend commands",
  );
  assert.equal(
    store().isUpdatingKey("theme"),
    false,
    "a pre-hydration updateSetting must not flip the isUpdating spinner",
  );
  assert.equal(toasts.length, 0, "a stashed intent is not an error, no toast");
}

// --- Clamp reaches the stash: the replayed value is the clamped one -------

{
  await store().updateSetting("hold_threshold_ms", 9999);
  assert.equal(
    countCalls("changeHoldThresholdMsSetting"),
    0,
    "a second pre-hydration intent still issues no backend command",
  );
}

// --- Failed hydration is loud, and retains the queue for later replay ----

{
  appSettingsResult = { status: "error", error: "settings store unreadable" };

  const warns: unknown[][] = [];
  const originalWarn = console.warn;
  console.warn = (...args: unknown[]) => {
    warns.push(args);
  };
  try {
    await store().refreshSettings();
  } finally {
    console.warn = originalWarn;
  }

  assert.equal(
    store().settings,
    null,
    "a failed hydration must not populate settings",
  );
  assert.equal(countCalls("getAppSettings"), 1, "one hydration attempt");
  assert.equal(
    countCalls("changeThemeSetting"),
    0,
    "a failed hydration must not replay the queued intents",
  );
  assert.equal(
    warns.length,
    1,
    "a failed hydration with queued intents warns exactly once",
  );
  assert.ok(
    warns[0]!.some(
      (part) =>
        typeof part === "string" &&
        part.includes("pre-hydration update intent"),
    ),
    "the warn names the queued pre-hydration intents",
  );

  // Retained, not dropped: the next SUCCESSFUL hydration replays them.
  appSettingsResult = { status: "ok", data: { ...BASE_SETTINGS } };
  await store().refreshSettings();
  assert.notEqual(store().settings, null, "hydration now populated settings");
  assert.equal(
    countCalls("changeThemeSetting"),
    1,
    "the retained theme replayed",
  );
}

// --- Brief arm 2: the pending values apply on hydration --------------------

{
  const settings = store().settings!;
  assert.equal(
    settings.theme,
    "dark",
    "the stashed theme applied on hydration (replay's optimistic write)",
  );
  assert.equal(
    settings.hold_threshold_ms,
    1000,
    "the stashed 9999 replayed as its clamped value 1000",
  );
  assert.deepEqual(
    calls.find((call: RecordedCall) => call.method === "changeThemeSetting")!
      .args,
    ["dark"],
    "the replay drove the real backend updater with the stashed value",
  );
  assert.deepEqual(
    calls.find(
      (call: RecordedCall) => call.method === "changeHoldThresholdMsSetting",
    )!.args,
    [1000],
    "the replay drove the backend updater with the CLAMPED value",
  );

  // Replay is one-shot: a second refresh (the settings-changed listener
  // fires these constantly) must find the queue empty.
  await store().refreshSettings();
  assert.equal(
    countCalls("changeThemeSetting"),
    1,
    "a second hydration must not re-apply the already-replayed intent",
  );
  assert.equal(
    countCalls("changeHoldThresholdMsSetting"),
    1,
    "a second hydration must not re-apply the clamped intent either",
  );
}

// --- Brief arm 3: the post-hydration path is unchanged ----------------------

{
  await store().updateSetting("app_language", "de");
  assert.equal(
    store().settings!.app_language,
    "de",
    "post-hydration updateSetting optimistically writes",
  );
  assert.deepEqual(
    calls.find(
      (call: RecordedCall) => call.method === "changeAppLanguageSetting",
    )!.args,
    ["de"],
    "post-hydration updateSetting drives the backend updater",
  );

  // The KB-203 failure path still rolls back per-key and toasts.
  failNext.changePasteDelayMsSetting = "paste subsystem busy";
  await store().updateSetting("paste_delay_ms", 90);
  assert.equal(
    store().settings!.paste_delay_ms,
    60,
    "a rejected post-hydration write rolls back to the current stored value",
  );
  assert.equal(toasts.length, 1, "the rejected write toasted exactly once");
  assert.equal(
    store().isUpdatingKey("paste_delay_ms"),
    false,
    "the isUpdating flag clears after a rejected write",
  );
}

// --- Numeric clamps at the setter -------------------------------------------

{
  // Table pinned against the UI components that declare each bound, so a
  // component edit that moves a bound shows up here as a diff.
  assert.deepEqual(SETTING_NUMERIC_CLAMPS, {
    hold_threshold_ms: { kind: "minMax", min: 100, max: 1000 },
    audio_feedback_volume: { kind: "minMax", min: 0, max: 1 },
    word_correction_threshold: { kind: "minMax", min: 0, max: 1 },
    paste_delay_ms: { kind: "minMax", min: 10, max: 5000 },
    paste_delay_after_ms: { kind: "minMax", min: 10, max: 5000 },
    history_limit: { kind: "minMax", min: 0, max: 1000 },
    extra_recording_buffer_ms: { kind: "minMax", min: 0, max: 1500 },
    streaming_release_tail_ms: { kind: "minMax", min: 0, max: 1000 },
    memory_gate_headroom_mb: { kind: "zeroOrAtLeast", least: 5 },
  });

  // Direct clamp behavior: floor/ceiling, passthrough, memory rule.
  assert.equal(
    clampSettingValue("hold_threshold_ms", 5),
    100,
    "below min floors",
  );
  assert.equal(
    clampSettingValue("hold_threshold_ms", 5000),
    1000,
    "above max caps",
  );
  assert.equal(
    clampSettingValue("hold_threshold_ms", 300),
    300,
    "in-range passes",
  );
  assert.equal(
    clampSettingValue("audio_feedback_volume", 1.7),
    1,
    "volume clamps to its 0..1 bound",
  );
  assert.equal(
    clampSettingValue("memory_gate_headroom_mb", 3),
    0,
    "a 1-4 margin normalizes to 0, mirroring the backend load guard, never to 5",
  );
  assert.equal(
    clampSettingValue("memory_gate_headroom_mb", 0),
    0,
    "0 (guard off) stays 0",
  );
  assert.equal(
    clampSettingValue("memory_gate_headroom_mb", 512),
    512,
    ">= 5 passes untouched",
  );
  assert.equal(
    clampSettingValue("theme", "dark"),
    "dark",
    "keys without a declared bound pass through",
  );

  // Through the setter: optimistic write AND backend write both see the
  // clamped value.
  await store().updateSetting("paste_delay_ms", 3);
  assert.equal(
    store().settings!.paste_delay_ms,
    10,
    "the optimistic write carries the clamped value",
  );
  const pasteDelayCalls = calls.filter(
    (call: RecordedCall) => call.method === "changePasteDelayMsSetting",
  );
  assert.deepEqual(
    pasteDelayCalls[pasteDelayCalls.length - 1]!.args,
    [10],
    "the backend updater receives the clamped value",
  );
}

console.log("settingsStore (W1-L4 wiring): all assertions passed");
