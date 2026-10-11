import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";

// KB-160: closing the settings window HIDES it (prevent_close + hide), so an
// armed shortcut recorder never sees its click-outside cancel - every global
// binding stays suspended (suspendAllBindings ran when the recorder armed)
// with zero feedback until the window is reopened and clicked. Both
// recorders therefore listen for the backend's main-window-hidden event and
// run the same cancelRecording path the click-outside uses, releasing the
// suspension and discarding the pending chord even though the component
// stays mounted.
//
// The recorders are Tauri window components with no React render harness in
// this repo, so this is a static source assertion in the repo's assert-script
// style (same technique as Footer.test.ts): it reads both recorder sources
// and the backend emit, and pins the listener wiring.

const settingsDir = import.meta.dirname;

const globalSrc = fs.readFileSync(
  path.join(settingsDir, "GlobalShortcutInput.tsx"),
  "utf8",
);
const handySrc = fs.readFileSync(
  path.join(settingsDir, "HandyKeysShortcutInput.tsx"),
  "utf8",
);

// The listener call itself, shared shape by both recorders: the event name
// must match the backend emit exactly, and the handler must be the cancel
// path (the one that resumes the suspended bindings), not a log no-op.
const windowHideCancel =
  /listen\(\s*"main-window-hidden"\s*,\s*\(\)\s*=>\s*cancelRecording\(\)\s*\)/;

assert.ok(
  windowHideCancel.test(globalSrc),
  'KB-160: GlobalShortcutInput must cancel the armed recorder on main-window-hidden: listen("main-window-hidden", () => cancelRecording())',
);
assert.ok(
  windowHideCancel.test(handySrc),
  'KB-160: HandyKeysShortcutInput must cancel the armed recorder on main-window-hidden: listen("main-window-hidden", () => cancelRecording())',
);

// The listener arms only while a recording is actually live:
// GlobalShortcutInput while a row's recorder is armed (editingShortcutId
// set), so an idle settings page keeps no event listener registered...
const armedGlobal =
  /if \(editingShortcutId === null\) return;[\s\S]{0,300}?listen\(\s*"main-window-hidden"/;
assert.ok(
  armedGlobal.test(globalSrc),
  "KB-160: the GlobalShortcutInput listener must sit inside the armed-only effect, guarded by editingShortcutId",
);

// ...and HandyKeysShortcutInput while its single recorder is live.
const armedHandy =
  /if \(!isRecording\) return;[\s\S]{0,300}?listen\(\s*"main-window-hidden"/;
assert.ok(
  armedHandy.test(handySrc),
  "KB-160: the HandyKeysShortcutInput listener must sit inside the recording-only effect, guarded by isRecording",
);

// The cross-language half (mirrors the modelRefusals.test.ts backend sync
// pin): the backend emits main-window-hidden from exactly ONE place, the
// single window-hide path. A second emit (the merge regression the audit
// found) would cancel a freshly armed recorder mid-keystroke.
const libSrc = fs.readFileSync(
  path.join(settingsDir, "..", "..", "..", "src-tauri", "src", "lib.rs"),
  "utf8",
);
const emitCount = (libSrc.match(/emit\(\s*"main-window-hidden"/g) ?? []).length;
assert.equal(
  emitCount,
  1,
  `KB-160: lib.rs must emit main-window-hidden from exactly one hide path (found ${emitCount})`,
);

console.log("shortcutRecorderWindowHide: all assertions passed");
