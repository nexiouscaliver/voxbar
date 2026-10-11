import assert from "node:assert/strict";
import type { AppSettings as Settings } from "@/bindings";
import {
  PRE_HYDRATION_STASH_LIMIT,
  PreHydrationUpdateQueue,
} from "./settingsHydrationQueue";

// W1-L4 hydration guard: while settingsStore's settings === null (the boot
// window before the first getAppSettings response), updateSetting used to
// skip only the optimistic write (the spread kept settings null) but still
// run the backend updater unconditionally. The in-flight hydration response
// then overwrote the store with the pre-write value, so the user's change
// vanished from the UI even though the backend had accepted it.
//
// The fix: updateSetting (settingsStore.ts, the W1-L4 hydration guard at
// the top of the action) now does NOTHING observable while settings is
// null (no optimistic write, no backend write, no isUpdating spinner) and
// stashes the intent in a module-level PreHydrationUpdateQueue instead;
// refreshSettings drains and replays the queue right after its successful
// set(), serially and through the normal updateSetting path so every
// replayed intent gets the full optimistic-write / backend-write /
// KB-203-rollback treatment a direct call would.
//
// This file is the contract test for the pure queue module, the same
// split the repo uses for store-coupled decisions (settingsWriteOutcome,
// updateCheckGate, noticeRouting): the queue decides what is retained and
// in what order; the store performs the effects. The store WIRING (the
// guard in updateSetting, the drain-and-replay in refreshSettings, the
// failed-hydration warn) is exercised against the real store in
// settingsStore.test.ts, which mocks only the Tauri edges (@/bindings,
// ../i18n, sonner) with bun's mock.module; this file pins the queue's own
// semantics:
//
//   1. A pre-hydration updateSetting(key, value) call must resolve
//      without touching zustand state (settings stays null) and without
//      invoking ANY backend command, and its intent must reach the queue
//      via stash (settingsStore.ts guard: stash then return).
//   2. The first successful refreshSettings must drain() the queue once
//      and replay each entry serially via updateSetting; a second
//      refreshSettings (the settings-changed event fires one) must find
//      the queue empty and re-apply nothing (drain is take, not peek).
//   3. The post-hydration updateSetting path is unchanged: optimistic
//      write, backend updater, mapCommandResult rollback.
//
// Queue semantics pinned below: last write per key wins, first-touch
// replay order, bounded distinct keys with in-limit overwrites always
// retained.

// --- drain() on a fresh queue is empty, not undefined/null ----------------

{
  const queue = new PreHydrationUpdateQueue();
  assert.deepEqual(queue.drain(), [], "a fresh queue drains to no entries");
  assert.deepEqual(
    queue.drain(),
    [],
    "draining an already-drained queue stays empty (drain is take, not peek)",
  );
}

// --- single intent is retained and drains exactly once ---------------------

{
  const queue = new PreHydrationUpdateQueue();
  assert.deepEqual(
    queue.stash("theme", "dark"),
    { stashed: true },
    "an intent under the limit is stashed",
  );
  assert.deepEqual(
    queue.drain(),
    [["theme", "dark"]],
    "the stashed intent drains with its key and value",
  );
  assert.deepEqual(queue.drain(), [], "the intent does not drain twice");
}

// --- last write per key wins ------------------------------------------------

{
  const queue = new PreHydrationUpdateQueue();
  queue.stash("theme", "dark");
  queue.stash("theme", "light");
  // One entry, the LATEST value: replaying a superseded intermediate would
  // fire a backend write whose result is immediately overwritten.
  assert.deepEqual(
    queue.drain(),
    [["theme", "light"]],
    "repeat updates to one key drain as a single latest-value entry",
  );
  assert.equal(queue.size, 0, "size counts distinct keys, not writes");
}

// --- first-touch replay order, latest values --------------------------------

{
  const queue = new PreHydrationUpdateQueue();
  queue.stash("theme", "dark");
  queue.stash("app_language", "de");
  queue.stash("theme", "light"); // overwrite keeps the key's first slot
  assert.deepEqual(
    queue.drain(),
    [
      ["theme", "light"],
      ["app_language", "de"],
    ],
    "replay order is first-touch with the latest value per key",
  );
}

// --- the stash is bounded on DISTINCT keys ----------------------------------

{
  const bounded = new PreHydrationUpdateQueue();

  // Drive the bound off the exported constant, not a hardcoded 16, so the
  // test states the behavior (refuse new distinct keys at the limit) and
  // not the number. Fixture values are per-key type-correct so this file
  // passes the same strict tsc the build runs.
  const fill = [
    ["app_language", "de"],
    ["selected_language", "en"],
    ["accent_color", "blue"],
    ["whats_new_last_seen_version", "1.5.4"],
    ["external_script_path", "/tmp/set.sh"],
    ["selected_microphone", "Default"],
    ["clamshell_microphone", null],
    ["start_hidden", false],
    ["debug_mode", true],
    ["auto_fallback", false],
    ["memory_gate_headroom_mb", 512],
    ["hold_threshold_ms", 300],
    ["audio_feedback_volume", 0.5],
    ["paste_delay_ms", 60],
    ["history_limit", 50],
  ] as const satisfies ReadonlyArray<readonly [keyof Settings, unknown]>;
  assert.ok(
    fill.length === PRE_HYDRATION_STASH_LIMIT - 1,
    "fixture must land exactly one key short of the limit",
  );
  for (const [key, value] of fill) {
    assert.deepEqual(
      bounded.stash(key, value),
      { stashed: true },
      `distinct key ${String(key)} under the limit is stashed`,
    );
  }
  assert.equal(bounded.size, PRE_HYDRATION_STASH_LIMIT - 1);

  // The limit-th distinct key still fits.
  assert.deepEqual(
    bounded.stash("theme", "dark"),
    { stashed: true },
    "the limit-th distinct key is stashed",
  );
  assert.equal(bounded.size, PRE_HYDRATION_STASH_LIMIT);

  // One beyond the limit is refused, with the reason the store's warn log
  // and any future telemetry key off of.
  assert.deepEqual(
    bounded.stash("translate_to_english", true),
    { stashed: false, reason: "limit" },
    "a NEW distinct key beyond the limit is refused",
  );
  assert.equal(
    bounded.size,
    PRE_HYDRATION_STASH_LIMIT,
    "a refused key does not occupy a slot",
  );

  // Overwriting an existing key at the limit is always allowed: a user
  // refining one setting pre-hydration must never lose intent to the
  // bound, and overwrites do not grow the queue.
  assert.deepEqual(
    bounded.stash("theme", "light"),
    { stashed: true },
    "overwriting an already-stashed key at the limit is still retained",
  );
  assert.equal(bounded.size, PRE_HYDRATION_STASH_LIMIT);

  const drained = bounded.drain();
  assert.equal(
    drained.length,
    PRE_HYDRATION_STASH_LIMIT,
    "the drained replay carries exactly the limit distinct keys",
  );
  assert.ok(
    !drained.some(([key]) => key === "translate_to_english"),
    "the refused key never reaches replay",
  );
  assert.deepEqual(
    drained.find(([key]) => key === "theme"),
    ["theme", "light"],
    "the at-limit overwrite replays its latest value",
  );
}

console.log("settingsHydrationQueue: all assertions passed");
