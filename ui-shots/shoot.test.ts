import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";

// AUD-08: the toast-stack-mixed scenario in the ui-shots rig still injects
// the retired command-mode-no-session event (shoot.mjs:540). Nothing in
// the frontend listens for that name anymore — the KB-038 migration moved
// command-mode-without-dictation onto the shared notice channel and the
// old dedicated listener in App.tsx was deleted — so the emitted event hits
// zero listeners and the mixed stack silently screenshots two toasts
// instead of three.
//
// The live path the rig must exercise is the one the App notice router
// consumes (App.tsx: events.overlayNoticeEvent, event name
// "overlay-notice-event", payload { kind, code, detail?, card_visible } per
// bindings.ts OverlayNoticeEvent). The router drops the event when
// card_visible is true, requires code in ROUTED_NOTICE_CODES
// (command_mode_no_session is in the set), and kind info lands on
// toast.info. The backend emits exactly that notice as
// NoticeCode::CommandModeNoSession with OverlayNoticeKind::Info
// (src-tauri/src/managers/transcription.rs), so the harness payload that
// recreates the real third toast is
//   ["overlay-notice-event",
//    { kind: "info", code: "command_mode_no_session", card_visible: false }]
// fed through the same shared window.__voxshot.emit(event, payload) loop
// the scenario array already uses — the same emission shape shoot.mjs
// itself uses for the overlay-notice-error screen.
//
// shoot.mjs is a Playwright driver with no importable seam, so this is a
// static source assertion in the repo's assert-script style (same
// technique as Footer.test.ts pinning the footer's version gate).

const harnessPath = path.join(import.meta.dirname, "shoot.mjs");
const harnessSrc = fs.readFileSync(harnessPath, "utf8");

// --- sanity preconditions (must hold before AND after the fix; if one of
// --- these fails, the rig's scenario layout drifted and this file needs
// --- updating — that is not the AUD-08 failure) ---

// The scenario under test still exists.
const scenarioIndex = harnessSrc.indexOf('"toast-stack-mixed"');
assert.ok(
  scenarioIndex !== -1,
  "toast-stack-mixed scenario must still exist in shoot.mjs",
);

// The shared toast prepare still feeds every [event, payload] pair through
// the mock's emit path; a bespoke dispatch would sidestep the mock contract.
assert.ok(
  /window\.__voxshot\.emit\(\s*event,\s*payload\s*\)/.test(harnessSrc),
  "toast scenarios must keep dispatching via window.__voxshot.emit(event, payload)",
);

// Extract the scenario's literal block by balancing brackets from the [ that
// opens the ["toast-stack-mixed", [...]] entry (none of the string literals
// inside the block contain brackets).
const openIndex = harnessSrc.lastIndexOf("[", scenarioIndex);
let depth = 0;
let closeIndex = -1;
for (let i = openIndex; i < harnessSrc.length; i += 1) {
  const ch = harnessSrc[i];
  if (ch === "[") depth += 1;
  else if (ch === "]") {
    depth -= 1;
    if (depth === 0) {
      closeIndex = i;
      break;
    }
  }
}
assert.ok(closeIndex !== -1, "toast-stack-mixed block must be balanced");
const scenarioBlock = harnessSrc.slice(openIndex, closeIndex + 1);

// The other two toasts of the mixed stack stay put: the scenario exists to
// screenshot three stacked toasts, and the fix must only re-shape the third.
assert.ok(
  scenarioBlock.includes('"recording-error"'),
  "toast-stack-mixed must keep its recording-error toast",
);
assert.ok(
  scenarioBlock.includes('"post-process-skip-event"'),
  "toast-stack-mixed must keep its post-process-skip toast",
);

// --- the AUD-08 assertions (both fail against today's harness) ---

// 1) The retired event name is gone from the rig entirely: no listener for
//    command-mode-no-session exists in the frontend since KB-038, so any
//    emission of it is a dead event the scenario mistakes for a toast.
assert.ok(
  !harnessSrc.includes("command-mode-no-session"),
  "AUD-08: shoot.mjs still injects the retired command-mode-no-session event; nothing listens for that name since KB-038 moved it onto overlay-notice-event, so the toast-stack-mixed screenshot silently loses its third toast",
);

// 2) The third toast rides the notice channel in the shape the App router
//    consumes: event name overlay-notice-event with a flat payload object
//    (key-order agnostic) carrying the three fields that make the toast
//    actually render —
//      code command_mode_no_session  → in ROUTED_NOTICE_CODES
//      kind info                     → toast.info
//      card_visible false            → router does not early-return
const noticeEntry = scenarioBlock.match(
  /\[\s*["']overlay-notice-event["']\s*,\s*\{[^{}]*\}\s*\]/,
);
assert.ok(
  noticeEntry !== null,
  'AUD-08: the third toast of toast-stack-mixed must be injected as ["overlay-notice-event", { kind: "info", code: "command_mode_no_session", card_visible: false }] so the App notice router renders it',
);
assert.match(
  noticeEntry[0],
  /code\s*:\s*["']command_mode_no_session["']/,
  "the notice payload must carry code command_mode_no_session",
);
assert.match(
  noticeEntry[0],
  /kind\s*:\s*["']info["']/,
  "the notice payload must carry kind info (the backend tone for NoticeCode::CommandModeNoSession; error would land on toast.error)",
);
assert.match(
  noticeEntry[0],
  /card_visible\s*:\s*false/,
  "the notice payload must carry card_visible false (a true value makes the router drop the event before toasting)",
);

console.log("shoot.mjs toast-stack-mixed scenario tests passed");
