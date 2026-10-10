import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";

// AUD-07 / KB-193 (Footer): the footer's version state starts as "" and the
// real value arrives over an async IPC roundtrip (fetchAppVersion in the
// mount effect), so for at least one rendered frame `version` is "". An
// ungated `<span>v{version}</span>` therefore flashes a bare "v" — the
// exact bug AboutSettings already fixed by rendering the span only once the
// version has actually loaded (AboutSettings: `{version !== "" && (...)}`).
//
// Footer is a Tauri window component with no React render harness in this
// repo, so this is a static source assertion in the repo's assert-script
// style (same technique as localLlmRouting.test.ts pinning the backend
// constant): it reads Footer.tsx and asserts the version span is wrapped
// in a non-empty version guard.

const footerPath = path.join(import.meta.dirname, "Footer.tsx");
const footerSrc = fs.readFileSync(footerPath, "utf8");

// The version span itself must still exist. If this fails, the footer
// stopped rendering v{version} at all and this file needs updating — it is
// not the AUD-07 failure.
assert.ok(
  /<span[^>]*>\s*v\{version\}\s*<\/span>/.test(footerSrc),
  "Footer must keep rendering the version as v{version} inside a span",
);

// The flash window is real: the version state must start empty. This pins
// the precondition that makes the guard mandatory (a non-empty initial
// state would hide the bug instead of fixing it).
assert.ok(
  /useState\(\s*["']{2}\s*\)/.test(footerSrc),
  "Footer's version state must start empty (the IPC roundtrip window the guard covers)",
);

// The contract under test: the v{version} span renders only under a
// non-empty guard, exactly like the AboutSettings gate. Accepted guard
// forms (all semantically "version is non-empty"):
//   version !== ""  |  "" !== version  |  version.length > 0
//   0 < version.length  |  truthy {version && ...}
const GUARD = [
  String.raw`version\s*!==\s*["']["']`,
  String.raw`["']["']\s*!==\s*version`,
  String.raw`version\.length\s*>\s*0`,
  String.raw`0\s*<\s*version\.length`,
  String.raw`version`,
].join("|");
const guardedVersionSpan = new RegExp(
  String.raw`\{\s*(?:` +
    GUARD +
    String.raw`)\s*&&\s*\(?\s*` +
    String.raw`[\s\S]*?` +
    String.raw`<span[^>]*>\s*v\{version\}\s*<\/span>\s*\)?\s*\}`,
);

// Sanity: the pattern must be satisfiable — the already-fixed
// AboutSettings row has to match it. If this fails, the regex (not the
// footer) drifted and this file needs updating.
const aboutSrc = fs.readFileSync(
  path.join(import.meta.dirname, "..", "settings", "about", "AboutSettings.tsx"),
  "utf8",
);
assert.ok(
  guardedVersionSpan.test(aboutSrc),
  "guard pattern must match the already-fixed AboutSettings gate (regex drift check)",
);

// The AUD-07 assertion itself: this is what fails against the ungated
// footer.
assert.ok(
  guardedVersionSpan.test(footerSrc),
  'AUD-07: Footer renders v{version} ungated — during the version IPC roundtrip the footer flashes a bare "v"; gate the span exactly like AboutSettings: {version !== "" && (<span>v{version}</span>)}',
);

console.log("Footer version-gate tests passed");
