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
 * content placed on solid accent fills (badge text, toggle knob). The pure
 * mapping below is unit-tested (`accent.test.ts`) against WCAG ratios.
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
export const ACCENT_STORAGE_KEY = "handy.accent";

/*
 * Fixed palette. Light render colors sit at Tailwind 600/700 depth (white
 * content on them passes 4.5:1), dark render colors at 400 depth (readable on
 * #2c2b29). The monochrome entries flip: black renders near-black on light
 * and light gray on dark, white vice versa - the only accessible reading of
 * those choices. The default `pink` entry pins today's exact values
 * (logo-primary #faa2ca/#f28cbb, background-ui #da5893, knob white, content
 * color = theme text) so the mapping is a no-op until a user picks.
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
      fill: "#f87171",
      onFill: "#1c1c1f",
      knob: "#1c1c1f",
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
      fill: "#fb923c",
      onFill: "#1c1c1f",
      knob: "#1c1c1f",
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
      fill: "#fbbf24",
      onFill: "#1c1c1f",
      knob: "#1c1c1f",
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
      fill: "#4ade80",
      onFill: "#1c1c1f",
      knob: "#1c1c1f",
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
      fill: "#2dd4bf",
      onFill: "#1c1c1f",
      knob: "#1c1c1f",
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
      fill: "#22d3ee",
      onFill: "#1c1c1f",
      knob: "#1c1c1f",
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
      fill: "#60a5fa",
      onFill: "#1c1c1f",
      knob: "#1c1c1f",
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
      fill: "#818cf8",
      onFill: "#1c1c1f",
      knob: "#1c1c1f",
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
      fill: "#a78bfa",
      onFill: "#1c1c1f",
      knob: "#1c1c1f",
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
      fill: "#e879f9",
      onFill: "#1c1c1f",
      knob: "#1c1c1f",
    },
  },
  {
    id: DEFAULT_ACCENT_ID,
    swatch: "#f28cbb",
    // Pins today's palette: logo pink, UI pink fill, white knob, theme-text
    // content color. Only reachable through the pure mapping (applyAccent
    // clears overrides instead), kept so the mapping is total.
    light: {
      color: "#faa2ca",
      fill: "#da5893",
      onFill: "#0f0f0f",
      knob: "#ffffff",
    },
    dark: {
      color: "#f28cbb",
      fill: "#da5893",
      onFill: "#fbfbfb",
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
