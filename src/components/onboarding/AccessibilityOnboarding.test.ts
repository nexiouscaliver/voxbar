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

console.log("AccessibilityOnboarding: all assertions passed");
