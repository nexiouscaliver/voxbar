import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";

// KB-162: the show-overlay listener resets transient card state before the
// overlay paints. The reset used to clear the notice unconditionally, so
// opening the PREVIEW cut off the session's tail notice (e.g. a post-process
// skip shown moments earlier) exactly while the user reads the previewed
// text. The preview keeps the notice and its dismiss timer; fresh sessions
// (recording / streaming) still start notice-clean, and hide-overlay still
// clears it.
//
// RecordingOverlay is a Tauri window component with no React render harness
// in this repo, so this is a static source assertion in the repo's
// assert-script style (same technique as Footer.test.ts).

const overlaySrc = fs.readFileSync(
  path.join(import.meta.dirname, "RecordingOverlay.tsx"),
  "utf8",
);

// The contract: the notice clear inside the show-listener reset is gated on
// the state NOT being the preview.
const gatedNoticeClear =
  /if \(overlayState !== "preview"\) \{\s*setNotice\(null\);\s*\}/;
assert.ok(
  gatedNoticeClear.test(overlaySrc),
  'KB-162: the show-listener reset must skip setNotice(null) for the preview: if (overlayState !== "preview") { setNotice(null); }',
);

// The notice's dismiss timer rides the same rule: the preview keeps a
// still-running timer instead of cancelling it out from under the notice it
// just chose to keep.
const gatedTimerClear =
  /if \(overlayState !== "preview" && noticeTimerRef\.current !== null\) \{/;
assert.ok(
  gatedTimerClear.test(overlaySrc),
  'KB-162: the notice timer clear must ride the same preview guard (overlayState !== "preview")',
);

// The rest of the contract stays intact: the gate above already clears for
// fresh sessions (recording / streaming fail the !== "preview" check), and
// hide-overlay must keep clearing the notice unconditionally.
const hideClears = /listen\(\s*"hide-overlay"[\s\S]{0,600}?setNotice\(null\);/;
assert.ok(
  hideClears.test(overlaySrc),
  "KB-162: hide-overlay must still clear the notice; only the preview keeps it",
);

console.log("overlayPreviewNotice: all assertions passed");
