import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";

import {
  TAILWIND_TEXT_HEX,
  contrastRatio,
  themeHex,
} from "../../lib/utils/wcagContrast";

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
// KB-288: the background and the palette hexes come from the shared
// contrast table (lib/utils/wcagContrast.ts), which parses theme.css -
// not from hardcoded copies here, so a theme background change moves the
// measurement with it.
{
  const LIGHT_BACKGROUND = themeHex("--light-color-background");
  const shadeHex = onboardingSrc.match(
    /text-emerald-(\d{3}) dark:text-emerald-400/,
  )?.[1];
  assert.ok(shadeHex, "the checkmark pair must keep a numeric emerald shade");
  const lightShade = TAILWIND_TEXT_HEX.emerald[shadeHex];
  assert.ok(
    lightShade !== undefined,
    `the light shade must be a known emerald hex (got emerald-${shadeHex})`,
  );
  const ratio = contrastRatio(lightShade, LIGHT_BACKGROUND);
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

// KB-267 review follow-up: the fix's own comment claims the onboarding pair
// is "the same pair the debug tables use", but three sibling light-theme
// sites (the live-log INFO tag and the post-process runs outcome/copied
// labels) still carried emerald-600 - the exact 3.64:1 failure the item
// fixes. They now carry the 700/400 pair, and this sweep pins both halves:
// no source under src/ may name text-emerald-600 at all (test files are
// exempt because this pin itself names the token it forbids; comments in
// shipped sources are covered, so a stale claim fails too), and the debug
// files must actually carry the pair (so the sweep cannot pass by deleting
// the sites).
{
  const walk = (dir: string): string[] =>
    fs
      .readdirSync(dir, { withFileTypes: true })
      .flatMap((entry) =>
        entry.isDirectory()
          ? walk(path.join(dir, entry.name))
          : /\.(ts|tsx)$/.test(entry.name) && !/\.test\.tsx?$/.test(entry.name)
            ? [path.join(dir, entry.name)]
            : [],
      );
  const srcRoot = path.join(import.meta.dirname, "..", "..");
  const sources = walk(srcRoot);
  assert.ok(
    sources.length > 50,
    `expected a real source tree, walked ${sources.length} files`,
  );
  for (const file of sources) {
    const source = fs.readFileSync(file, "utf8");
    assert.ok(
      !source.includes("text-emerald-600"),
      `KB-267: ${path.relative(srcRoot, file)} must not use text-emerald-600 ` +
        `(3.64:1 on the light background; emerald-700 is the light shade)`,
    );
  }
  for (const debugFile of [
    "settings/debug/LiveLogViewer.tsx",
    "settings/debug/PostProcessRunsTable.tsx",
  ]) {
    const debugSrc = fs.readFileSync(
      path.join(import.meta.dirname, "..", debugFile),
      "utf8",
    );
    assert.ok(
      debugSrc.includes("text-emerald-700 dark:text-emerald-400"),
      `KB-267: ${debugFile} must carry the emerald-700/400 light/dark pair`,
    );
  }
}

/* ---- KB-283 pin (amber family, same pattern as KB-267) ---- */

// The amber legs inside the very cells the emerald fix touched (the pp
// runs skipped/unchanged outcome and the live-log WARN tag) still carried
// amber-600, which measures 3.08:1 on the light background - under the
// 4.5:1 bar the emerald fix established for this table. Same pin shape as
// the emerald half above (and KB-288: same shared contrast table, with
// the background parsed from theme.css): compute the WCAG ratio from the
// real palette hexes, sweep every non-test source for the too-dim shade,
// and require the debug files to actually carry the passing pair.
{
  const LIGHT_BACKGROUND = themeHex("--light-color-background");
  const AMBER = TAILWIND_TEXT_HEX.amber;
  assert.ok(
    contrastRatio(AMBER["600"], LIGHT_BACKGROUND) < 4.5,
    "KB-283: the hex table must keep amber-600 measurable (3.08:1 expected)",
  );
  const walk = (dir: string): string[] =>
    fs
      .readdirSync(dir, { withFileTypes: true })
      .flatMap((entry) =>
        entry.isDirectory()
          ? walk(path.join(dir, entry.name))
          : /\.(ts|tsx)$/.test(entry.name) && !/\.test\.tsx?$/.test(entry.name)
            ? [path.join(dir, entry.name)]
            : [],
      );
  const srcRoot = path.join(import.meta.dirname, "..", "..");
  for (const file of walk(srcRoot)) {
    const source = fs.readFileSync(file, "utf8");
    assert.ok(
      !source.includes("text-amber-600"),
      `KB-283: ${path.relative(srcRoot, file)} must not use text-amber-600 ` +
        `(3.08:1 on the light background; amber-700 is the light shade)`,
    );
  }
  for (const debugFile of [
    "settings/debug/LiveLogViewer.tsx",
    "settings/debug/PostProcessRunsTable.tsx",
  ]) {
    const debugSrc = fs.readFileSync(
      path.join(import.meta.dirname, "..", debugFile),
      "utf8",
    );
    const amberShade = debugSrc.match(/text-amber-(\d{3}) dark:text-amber-400/)
      ?.[1];
    assert.ok(
      amberShade !== undefined,
      `KB-283: ${debugFile} must pair a light amber shade with dark:text-amber-400`,
    );
    const amberHex = AMBER[amberShade];
    assert.ok(
      amberHex !== undefined,
      `KB-283: the light shade must be a known amber hex (got amber-${amberShade})`,
    );
    const ratio = contrastRatio(amberHex, LIGHT_BACKGROUND);
    assert.ok(
      ratio >= 4.5,
      `KB-283: ${debugFile} amber-${amberShade} light-theme text must reach ` +
        `4.5:1 on ${LIGHT_BACKGROUND}, measured ${ratio.toFixed(2)}:1 ` +
        `(amber-600 is 3.08:1; amber-700 is 4.85:1)`,
    );
    assert.ok(
      debugSrc.includes("text-amber-700 dark:text-amber-400"),
      `KB-283: ${debugFile} must carry the amber-700/400 light/dark pair`,
    );
  }
}

console.log("AccessibilityOnboarding: all assertions passed");
