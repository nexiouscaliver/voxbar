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
  onboardingSrc.match(/text-emerald-700 dark:text-emerald-400/g) ?? []
).length;
assert.equal(
  emeraldPairs,
  3,
  `KB-091: all three checkmarks must pair text-emerald-700 with dark:text-emerald-400 (found ${emeraldPairs})`,
);
assert.ok(
  !/text-emerald-400(?! )/.test(
    onboardingSrc.replace(/dark:text-emerald-400/g, ""),
  ),
  "KB-091: no lone text-emerald-400 may remain (light-theme contrast)",
);

/* ---- KB-267 pin ---- */

// The Granted labels' own comment promised a 4.5:1 bar, but emerald-600 on
// the light background measures 3.64:1. This pin COMPUTES the WCAG 2.1
// contrast ratio of the light-theme text color actually used against the
// light background token (theme.css --light-color-background), so a future
// shade swap that dips under the bar fails with the measured number.
{
  const LIGHT_BACKGROUND = "#fbfbfb"; // styles/theme.css: --light-color-background
  const relativeLuminance = (hex: string): number => {
    const channels = [1, 3, 5].map(
      (at) => parseInt(hex.slice(at, at + 2), 16) / 255,
    );
    const [r, g, b] = channels.map((c) =>
      c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4,
    );
    return 0.2126 * r + 0.7152 * g + 0.0722 * b;
  };
  const contrast = (fg: string, bg: string): number => {
    const [l1, l2] = [relativeLuminance(fg), relativeLuminance(bg)].sort(
      (a, b) => b - a,
    );
    return (l1 + 0.05) / (l2 + 0.05);
  };
  const shadeHex = onboardingSrc.match(
    /text-emerald-(\d{3}) dark:text-emerald-400/,
  )?.[1];
  assert.ok(shadeHex, "the checkmark pair must keep a numeric emerald shade");
  // Tailwind v4 default palette: emerald-600 #059669, emerald-700 #047857.
  const EMERALD: Record<string, string> = {
    "600": "#059669",
    "700": "#047857",
  };
  const lightShade = EMERALD[shadeHex];
  assert.ok(
    lightShade !== undefined,
    `the light shade must be a known emerald hex (got emerald-${shadeHex})`,
  );
  const ratio = contrast(lightShade, LIGHT_BACKGROUND);
  assert.ok(
    ratio >= 4.5,
    `KB-267: light-theme Granted text must reach 4.5:1 on ${LIGHT_BACKGROUND}, ` +
      `measured ${ratio.toFixed(2)}:1 (emerald-600 is 3.64:1; emerald-700 is 5.30:1)`,
  );
  // And the too-dim shade is gone from the light slot entirely.
  assert.ok(
    !onboardingSrc.includes("text-emerald-600"),
    "KB-267: text-emerald-600 must not remain (3.64:1 on the light background)",
  );
}

console.log("AccessibilityOnboarding: all assertions passed");
