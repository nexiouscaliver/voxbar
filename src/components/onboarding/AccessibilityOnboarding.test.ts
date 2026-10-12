import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";

// KB-187: both onboarding permission cards (microphone and accessibility)
// used bg-white/5 as their card fill, which is invisible on the light
// theme's near-white background - the cards read as unpainted outlines on
// first launch. The fix fills them with bg-background, the same token the
// settings card chrome (SettingsGroup) uses, so the cards read in both
// themes.
//
// AccessibilityOnboarding is a Tauri window component with no React render
// harness in this repo, so this is a static source assertion in the repo's
// assert-script style (same technique as Footer.test.ts).

const onboardingSrc = fs.readFileSync(
  path.join(import.meta.dirname, "AccessibilityOnboarding.tsx"),
  "utf8",
);

// Both permission cards carry the theme token fill. Exactly two card
// containers exist (microphone + accessibility); each must use
// bg-background, not a hard-coded white tint.
const themedCards = (
  onboardingSrc.match(
    /className="w-full p-4 rounded-lg bg-background border border-mid-gray\/20"/g,
  ) ?? []
).length;
assert.equal(
  themedCards,
  2,
  `KB-187: both permission cards must fill with bg-background (found ${themedCards})`,
);

// The light-theme-invisible fill must not survive anywhere in a className:
// comments may name bg-white/5 (they explain the fix), class strings may not.
const whiteTintClasses = (
  onboardingSrc.match(/className="[^"]*bg-white\/5/g) ?? []
).length;
assert.equal(
  whiteTintClasses,
  0,
  `KB-187: no className may use bg-white/5, invisible on the light theme (found ${whiteTintClasses})`,
);

/* ---- KB-091 pins ---- */

// Check Again must answer with the permission's CURRENT state, never re-arm
// the 60s waiting spinner: the deadline that produced the denied guidance
// stays elapsed, so a still-denied answer keeps the guidance on screen
// (handleCheckAgain neither sets microphone to "waiting" nor calls
// startMicDeadline), and an immediate permission query decides.
const checkAgainAt = onboardingSrc.indexOf("const handleCheckAgain");
assert.ok(checkAgainAt >= 0, "handleCheckAgain must exist");
const checkAgainEnd = onboardingSrc.indexOf("};", checkAgainAt) + 2;
const checkAgain = onboardingSrc.slice(checkAgainAt, checkAgainEnd);
assert.ok(
  !checkAgain.includes("startMicDeadline"),
  "KB-091: Check Again must not re-arm the 60s denial deadline",
);
assert.ok(
  !checkAgain.includes('"waiting"') && !checkAgain.includes("'waiting'"),
  "KB-091: Check Again must not re-enter the waiting spinner state",
);
assert.ok(
  checkAgain.includes("checkMicrophonePermission") ||
    checkAgain.includes("hasWindowsMicrophoneAccess"),
  "KB-091: Check Again must query the permission state immediately",
);

// The three emerald checkmarks carry the light/dark pair (emerald-400 alone
// fails 4.5:1 on the light theme's near-white background).
const emeraldPairs = (
  onboardingSrc.match(/text-emerald-600 dark:text-emerald-400/g) ?? []
).length;
assert.equal(
  emeraldPairs,
  3,
  `KB-091: all three checkmarks must pair text-emerald-600 with dark:text-emerald-400 (found ${emeraldPairs})`,
);
assert.ok(
  !/text-emerald-400(?! )/.test(
    onboardingSrc.replace(/dark:text-emerald-400/g, ""),
  ),
  "KB-091: no lone text-emerald-400 may remain (light-theme contrast)",
);

console.log("AccessibilityOnboarding: all assertions passed");
