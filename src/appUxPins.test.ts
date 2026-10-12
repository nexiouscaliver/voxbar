import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";

/* Source pins for the v1.6.2 UX-polish wave items whose behavior lives in
 * App.tsx (a Tauri window component with no React render harness in this
 * repo) or in plain CSS. Static source assertions in the repo's
 * assert-script style (same technique as Footer.test.ts and
 * AccessibilityOnboarding.test.ts).
 *
 * Covers KB-087 (the tray copy outcome listener), KB-088 (the debug chord
 * auto-repeat guard), KB-093 (the settings tree ErrorBoundary) and KB-090
 * (the app-theme-aware dark variant).
 */

const here = import.meta.dirname;
const appSrc = fs.readFileSync(path.join(here, "App.tsx"), "utf8");
const appCss = fs.readFileSync(path.join(here, "App.css"), "utf8");

/* ---- KB-087: the tray copy outcome listener ---- */

// App.tsx listens for the Rust event and posts the OS notification through
// the routing decision, so the notifications master toggle governs it.
// Fleet pass: the payload also carries the REASON, so "nothing to copy"
// and "the clipboard write failed" render as different messages, and the
// failure legs write the in-app notification log row while the master
// toggle is on - the backstop that survives a denied macOS notification
// permission or a hidden window (the historyDbRecovery discipline),
// because notifyDesktop alone silently no-ops on exactly that machine.
const trayEventAt = appSrc.indexOf('"tray-copy-outcome"');
assert.ok(
  trayEventAt >= 0,
  'KB-087: App.tsx must listen for the "tray-copy-outcome" event',
);
const trayListenerAt = appSrc.lastIndexOf("listen<{", trayEventAt);
assert.ok(trayListenerAt >= 0, "KB-087: the listener call must be findable");
const trayListener = appSrc.slice(
  trayListenerAt,
  appSrc.indexOf("}, [t]);", trayEventAt),
);
assert.ok(
  trayListener.includes('reason?: "copied" | "empty" | "failed"'),
  "KB-087: the listener must read the outcome's reason from the payload",
);
assert.ok(
  trayListener.includes("trayCopyOutcomeDispatch(prefs)"),
  "KB-087: the tray copy notification must respect the notifications toggle",
);
assert.ok(
  trayListener.includes("notifyDesktop"),
  "KB-087: the outcome posts through notifyDesktop (live visibility gate)",
);
assert.ok(
  trayListener.includes('t("trayCopyOutcome.copied")') &&
    trayListener.includes('t("trayCopyOutcome.failed")') &&
    trayListener.includes('t("trayCopyOutcome.empty")'),
  "KB-087: all three outcome bodies come from the locale keys",
);
assert.ok(
  trayListener.includes("pushNotificationLogEntry") &&
    trayListener.includes("prefs.enabled"),
  "KB-087: the failure legs must write the in-app notification log row (the denied-permission backstop)",
);

// The empty key ships in every locale (the copied/failed pair already was).
const localesDir = path.join(here, "i18n/locales");
const locales = fs
  .readdirSync(localesDir)
  .filter((entry) => fs.statSync(path.join(localesDir, entry)).isDirectory());
assert.ok(
  locales.length >= 26,
  `expected at least 26 locales, found ${locales.length}`,
);
for (const locale of locales) {
  const bundle = JSON.parse(
    fs.readFileSync(path.join(localesDir, locale, "translation.json"), "utf8"),
  ) as Record<string, Record<string, string>>;
  const block = bundle.trayCopyOutcome;
  assert.ok(block, `${locale}: trayCopyOutcome block must exist`);
  for (const key of ["copied", "empty", "failed"]) {
    assert.ok(
      typeof block[key] === "string" && block[key].length > 0,
      `${locale}: trayCopyOutcome.${key} must be a non-empty string`,
    );
  }
}

/* ---- KB-088: the debug chord auto-repeat guard ---- */

const debugChordAt = appSrc.indexOf(
  "Handle keyboard shortcuts for debug mode toggle",
);
assert.ok(debugChordAt >= 0, "the debug chord handler must exist");
const debugChord = appSrc.slice(
  debugChordAt,
  appSrc.indexOf("}, [settings?.debug_mode", debugChordAt),
);
assert.ok(
  debugChord.includes("if (event.repeat) return;"),
  "KB-088: a held chord auto-repeat must not re-toggle debug mode",
);

/* ---- KB-093: the settings tree ErrorBoundary ---- */

// The settings scroll content (the per-section trees, the search, the
// banners) is wrapped, so one render throw retires the pane, not the app.
const scrollAt = appSrc.indexOf("ref={settingsScrollRef}");
assert.ok(scrollAt >= 0, "the settings scroll container must exist");
const scrollRegion = appSrc.slice(
  scrollAt,
  appSrc.indexOf("</ErrorBoundary>", scrollAt) + 1,
);
assert.ok(
  scrollRegion.includes("<ErrorBoundary context="),
  "KB-093: the settings scroll content must sit inside an ErrorBoundary",
);
assert.ok(
  scrollRegion.includes("<SettingsSearch") &&
    scrollRegion.includes("renderSettingsContent"),
  "KB-093: the boundary must wrap the section content itself",
);

/* ---- KB-090: the app-theme-aware dark variant ---- */

// App.css redefines dark: to follow the app theme (data-theme forced, else
// the OS media query without a light override) - the same resolution the
// color tokens in theme.css perform. The default variant keys on the OS
// alone, which is exactly the reported bug (debug tables kept the OS
// palette after the user forced the other theme).
assert.ok(
  appCss.includes("@custom-variant dark"),
  "KB-090: App.css must redefine the dark variant",
);
const variantAt = appCss.indexOf("@custom-variant dark");
const firstSlot = appCss.indexOf("@slot;", variantAt);
const secondSlot = appCss.indexOf("@slot;", firstSlot + 1);
const variantBlock = appCss.slice(
  variantAt,
  appCss.indexOf("}", secondSlot) + 1,
);
assert.ok(
  variantBlock.includes('&:where([data-theme="dark"], [data-theme="dark"] *)'),
  'KB-090: the variant must fire for a forced data-theme="dark"',
);
assert.ok(
  variantBlock.includes("@media (prefers-color-scheme: dark)") &&
    variantBlock.includes(':not([data-theme="light"]'),
  'KB-090: the OS leg must not fire once data-theme="light" is forced',
);
// The log console surface override honors the same resolution (the old
// media-query-only override left the console dark under a forced-light
// app, or light under a forced-dark one).
assert.ok(
  appCss.includes(':root:not([data-theme="light"])') &&
    appCss.includes(':root[data-theme="dark"]'),
  "KB-090: the log surface override must honor the forced theme",
);

console.log("appUxPins tests passed");
