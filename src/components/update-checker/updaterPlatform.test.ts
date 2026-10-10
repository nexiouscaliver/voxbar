import assert from "node:assert/strict";
import { updaterAutoUpdateSupported } from "./updaterPlatform";

// macOS is the only platform with signed updater artifacts today
// (scripts/release-macos.sh publishes the darwin entries in latest.json).
assert.equal(updaterAutoUpdateSupported("macos"), true);

// Windows and Linux build installers (msi / deb / rpm / AppImage) but no
// updater payloads ship for them yet, so a check there can only fail.
assert.equal(updaterAutoUpdateSupported("windows"), false);
assert.equal(updaterAutoUpdateSupported("linux"), false);

// Unknown or non-desktop platforms must fail closed.
assert.equal(updaterAutoUpdateSupported("android"), false);
assert.equal(updaterAutoUpdateSupported("ios"), false);
assert.equal(updaterAutoUpdateSupported("fuchsia"), false);
assert.equal(updaterAutoUpdateSupported(""), false);

console.log("updaterPlatform: all assertions passed");
