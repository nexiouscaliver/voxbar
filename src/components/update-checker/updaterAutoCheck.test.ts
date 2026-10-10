import assert from "node:assert/strict";
import { resolveAutoCheck } from "./updaterAutoCheck";

// Fresh launch with checks enabled: the still-loading pass keeps the latch
// armed (an unknown state is not a policy)...
assert.deepEqual(
  resolveAutoCheck({
    settingsLoaded: false,
    updateChecksEnabled: true,
    autoUpdateSupported: true,
    hasAutoChecked: false,
  }),
  { shouldRunCheck: false, hasAutoChecked: false },
);
// ...and the first loaded pass fires the one silent startup check, latching
// so it never repeats.
assert.deepEqual(
  resolveAutoCheck({
    settingsLoaded: true,
    updateChecksEnabled: true,
    autoUpdateSupported: true,
    hasAutoChecked: false,
  }),
  { shouldRunCheck: true, hasAutoChecked: true },
);
assert.deepEqual(
  resolveAutoCheck({
    settingsLoaded: true,
    updateChecksEnabled: true,
    autoUpdateSupported: true,
    hasAutoChecked: true,
  }),
  { shouldRunCheck: false, hasAutoChecked: true },
);

// KB-191: a launch with checks disabled consumes the session's automatic
// check (no network call)...
assert.deepEqual(
  resolveAutoCheck({
    settingsLoaded: true,
    updateChecksEnabled: false,
    autoUpdateSupported: true,
    hasAutoChecked: false,
  }),
  { shouldRunCheck: false, hasAutoChecked: true },
);
// ...so re-enabling checks mid-session - the latch carried over from the
// disabled pass - must not fire a silent check as a side effect of the
// toggle flip. The next launch auto-checks again via the fresh-launch cases
// above.
assert.deepEqual(
  resolveAutoCheck({
    settingsLoaded: true,
    updateChecksEnabled: true,
    autoUpdateSupported: true,
    hasAutoChecked: true,
  }),
  { shouldRunCheck: false, hasAutoChecked: true },
);

// Disabled *before* settings load stays armed: the toggle-off is not known
// yet, and a fresh launch that loads with checks enabled must still check.
assert.deepEqual(
  resolveAutoCheck({
    settingsLoaded: false,
    updateChecksEnabled: false,
    autoUpdateSupported: true,
    hasAutoChecked: false,
  }),
  { shouldRunCheck: false, hasAutoChecked: false },
);

// Platforms without updater artifacts never fire regardless of latch state.
assert.deepEqual(
  resolveAutoCheck({
    settingsLoaded: true,
    updateChecksEnabled: true,
    autoUpdateSupported: false,
    hasAutoChecked: false,
  }),
  { shouldRunCheck: false, hasAutoChecked: false },
);
assert.deepEqual(
  resolveAutoCheck({
    settingsLoaded: true,
    updateChecksEnabled: true,
    autoUpdateSupported: false,
    hasAutoChecked: true,
  }),
  { shouldRunCheck: false, hasAutoChecked: true },
);

console.log("updaterAutoCheck: all assertions passed");
