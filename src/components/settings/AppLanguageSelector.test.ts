import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";

// KB-192: the app language selector persists before the webview switches.
// The old order called i18n.changeLanguage first, so a failed backend write
// left the webview in the new language while the store (and the tray, which
// rebuilds from the persisted setting) stayed in the old one until relaunch.
// The fixed shape awaits updateSetting("app_language"), reads success off
// the post-write store state, and only then switches i18n.
//
// AppLanguageSelector is a Tauri settings component with no React render
// harness in this repo, so this is a static source assertion in the repo's
// assert-script style (same technique as Footer.test.ts).

const selectorSrc = fs.readFileSync(
  path.join(import.meta.dirname, "AppLanguageSelector.tsx"),
  "utf8",
);

// The persist-before-apply shape: await the write, then gate the language
// switch on the post-write store confirming the value actually landed.
const persistFirst =
  /await updateSetting\(\s*"app_language",\s*langCode\s*\);\s*if \(useSettingsStore\.getState\(\)\.settings\?\.app_language === langCode\) \{\s*i18n\.changeLanguage\(langCode\);\s*\}/;
assert.ok(
  persistFirst.test(selectorSrc),
  'KB-192: the selector must await updateSetting("app_language") and call i18n.changeLanguage only inside the post-write store check',
);

// No ungated twin: exactly one changeLanguage call site exists, so the
// switch can never fire ahead of (or besides) the confirmed persist.
const changeLanguageCalls = (selectorSrc.match(/i18n\.changeLanguage\(/g) ?? [])
  .length;
assert.equal(
  changeLanguageCalls,
  1,
  `KB-192: i18n.changeLanguage must have exactly one call site, inside the store check (found ${changeLanguageCalls})`,
);

console.log("AppLanguageSelector: all assertions passed");
