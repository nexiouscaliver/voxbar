import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";

/* KB-081 pin: the history star toggle was optimistic with a blind revert -
 * the failure path re-derived the restore as one more flip off whatever
 * the row CURRENTLY showed, so a concurrent "updated" history event (or a
 * second click racing the first request) made a failed toggle land on the
 * opposite of the pre-click state, silently. The fix: per-row pending
 * state (a second click on an in-flight row is ignored), the pre-click
 * value captured at click time and restored verbatim on failure, and a
 * failure toast. HistorySettings is a Tauri window component with no
 * React render harness in this repo, so this is a static source assertion
 * in the repo's assert-script style (same technique as Footer.test.ts). */

const here = import.meta.dirname;
const source = fs.readFileSync(path.join(here, "HistorySettings.tsx"), "utf8");

const toggleAt = source.indexOf("const toggleSaved");
assert.ok(toggleAt >= 0, "toggleSaved must exist");
const toggleEnd = source.indexOf("\n  };", toggleAt) + 1;
const toggle = source.slice(toggleAt, toggleEnd);

// Per-row pending gate: a second click while the row's request is in
// flight is the race that broke the revert.
assert.ok(
  toggle.includes("if (pendingSavedIds.has(id)) return;"),
  "KB-081: the toggle must ignore clicks on rows with a request in flight",
);

// The pre-click value is captured BEFORE the optimistic flip...
assert.ok(
  toggle.includes("const before ="),
  "KB-081: the pre-click saved value must be captured at click time",
);
const beforeAt = toggle.indexOf("const before =");
const flipAt = toggle.indexOf("setEntries", beforeAt);
assert.ok(
  beforeAt >= 0 && flipAt > beforeAt,
  "KB-081: the capture must precede the optimistic update",
);

// ...and the failure restores THAT value, not another flip. The single
// `!e.saved` flip that remains is the optimistic update itself, before the
// request; the revert arms must not re-derive anything.
const flips = (toggle.match(/saved: !e\.saved/g) ?? []).length;
assert.equal(
  flips,
  1,
  `KB-081: exactly one blind flip may remain (the optimistic update); reverts must restore the captured value (found ${flips})`,
);
const optimisticFlipAt = toggle.indexOf("saved: !e.saved");
const tryAt = toggle.indexOf("try {");
assert.ok(
  optimisticFlipAt < tryAt,
  "KB-081: the one blind flip is the optimistic update, before the request",
);

// Failures are visible: both paths toast the localized error.
assert.equal(
  (
    toggle.match(
      /toast\.error\(t\("settings\.history\.savedToggleError"\)\)/g,
    ) ?? []
  ).length,
  2,
  "KB-081: both failure paths must show the failure toast",
);

// The pending set is armed before the request and cleared in finally, so a
// settled row (success or failure) becomes clickable again.
assert.ok(
  toggle.includes("setPendingSavedIds((prev) => new Set(prev).add(id));") &&
    /finally\s*\{[\s\S]*next\.delete\(id\)/.test(toggle),
  "KB-081: the pending gate must arm before the request and clear in finally",
);

// The star button disables while the row's toggle is in flight.
assert.ok(
  source.includes("disabled={retrying || savedPending}"),
  "KB-081: the star button must disable while the row's toggle is pending",
);

console.log("HistorySettings: all assertions passed");
