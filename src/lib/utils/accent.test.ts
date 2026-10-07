import assert from "node:assert/strict";
import { ACCENTS, DEFAULT_ACCENT_ID, effectiveAccentPalette } from "./accent";

/* WCAG contrast helpers (https://www.w3.org/WAI/WCAG22/Understanding/contrast-minimum.html). */
function channel(c: number): number {
  const s = c / 255;
  return s <= 0.04045 ? s / 12.92 : Math.pow((s + 0.055) / 1.055, 2.4);
}

function relativeLuminance(hex: string): number {
  const m = /^#([0-9a-f]{6})$/i.exec(hex.trim());
  assert.ok(m, `not a 6-digit hex color: ${hex}`);
  const n = parseInt(m[1], 16);
  return (
    0.2126 * channel((n >> 16) & 0xff) +
    0.7152 * channel((n >> 8) & 0xff) +
    0.0722 * channel(n & 0xff)
  );
}

function contrastRatio(a: string, b: string): number {
  const la = relativeLuminance(a);
  const lb = relativeLuminance(b);
  const [hi, lo] = la >= lb ? [la, lb] : [lb, la];
  return (hi + 0.05) / (lo + 0.05);
}

/* Theme backgrounds the render colors must sit on (theme.css). */
const THEME_BACKGROUND: Record<"light" | "dark", string> = {
  light: "#fbfbfb",
  dark: "#2c2b29",
};

/* 1. The default pins today's exact palette — no behavior change until the
      user picks an accent. */
const pink = ACCENTS.find((a) => a.id === DEFAULT_ACCENT_ID);
assert.ok(pink, "default pink accent exists");
assert.deepEqual(effectiveAccentPalette(DEFAULT_ACCENT_ID, "light"), {
  color: "#faa2ca", // --light-color-logo-primary
  fill: "#da5893", // --color-background-ui
  onFill: "#0f0f0f", // --light-color-text (badge text today)
  knob: "#ffffff", // ToggleSwitch knob today
});
assert.deepEqual(effectiveAccentPalette(DEFAULT_ACCENT_ID, "dark"), {
  color: "#f28cbb", // --dark-color-logo-primary
  fill: "#da5893",
  onFill: "#fbfbfb", // --dark-color-text
  knob: "#ffffff",
});

/* 2. Unknown / missing ids fall back to the default instead of crashing or
      rendering an invisible UI. */
assert.deepEqual(effectiveAccentPalette("not-a-color", "dark"), pink.dark);
assert.deepEqual(effectiveAccentPalette(null, "light"), pink.light);
assert.deepEqual(effectiveAccentPalette(undefined, "dark"), pink.dark);

/* 3. The mapping is theme-sensitive: black renders near-black on light and
      light gray on dark (the deliberate contrast flip for monochrome). */
const black = ACCENTS.find((a) => a.id === "black");
assert.ok(black, "black accent exists");
assert.equal(effectiveAccentPalette("black", "light").color, "#18181b");
assert.equal(effectiveAccentPalette("black", "dark").color, "#d4d4d8");
const white = ACCENTS.find((a) => a.id === "white");
assert.ok(white, "white accent exists");
assert.equal(effectiveAccentPalette("white", "light").color, "#27272a");
assert.equal(effectiveAccentPalette("white", "dark").color, "#fafafa");

/* 4. Swatch-grid integrity: the full requested set is present exactly once,
      and every swatch is a valid 6-digit hex so the grid always renders
      visible (cells also carry a CSS border). */
const expectedIds = [
  "black",
  "gray",
  "white",
  "red",
  "orange",
  "amber",
  "green",
  "teal",
  "cyan",
  "blue",
  "indigo",
  "violet",
  "magenta",
  "pink",
];
assert.deepEqual(
  ACCENTS.map((a) => a.id),
  expectedIds,
);
for (const accent of ACCENTS) {
  assert.match(accent.swatch, /^#[0-9a-f]{6}$/i);
}

/* 5. Contrast discipline for every chosen (non-default) accent on both
      themes:
        - render color vs theme background  >= 4.5:1 (text usage)
        - fill vs onFill                    >= 4.5:1 (badge labels)
        - fill vs knob                      >= 3:1  (WCAG 1.4.11 UI component)
      The default pink is pinned by (1) instead — its historical values are
      intentionally not re-litigated here. */
for (const accent of ACCENTS) {
  if (accent.id === DEFAULT_ACCENT_ID) continue;
  for (const theme of ["light", "dark"] as const) {
    const p = effectiveAccentPalette(accent.id, theme);
    const bg = contrastRatio(p.color, THEME_BACKGROUND[theme]);
    assert.ok(
      bg >= 4.5,
      `${accent.id}/${theme}: render ${p.color} vs ${THEME_BACKGROUND[theme]} = ${bg.toFixed(2)}`,
    );
    const on = contrastRatio(p.fill, p.onFill);
    assert.ok(
      on >= 4.5,
      `${accent.id}/${theme}: onFill ${p.onFill} on ${p.fill} = ${on.toFixed(2)}`,
    );
    const knob = contrastRatio(p.fill, p.knob);
    assert.ok(
      knob >= 3,
      `${accent.id}/${theme}: knob ${p.knob} on ${p.fill} = ${knob.toFixed(2)}`,
    );
  }
}

console.log("accent: all assertions passed");
