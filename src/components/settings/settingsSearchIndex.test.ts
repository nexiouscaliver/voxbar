import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { SEARCH_INDEX } from "./settingsSearchIndex";

// AUD-11: the settings search index claims every visible About row is
// indexed (SettingsSearch.tsx KB-190 comment), but the Theme and Accent
// Color rows that AboutSettings.tsx renders (theme.title from
// ThemeSelector.tsx, theme.accentColor.label from AccentColorSelector.tsx)
// are missing, so typing "theme" or "accent" returns nothing.
//
// Contract for the module under test (same pattern as
// commands/commandGroups.ts — "pure data ... unit-testable without React"):
// ./settingsSearchIndex must export SEARCH_INDEX, the exact readonly array
// of { section, key } entries the SettingsSearch component filters over
// (STATIC_INDEX plus the command-name entries), with no React/DOM/Tauri
// runtime imports so a plain bun script can import it.

const here = path.dirname(fileURLToPath(import.meta.url));
const en = JSON.parse(
  fs.readFileSync(
    path.join(here, "../../i18n/locales/en/translation.json"),
    "utf8",
  ),
) as Record<string, unknown>;

// i18next-style dotted-key lookup against the nested locale object — the
// same resolution t(entry.key) performs inside SettingsSearch. Returns
// undefined when the key does not exist.
const resolveEn = (key: string): string | undefined => {
  let node: unknown = en;
  for (const part of key.split(".")) {
    if (node == null || typeof node !== "object") return undefined;
    node = (node as Record<string, unknown>)[part];
  }
  return typeof node === "string" ? node : undefined;
};

// Guard: this is the real index the component searches, not a stub — the
// About section is present and its previously-indexed sibling rows
// (App Language per KB-190, Update Policy, the sidebar label) stay in.
assert.ok(SEARCH_INDEX.length > 0, "search index must not be empty");
const keys = SEARCH_INDEX.map((entry) => entry.key);
for (const sibling of [
  "sidebar.about",
  "appLanguage.title",
  "settings.about.updatePolicy.title",
]) {
  assert.ok(keys.includes(sibling), `${sibling} must stay indexed`);
}

// Core AUD-11 assertion: both visible About rows are indexed under the
// about section, exactly like their siblings.
const aboutKeys = SEARCH_INDEX.filter((entry) => entry.section === "about").map(
  (entry) => entry.key,
);
assert.ok(
  aboutKeys.includes("theme.title"),
  'About section must index theme.title — the Theme row (AboutSettings.tsx renders ThemeSelector with t("theme.title"))',
);
assert.ok(
  aboutKeys.includes("theme.accentColor.label"),
  'About section must index theme.accentColor.label — the Accent Color row (AboutSettings.tsx renders AccentColorSelector with t("theme.accentColor.label"))',
);

// The two keys must be the existing locale keys (fix direction), i.e. they
// resolve in the English translation to labels users can actually match.
assert.match(
  String(resolveEn("theme.title")),
  /theme/i,
  "theme.title must resolve to an English label containing 'theme'",
);
assert.match(
  String(resolveEn("theme.accentColor.label")),
  /accent/i,
  "theme.accentColor.label must resolve to an English label containing 'accent'",
);

// User-facing symptom, replicating the component's filter
// (SettingsSearch.tsx results useMemo: t(entry.key).toLowerCase()
// .includes(needle)): each query must surface at least one About hit.
const search = (needle: string) =>
  SEARCH_INDEX.filter((entry) =>
    (resolveEn(entry.key) ?? "").toLowerCase().includes(needle),
  );
assert.ok(
  search("theme").some((entry) => entry.section === "about"),
  'searching "theme" must reach the About section',
);
assert.ok(
  search("accent").some((entry) => entry.section === "about"),
  'searching "accent" must reach the About section',
);

// KB-144: entries whose rows render only inside Advanced's experimental
// group must carry requiresExperimental, so SettingsSearch drops them while
// experimental_enabled is off (a hit that jumps to a row that does not
// render is a dead result). Exhaustive by explicit list: the gated set is
// exactly the experimental group's rows (AdvancedSettings renders that
// whole SettingsGroup behind the flag) - a row moving in or out of the
// group fails here until its entry's gate moves with it.
const gated = SEARCH_INDEX.filter((entry) => entry.requiresExperimental);
assert.deepEqual(
  gated.map((entry) => entry.key).sort(),
  [
    "settings.advanced.groups.experimental",
    "settings.debug.keyboardImplementation.title",
    "settings.advanced.acceleration.ort.title",
    "settings.advanced.acceleration.transcribe.title",
    "settings.advanced.lazyStreamClose.label",
    "settings.advanced.vadBackend.title",
    // Companion Devices lives in the same experimental group
    // (CompanionDevices.tsx's ToggleSwitch label key) and was previously
    // missing from the index entirely.
    "settings.advanced.companionDevices.label",
  ].sort(),
  "the experimental-gated search entries must be exactly the experimental group's rows",
);

// The always-rendering Advanced siblings must NOT be gated: the post-process
// on-ramp toggle renders unconditionally (AdvancedSettings promotes it out
// of the experimental group), and the ExperimentalToggle itself must stay
// reachable by search or a default-config user could never turn the group
// on (the same reason the sidebar keeps the toggle visible).
for (const alwaysVisible of [
  "settings.debug.postProcessingToggle.label",
  "settings.advanced.experimentalToggle.label",
]) {
  const entry = SEARCH_INDEX.find((e) => e.key === alwaysVisible);
  assert.ok(entry, `${alwaysVisible} must stay indexed`);
  assert.ok(
    !entry?.requiresExperimental,
    `${alwaysVisible} renders outside the experimental group and must not be gated`,
  );
}

// The companion entry resolves to a real English label users can match.
assert.match(
  String(resolveEn("settings.advanced.companionDevices.label")),
  /companion/i,
  "settings.advanced.companionDevices.label must resolve to an English label containing 'companion'",
);

console.log("settingsSearchIndex tests passed");
