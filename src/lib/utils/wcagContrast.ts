import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";

/* Single shared table for the WCAG contrast pins (KB-267 emerald family,
   KB-283 amber family, KB-288 de-hardcoding). Before this module both
   blocks of the onboarding pin carried their own copies of the light
   background hex and the palette hexes, so a theme.css background change
   would have left the pins silently measuring against a stale surface.

   The theme backgrounds are PARSED from src/styles/theme.css at run
   time, so the pin always measures against the background the app
   actually renders; the Tailwind hexes (not defined anywhere in this
   repo's CSS - they come from the framework's default palette) live in
   exactly one table here. */

/* WCAG contrast helpers
   (https://www.w3.org/WAI/WCAG22/Understanding/contrast-minimum.html). */
function channel(c: number): number {
  const s = c / 255;
  return s <= 0.04045 ? s / 12.92 : Math.pow((s + 0.055) / 1.055, 2.4);
}

export function relativeLuminance(hex: string): number {
  const m = /^#([0-9a-f]{6})$/i.exec(hex.trim());
  assert.ok(m, `not a 6-digit hex color: ${hex}`);
  const n = parseInt(m[1], 16);
  return (
    0.2126 * channel((n >> 16) & 0xff) +
    0.7152 * channel((n >> 8) & 0xff) +
    0.0722 * channel(n & 0xff)
  );
}

export function contrastRatio(a: string, b: string): number {
  const la = relativeLuminance(a);
  const lb = relativeLuminance(b);
  const [hi, lo] = la >= lb ? [la, lb] : [lb, la];
  return (hi + 0.05) / (lo + 0.05);
}

/* Read one `--name: #rrggbb;` custom property from the shared theme
   palette. Fails loudly (never returns a stale fallback) so a renamed or
   deleted token breaks the pins that measure against it. */
export function themeHex(token: string): string {
  const themeCss = fs.readFileSync(
    path.join(import.meta.dirname, "..", "..", "styles", "theme.css"),
    "utf8",
  );
  const m = new RegExp(
    `^\\s*${token.replace(/-/g, "\\-")}:\\s*(#[0-9a-f]{6})\\s*;`,
    "m",
  ).exec(themeCss);
  assert.ok(
    m,
    `theme.css must define ${token} as a 6-digit hex (KB-288: the contrast pins measure against it)`,
  );
  return m[1];
}

/* Tailwind v4 default palette hexes for the color families the contrast
   pins police. The repo's own CSS never redefines these shades, so this
   table is the single place a framework-palette bump lands. */
export const TAILWIND_TEXT_HEX: Record<
  "emerald" | "amber",
  Record<string, string>
> = {
  emerald: {
    "600": "#059669",
    "700": "#047857",
  },
  amber: {
    "600": "#d97706",
    "700": "#b45309",
  },
};
