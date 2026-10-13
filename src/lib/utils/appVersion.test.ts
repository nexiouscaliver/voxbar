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

// Fleet pass 2 (the v1.6.2 version bump pin): a bump must be COMPLETE or
// the What's New modal can never show it. findReleaseNoteToShow requires a
// note strictly newer than whats_new_last_seen_version and not newer than
// the running app version, so a branch that fixes things but never bumps
// (or bumps but ships no note) is invisible to every upgrader - exactly
// what happened to the whole v1.6.2 backlog line. All four version
// locations must agree, and a release note for the shipping version must
// exist.
const root = path.join(import.meta.dirname, "../../..");
const tauriConf = JSON.parse(
  fs.readFileSync(path.join(root, "src-tauri/tauri.conf.json"), "utf8"),
) as { version: string };
assert.equal(
  tauriConf.version,
  pkg.version,
  `tauri.conf.json's version (${tauriConf.version}) must match package.json's (${pkg.version})`,
);
const cargo = fs.readFileSync(path.join(root, "src-tauri/Cargo.toml"), "utf8");
const cargoLines = cargo.split("\n");
const packageIdx = cargoLines.findIndex((l) => l.trim() === "[package]");
assert.ok(packageIdx >= 0, "Cargo.toml must have a [package] section");
const cargoVersion = cargoLines
  .slice(packageIdx, packageIdx + 6)
  .find((l) => l.startsWith("version ="));
assert.equal(
  cargoVersion,
  `version = "${pkg.version}"`,
  `Cargo.toml's package version (${cargoVersion}) must match package.json's (${pkg.version})`,
);
const releaseNote = path.join(
  root,
  "src/content/release-notes",
  `${pkg.version}.md`,
);
assert.ok(
  fs.existsSync(releaseNote),
  `a release note for the shipping version must exist at src/content/release-notes/${pkg.version}.md, or the What's New modal can never show this version's changes`,
);

console.log("appVersion tests passed");
