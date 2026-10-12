import { commands } from "@/bindings";

/**
 * Accent color handling.
 *
 * Every accent surface in the app reads the `--color-logo-primary` /
 * `--color-background-ui` CSS tokens (Tailwind `*-logo-primary` /
 * `*-background-ui` utilities, overlay `--s-accent`). Those tokens resolve
 * through light/dark palette pairs declared once in `styles/theme.css`.
 *
 * This module offers a fixed set of swatches (no color wheel): picking one
 * overrides the palette pairs with inline custom properties on the document
 * root, so every theme selector (`prefers-color-scheme` media query or forced
 * `data-theme`) keeps resolving the pair for the active theme with no extra
 * JS. Picking the default (`pink`) clears the overrides, leaving `theme.css`
 * in charge - i.e. zero behavior change until the user chooses.
 *
 * Contrast discipline: monochrome accents would vanish if rendered literally
 * (black on the dark theme, white on light), so each swatch carries a
 * deliberate light-theme and dark-theme render color plus the colors for
 * content placed on solid accent fills (badge text, toggle knob). Fills are
 * 600/700-depth in both themes carrying white content, so the ON toggle's
 * knob is white for every chromatic accent in every theme; only the
 * monochrome accents' dark fills (light tracks) keep an ink knob and label.
 * The pure mapping below is unit-tested (`accent.test.ts`) against WCAG
 * ratios.
 */

export interface AccentPalette {
  /** Accent render color - text, icons, borders, rings, tinted fills. */
  color: string;
  /** Solid accent fill - checked toggle, solid badges, overlay pulse. */
  fill: string;
  /** Text color placed on a solid `fill` (e.g. badge labels). */
  onFill: string;
  /** Toggle-knob color placed on a solid `fill`. */
  knob: string;
}

export interface AccentDefinition {
  id: string;
  /** True swatch color shown in the picker grid (never theme-adjusted). */
  swatch: string;
  light: AccentPalette;
  dark: AccentPalette;
}

export type ThemeName = "light" | "dark";

export const DEFAULT_ACCENT_ID = "pink";
export const ACCENT_STORAGE_KEY = "voxbar.accent";

/*
 * Fixed palette. Light render colors sit at Tailwind 600/700 depth (white
 * content on them passes 4.5:1), dark RENDER colors at 400 depth (readable
 * as text/icons on #2c2b29). Solid FILLS are 600/700 depth in BOTH themes
 * with white content: white label + white knob on one saturated track in
 * every theme, so the ON toggle (and any selected pill) renders as the
 * same component everywhere; the audit's near-black knob on the 400-depth
 * blue/violet dark tracks read as a hole. The monochrome entries flip:
 * black renders near-black on light and light gray on dark - their dark
 * fills stay light tracks and are the ONLY accents keeping an ink knob
 * and ink label. The default `pink` entry pins today's exact values
 * (logo-primary #c2417c/#f28cbb, background-ui #c2417c, white knob and
 * white content on the fill) so the mapping is a no-op until a user picks.
 */
export const ACCENTS: readonly AccentDefinition[] = [
  {
    id: "black",
    swatch: "#18181b",
    light: {
      color: "#18181b",
      fill: "#18181b",
      onFill: "#f4f4f5",
      knob: "#ffffff",
    },
    dark: {
      color: "#d4d4d8",
      fill: "#d4d4d8",
      onFill: "#18181b",
      knob: "#18181b",
    },
  },
  {
    id: "gray",
    swatch: "#71717a",
    light: {
      color: "#52525b",
      fill: "#52525b",
      onFill: "#f4f4f5",
      knob: "#ffffff",
    },
    dark: {
      color: "#a1a1aa",
      fill: "#a1a1aa",
      onFill: "#18181b",
      knob: "#18181b",
    },
  },
  {
    id: "white",
    swatch: "#ffffff",
    light: {
      color: "#27272a",
      fill: "#27272a",
      onFill: "#f4f4f5",
      knob: "#ffffff",
    },
    dark: {
      color: "#fafafa",
      fill: "#fafafa",
      onFill: "#18181b",
      knob: "#18181b",
    },
  },
  {
    id: "red",
    swatch: "#dc2626",
    light: {
      color: "#dc2626",
      fill: "#dc2626",
      onFill: "#ffffff",
      knob: "#ffffff",
    },
    dark: {
      color: "#f87171",
      fill: "#dc2626",
      onFill: "#ffffff",
      knob: "#ffffff",
    },
  },
  {
    id: "orange",
    swatch: "#c2410c",
    light: {
      color: "#c2410c",
      fill: "#c2410c",
      onFill: "#ffffff",
      knob: "#ffffff",
    },
    dark: {
      color: "#fb923c",
      fill: "#c2410c",
      onFill: "#ffffff",
      knob: "#ffffff",
    },
  },
  {
    id: "amber",
    swatch: "#b45309",
    light: {
      color: "#b45309",
      fill: "#b45309",
      onFill: "#ffffff",
      knob: "#ffffff",
    },
    dark: {
      color: "#fbbf24",
      fill: "#b45309",
      onFill: "#ffffff",
      knob: "#ffffff",
    },
  },
  {
    id: "green",
    swatch: "#15803d",
    light: {
      color: "#15803d",
      fill: "#15803d",
      onFill: "#ffffff",
      knob: "#ffffff",
    },
    dark: {
      color: "#4ade80",
      fill: "#15803d",
      onFill: "#ffffff",
      knob: "#ffffff",
    },
  },
  {
    id: "teal",
    swatch: "#0f766e",
    light: {
      color: "#0f766e",
      fill: "#0f766e",
      onFill: "#ffffff",
      knob: "#ffffff",
    },
    dark: {
      color: "#2dd4bf",
      fill: "#0f766e",
      onFill: "#ffffff",
      knob: "#ffffff",
    },
  },
  {
    id: "cyan",
    swatch: "#0e7490",
    light: {
      color: "#0e7490",
      fill: "#0e7490",
      onFill: "#ffffff",
      knob: "#ffffff",
    },
    dark: {
      color: "#22d3ee",
      fill: "#0e7490",
      onFill: "#ffffff",
      knob: "#ffffff",
    },
  },
  {
    id: "blue",
    swatch: "#2563eb",
    light: {
      color: "#2563eb",
      fill: "#2563eb",
      onFill: "#ffffff",
      knob: "#ffffff",
    },
    dark: {
      color: "#60a5fa",
      fill: "#2563eb",
      onFill: "#ffffff",
      knob: "#ffffff",
    },
  },
  {
    id: "indigo",
    swatch: "#4f46e5",
    light: {
      color: "#4f46e5",
      fill: "#4f46e5",
      onFill: "#ffffff",
      knob: "#ffffff",
    },
    dark: {
      color: "#818cf8",
      fill: "#4f46e5",
      onFill: "#ffffff",
      knob: "#ffffff",
    },
  },
  {
    id: "violet",
    swatch: "#7c3aed",
    light: {
      color: "#7c3aed",
      fill: "#7c3aed",
      onFill: "#ffffff",
      knob: "#ffffff",
    },
    dark: {
      color: "#a78bfa",
      fill: "#7c3aed",
      onFill: "#ffffff",
      knob: "#ffffff",
    },
  },
  {
    id: "magenta",
    swatch: "#c026d3",
    light: {
      color: "#c026d3",
      fill: "#c026d3",
      onFill: "#ffffff",
      knob: "#ffffff",
    },
    dark: {
      color: "#e879f9",
      fill: "#c026d3",
      onFill: "#ffffff",
      knob: "#ffffff",
    },
  },
  {
    id: DEFAULT_ACCENT_ID,
    swatch: "#f28cbb",
    // Pins today's palette: deep pink render color in light (#c2417c on
    // #fbfbfb = 4.84:1 as text; the pale #faa2ca tint measured 1.83:1),
    // pink render color in dark, deep pink fill in both themes, white knob
    // and white content on the fill. Only reachable through the pure
    // mapping (applyAccent clears overrides instead), kept so the mapping
    // is total.
    light: {
      color: "#c2417c",
      fill: "#c2417c",
      onFill: "#ffffff",
      knob: "#ffffff",
    },
    dark: {
      color: "#f28cbb",
      fill: "#c2417c",
      onFill: "#ffffff",
      knob: "#ffffff",
    },
  },
];

const byId: ReadonlyMap<string, AccentDefinition> = new Map(
  ACCENTS.map((accent) => [accent.id, accent]),
);

export const isAccentId = (value: unknown): value is string =>
  typeof value === "string" && byId.has(value);

/**
 * The pure (accent, theme) → effective render palette mapping. Unknown or
 * missing ids fall back to the default pink so a stale stored value can
 * never produce an invisible UI.
 */
export function effectiveAccentPalette(
  accentId: string | null | undefined,
  theme: ThemeName,
): AccentPalette {
  const fallback = byId.get(DEFAULT_ACCENT_ID);
  if (!fallback) throw new Error("accent table is missing its default entry");
  const accent =
    (accentId != null ? byId.get(accentId) : undefined) ?? fallback;
  return theme === "dark" ? accent.dark : accent.light;
}

/* Inline overrides mirroring the light/dark pairs in styles/theme.css. */
const OVERRIDE_PROPS: ReadonlyArray<
  [string, (def: AccentDefinition) => string]
> = [
  ["--light-color-logo-primary", (d) => d.light.color],
  ["--dark-color-logo-primary", (d) => d.dark.color],
  ["--light-color-background-ui", (d) => d.light.fill],
  ["--dark-color-background-ui", (d) => d.dark.fill],
  ["--light-accent-on", (d) => d.light.onFill],
  ["--dark-accent-on", (d) => d.dark.onFill],
  ["--light-accent-knob", (d) => d.light.knob],
  ["--dark-accent-knob", (d) => d.dark.knob],
];

/** Apply an accent to the document root and remember it for the next launch. */
export function applyAccent(accentId: string): void {
  const root = document.documentElement;
  const accent = byId.get(accentId.trim().toLowerCase());
  if (!accent || accent.id === DEFAULT_ACCENT_ID) {
    // Default (or unknown): let theme.css's own palette rule - the exact
    // historical rendering.
    for (const [prop] of OVERRIDE_PROPS) root.style.removeProperty(prop);
  } else {
    for (const [prop, pick] of OVERRIDE_PROPS) {
      root.style.setProperty(prop, pick(accent));
    }
  }
  try {
    localStorage.setItem(
      ACCENT_STORAGE_KEY,
      accent ? accent.id : DEFAULT_ACCENT_ID,
    );
  } catch {
    // localStorage may be unavailable; the setting still persists in
    // AppSettings, so this only costs a one-frame flash on boot.
  }
}

/** Read the last-applied accent for synchronous boot-time application. */
export function getStoredAccent(): string {
  try {
    const stored = localStorage.getItem(ACCENT_STORAGE_KEY);
    if (isAccentId(stored)) return stored;
  } catch {
    // ignore
  }
  return DEFAULT_ACCENT_ID;
}

/** Apply the persisted accent from AppSettings (the source of truth). */
export const syncAccentFromSettings = async (): Promise<void> => {
  try {
    const result = await commands.getAppSettings();
    if (result.status === "ok") {
      applyAccent(result.data.accent_color ?? DEFAULT_ACCENT_ID);
    }
  } catch (e) {
    console.warn("Failed to sync accent from settings:", e);
  }
};
