import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

// AUD-14: comment-accuracy bundle. Four comments written by the audited
// commits describe behavior the code does not have. These are static source
// assertions (the test reads the four files and pins the corrected wording);
// they cannot execute the Rust paths, so they pin text, not behavior.
//
// Ground truth verified in this repo:
// 1. src-tauri/src/lib.rs (RunEvent::Exit arm, ~1207-1211): the KB-151
//    comment says quitting must not strand the macOS system INPUT muted,
//    but mute_while_recording mutes system OUTPUT — managers/audio.rs
//    set_mute on macOS runs `osascript -e "set volume output muted ..."`
//    (audio.rs:135-141) and get_mute's doc calls it "the system output
//    mute state" (audio.rs:143-148).
// 2. src-tauri/src/managers/transcription.rs:576-578 (emit_overlay_notice
//    probe): the comment says the probe makes "the same overlay_style
//    lookup the overlay show and hide paths make", but the hide path hides
//    unconditionally — overlay.rs:723-725 hide_recording_overlay: "Always
//    hide the overlay regardless of settings", no overlay_style read. Only
//    the show paths do the lookup (overlay.rs:494-498, 645-649).
// 3. src-tauri/src/companion/mod.rs:414-420: apply_enabled carries two
//    stacked rustdoc blocks — the old void-era one ("Apply a
//    `companion_devices_enabled` change: start/stop the server with side
//    effects. Called from the settings command and at startup.") directly
//    above the current one ("Apply the companion enabled state ...").
// 4. src/lib/noticeMessage.ts:11-13: the NoticeTranslateFn comment cites
//    non-React callers "(updaterFlow)" passing their own t, but
//    updaterFlow (src/components/update-checker/updaterFlow.ts) never
//    imports noticeMessage; the only importers are App.tsx and
//    RecordingOverlay.tsx, both React components.
//
// Contract for the fix, pinned by the assertions below (correct the
// comments; no behavior changes — deleting a comment wholesale instead of
// correcting it keeps this test red on the "corrected present" half):
// 1. The Exit-arm comment no longer says the system input is muted and
//    does say the restore covers the system output.
// 2. The probe comment no longer claims the hide path makes the
//    overlay_style lookup and attributes it to the show path(s) only.
// 3. The old void-era rustdoc sentence is gone; the current block
//    ("Apply the companion enabled state ...") remains, exactly once.
// 4. noticeMessage.ts no longer names updaterFlow; the type comment keeps
//    its truthful core (the react-i18next useTranslation function).

const here = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.join(here, "..", "..");
const read = (rel: string): string =>
  fs.readFileSync(path.join(repoRoot, rel), "utf8");

// Collapse all whitespace (incl. the "// " line-wrap seams of comments) so
// multi-line comment phrasings match regardless of where they wrap.
const squash = (s: string): string => s.replace(/\s+/g, " ");

// --- 1/4: lib.rs Exit-arm mute-restore comment -------------------------------
{
  const src = read("src-tauri/src/lib.rs");
  const armStart = src.indexOf("tauri::RunEvent::Exit => {");
  assert.notEqual(
    armStart,
    -1,
    "AUD-14 setup: RunEvent::Exit arm not found in lib.rs",
  );
  const armEnd = src.indexOf("companion::shutdown(app);", armStart);
  assert.notEqual(
    armEnd,
    -1,
    "AUD-14 setup: companion::shutdown anchor not found in lib.rs Exit arm",
  );
  const arm = squash(src.slice(armStart, armEnd));

  assert.ok(
    !arm.includes("system input"),
    "AUD-14 (1/4) lib.rs: the exit-restore comment still claims quitting strands the macOS system INPUT muted, but mute_while_recording mutes system OUTPUT (managers/audio.rs set_mute -> `set volume output muted`)",
  );
  assert.ok(
    arm.includes("output"),
    "AUD-14 (1/4) lib.rs: the corrected exit-restore comment must say the mute restore covers the system OUTPUT",
  );
}

// --- 2/4: transcription.rs emit_overlay_notice probe comment ------------------
{
  const src = read("src-tauri/src/managers/transcription.rs");
  const fnStart = src.indexOf("pub fn emit_overlay_notice");
  assert.notEqual(
    fnStart,
    -1,
    "AUD-14 setup: emit_overlay_notice not found in managers/transcription.rs",
  );
  const fnEnd = src.indexOf("\n}", fnStart);
  assert.notEqual(
    fnEnd,
    -1,
    "AUD-14 setup: end of emit_overlay_notice not found",
  );
  const body = squash(src.slice(fnStart, fnEnd));

  assert.ok(
    !body.includes("show and hide paths"),
    'AUD-14 (2/4) transcription.rs: the probe comment must never claim the hide path makes an overlay_style lookup (overlay.rs hide_recording_overlay hides unconditionally)',
  );
  assert.ok(
    body.includes("overlay_enabled") && body.includes("atomic"),
    "AUD-14 (2/4) transcription.rs: the probe comment must describe the cached overlay_enabled atomic (no per-notice get_settings deserialize) and its live is_visible half",
  );
}

// --- 3/4: companion/mod.rs stacked rustdoc on apply_enabled -------------------
{
  const src = read("src-tauri/src/companion/mod.rs");
  const fnIdx = src.indexOf("pub fn apply_enabled");
  assert.notEqual(
    fnIdx,
    -1,
    "AUD-14 setup: apply_enabled not found in companion/mod.rs",
  );
  const head = src.slice(0, fnIdx);
  const prevClose = head.lastIndexOf("\n}\n");
  assert.notEqual(
    prevClose,
    -1,
    "AUD-14 setup: preceding item end not found before apply_enabled",
  );
  // The doc-comment lines sit between the previous top-level item and the fn.
  const doc = squash(src.slice(prevClose, fnIdx));

  assert.ok(
    !doc.includes("Apply a `companion_devices_enabled` change"),
    'AUD-14 (3/4) companion/mod.rs: the old void-era rustdoc block ("Apply a `companion_devices_enabled` change: start/stop the server with side effects...") is still stacked on apply_enabled',
  );
  assert.ok(
    doc.includes("Apply the companion enabled state"),
    'AUD-14 (3/4) companion/mod.rs: the current rustdoc block ("Apply the companion enabled state ...") must remain as apply_enabled\'s single doc block',
  );
}

// --- 4/4: noticeMessage.ts updaterFlow citation -------------------------------
{
  const src = read("src/lib/noticeMessage.ts");

  assert.ok(
    !src.includes("updaterFlow"),
    "AUD-14 (4/4) noticeMessage.ts: the NoticeTranslateFn comment still cites updaterFlow as a caller that can pass its own t, but updaterFlow never imports noticeMessage (only App.tsx and RecordingOverlay.tsx do)",
  );
  assert.ok(
    src.includes("useTranslation"),
    "AUD-14 (4/4) noticeMessage.ts: the type comment must keep its truthful core — it describes react-i18next's useTranslation translate function",
  );
}

console.log("AUD-14 comment-accuracy tests passed");
