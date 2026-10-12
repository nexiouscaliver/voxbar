import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";

// KB-096: the version fallback can never drift from the shipping version.
// The footer and About surfaces show FALLBACK_APP_VERSION whenever the
// Tauri app handle is unavailable (tests, plain-browser preview), and it
// hardcoded "0.1.2" for the whole 1.x line. Now vite injects the real
// package version at build time (__APP_VERSION__, defined in
// vite.config.ts) and the literal below only covers non-vite execution -
// this pin fails a version bump that forgets the literal.
//
// appVersion.ts imports @tauri-apps/api/app at module scope, so a bun
// test that executed it would drag the Tauri runtime shim in; this is a
// static source assertion instead (the repo's assert-script style, same
// technique as Footer.test.ts pinning the version guard).

const modulePath = path.join(import.meta.dirname, "appVersion.ts");
const source = fs.readFileSync(modulePath, "utf8");
const pkgPath = path.join(import.meta.dirname, "../../../package.json");
const pkg = JSON.parse(fs.readFileSync(pkgPath, "utf8")) as {
  version: string;
};

// The module reads the injected define via typeof (no ReferenceError in
// non-vite contexts) and falls back to a literal.
assert.ok(
  /typeof __APP_VERSION__ === "string"/.test(source),
  "appVersion must resolve the injected __APP_VERSION__ via typeof, never a bare reference",
);
const literal = source.match(/: "([^"]+)";/);
assert.ok(literal, "appVersion must keep a synced literal fallback");
assert.equal(
  literal[1],
  pkg.version,
  `the fallback literal (${literal[1]}) must match package.json's version (${pkg.version})`,
);

// The injection itself exists at the build boundary.
const vitePath = path.join(import.meta.dirname, "../../../vite.config.ts");
const vite = fs.readFileSync(vitePath, "utf8");
assert.ok(
  vite.includes("__APP_VERSION__") && vite.includes("pkg.version"),
  "vite.config.ts must define __APP_VERSION__ from package.json's version",
);

console.log("appVersion tests passed");
