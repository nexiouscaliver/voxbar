import assert from "node:assert/strict";
import { noticeMessage } from "./noticeMessage";

// AUD-02 red test: pin the pure notice-router decision table that must be
// extracted out of App.tsx into src/lib/noticeRouting.ts.
//
// Today the router (App.tsx:400-423) inlines its policy with no seam:
//   - App.tsx:403  `if (card_visible) return;`
//   - App.tsx:404  `if (!ROUTED_NOTICE_CODES.has(code)) return;`
//   - App.tsx:416  notify only when `document.visibilityState !== "visible"`
// driven by the hand-written ROUTED_NOTICE_CODES table (App.tsx:77-90).
// That combination:
//   1. leaves the three orphaned Info codes - model_fallback,
//      post_process_download_missing, post_process_too_long - with NO
//      surface when the card AND the window are both hidden: they are not in
//      ROUTED_NOTICE_CODES, their Info tone plays no sound
//      (transcription.rs:479-492), and their legacy toasts expire unseen in
//      the hidden window;
//   2. dual-toasts companion_server_failed on top of the settings rollback
//      toast (settingsStore.ts:418) whenever the companion toggle fails;
//   3. drops command_mode_no_session during the Processing window
//      (card_visible=true), losing the always-fired main-window toast it
//      had before the KB-038 migration (transcription_coordinator.rs:1005);
//   4. never notifies companion_session_capped while the card shows it, even
//      though the cap notice is emitted AFTER finalize_companion_session
//      (server.rs:401 runs before the cap emit at server.rs:413) and the
//      finalize pipeline can overwrite the card's notice row
//      (RecordingOverlay.tsx:266-274);
//   5. keeps two hand tables (ROUTED_NOTICE_CODES vs noticeMessage's arms)
//      with nothing cross-pinning them.
//
// The fix extracts a pure decision module; this file is its contract. It is
// red until src/lib/noticeRouting.ts exists and green only when the whole
// decision table below holds. Rate limiting (App.tsx:407-409), the
// noticeMessage null check (App.tsx:405-406) and toast durations
// (App.tsx:410-413) stay in the component: the pure decision is only about
// WHICH surfaces (main-window toast, macOS notification) a code may use.

// ---- Contract the extracted module must satisfy (src/lib/noticeRouting.ts)
//
// export type NoticeRoutingDecision = { toast: boolean; notify: boolean };
//
// routerDecision(code, cardVisible, windowHidden) is pure and synchronous.
// `cardVisible` is the event payload's card_visible (could the overlay card
// render this notice at emit time, transcription.rs:387-392); `windowHidden`
// is the main window's hidden state (today derived from
// document.visibilityState !== "visible", App.tsx:416). Codes outside the
// notice universe are inert ({toast:false, notify:false} in every state),
// matching today's silent skip at App.tsx:404.
//
// Exports:
//   routerDecision(code, cardVisible, windowHidden) -> NoticeRoutingDecision
//   ROUTED_NOTICE_CODES: Set<string>    - every code the router owns
//   NOTICE_ROUTE_POLICIES: Record<string, string> - policy per code, one key
//     for every code noticeMessage has an arm for (the cross-pinned table)

interface NoticeRoutingDecision {
  toast: boolean;
  notify: boolean;
}

interface NoticeRoutingModule {
  routerDecision: (
    code: string,
    cardVisible: boolean,
    windowHidden: boolean,
  ) => NoticeRoutingDecision;
  ROUTED_NOTICE_CODES: Set<string>;
  NOTICE_ROUTE_POLICIES: Record<string, string>;
}

// Loaded dynamically so the red state (module not yet extracted) surfaces as
// a clear assertion message instead of a bare resolver crash.
let routing: NoticeRoutingModule | null = null;
let loadError = "";
try {
  routing = (await import("./noticeRouting")) as unknown as NoticeRoutingModule;
} catch (error) {
  loadError = error instanceof Error ? error.message : String(error);
}
if (routing === null) {
  assert.fail(
    "AUD-02 red: src/lib/noticeRouting.ts does not exist yet - extract the pure routerDecision + code tables out of App.tsx (import failed: " +
      loadError +
      ")",
  );
}
const { routerDecision, ROUTED_NOTICE_CODES, NOTICE_ROUTE_POLICIES } = routing;

// A translate fn that echoes the key (same trick as noticeMessage.test.ts);
// here only non-null returns matter, pinning that an arm exists per code.
const t = (key: string): string => key;

// The notice-code universe: one string per NoticeCode variant, mirroring
// NoticeCode::as_str (transcription.rs:498-529). noticeMessage.test.ts pins
// these same 28 codes arm-for-arm, so this list == noticeMessage's arm set.
const ALL_NOTICE_CODES = [
  "no_model_selected",
  "microphone_permission_denied",
  "no_input_device",
  "recording_failed",
  "transcription_failed",
  "paste_failed",
  "model_load_failed",
  "model_fallback",
  "post_process_memory_gate",
  "post_process_download_missing",
  "post_process_engine_failed",
  "post_process_timeout",
  "post_process_length_guard",
  "post_process_too_long",
  "post_process_output_invalid",
  "post_process_cloud_failed",
  "delete_last_word_no_session",
  "delete_last_word_no_buffer",
  "undo_no_session",
  "undo_no_buffer",
  "binding_busy",
  "wayland_tauri_hotkeys",
  "gnome_overlay_fallback",
  "post_process_prompt_cycled",
  "companion_disconnected_finalized",
  "companion_server_failed",
  "companion_session_capped",
  "command_mode_no_session",
] as const;

// The three orphaned Info codes (AUD-02 symptom 1) plus the dual-toasting
// companion server failure (symptom 2): legacy surfaces own the visible
// window, so the router acts only while the window is hidden.
const ORPHANED_INFO = [
  "model_fallback",
  "post_process_download_missing",
  "post_process_too_long",
] as const;
const LEGACY_SURFACE = [...ORPHANED_INFO, "companion_server_failed"];

// The eight no-op codes App.tsx already routes (key feedback + Linux setup
// warnings) plus companion_disconnected_finalized: keep today's behavior.
const CARD_GATED = [
  "delete_last_word_no_session",
  "delete_last_word_no_buffer",
  "undo_no_session",
  "undo_no_buffer",
  "binding_busy",
  "post_process_prompt_cycled",
  "wayland_tauri_hotkeys",
  "gnome_overlay_fallback",
  "companion_disconnected_finalized",
];

const ALWAYS_TOAST = ["command_mode_no_session"];
const CAP = ["companion_session_capped"];

// Codes whose dedicated legacy listeners own every main-window surface; the
// router stays inert for them (today's App.tsx:404 behavior, kept as-is).
const UNROUTED = [
  "no_model_selected",
  "microphone_permission_denied",
  "no_input_device",
  "recording_failed",
  "transcription_failed",
  "paste_failed",
  "model_load_failed",
  "post_process_memory_gate",
  "post_process_engine_failed",
  "post_process_timeout",
  "post_process_length_guard",
  "post_process_output_invalid",
  "post_process_cloud_failed",
];

// Test self-check: the policy groups partition the notice universe exactly.
const grouped = [
  ...CARD_GATED,
  ...LEGACY_SURFACE,
  ...ALWAYS_TOAST,
  ...CAP,
  ...UNROUTED,
];
assert.deepEqual(
  [...grouped].sort(),
  [...ALL_NOTICE_CODES].sort(),
  "test self-check: the policy groups must cover the whole notice universe",
);
assert.equal(
  new Set(grouped).size,
  ALL_NOTICE_CODES.length,
  "test self-check: no notice code may sit in two policy groups",
);

// ---- The four AUD-02 symptoms, stated as decisions (the exhaustive table
// below covers every cell; these make each regression legible).

// Symptom 1: the orphaned Info codes gain a surface when card AND window are
// both hidden - today App.tsx:404 drops them before any surface can fire.
for (const code of ORPHANED_INFO) {
  const d = routerDecision(code, false, true);
  assert.equal(
    d.toast,
    true,
    `${code} must toast when card and window are both hidden (today: not in ROUTED_NOTICE_CODES, App.tsx:77-90/404, so no surface at all)`,
  );
  assert.equal(
    d.notify,
    true,
    `${code} must notify when card and window are both hidden (Info tone plays no sound, transcription.rs:479-492; the legacy toast expires unseen)`,
  );
}

// Symptom 2: no dual toast - while the window is visible the settings
// rollback toast (settingsStore.ts:418) alone owns companion_server_failed.
const companionVisibleWindow = routerDecision(
  "companion_server_failed",
  false,
  false,
);
assert.equal(
  companionVisibleWindow.toast,
  false,
  "companion_server_failed must not toast while the window is visible - the settings rollback toast already covers it (dual toast)",
);
assert.equal(
  companionVisibleWindow.notify,
  false,
  "companion_server_failed must not notify while the window is visible - the rollback toast is on screen",
);

// Symptom 3: command_mode_no_session keeps its pre-migration always-fired
// main-window toast during the Processing window (cardVisible=true).
const cmdProcessingWindow = routerDecision(
  "command_mode_no_session",
  true,
  false,
);
assert.equal(
  cmdProcessingWindow.toast,
  true,
  "command_mode_no_session must toast even while the Processing card is visible - App.tsx:403 early-returns on card_visible today, silencing the pre-migration toast (transcription_coordinator.rs:1005)",
);

// Symptom 4: the cap notifies while the window is hidden even when the card
// is showing - the finalize pipeline emitted right before the cap notice can
// overwrite the card row, so the notification is the surviving surface.
const capCardShowing = routerDecision("companion_session_capped", true, true);
assert.equal(
  capCardShowing.notify,
  true,
  "companion_session_capped must notify when the window is hidden even though the card is showing (server.rs:401 finalizes before the cap emit at :413; RecordingOverlay.tsx:266-274 can overwrite the row)",
);
assert.equal(
  capCardShowing.toast,
  false,
  "companion_session_capped toast stays card-gated - the card row is the visible surface when present",
);

// ---- Exhaustive decision table: every routed code in all four
// cardVisible x windowHidden states.

const STATES: Array<readonly [boolean, boolean]> = [
  [true, true],
  [true, false],
  [false, true],
  [false, false],
];

// cardVisible-only gate: exactly today's App.tsx behavior (App.tsx:403-417)
// for the codes the router already routes.
const expectCardGated = (cardVisible: boolean, windowHidden: boolean) => ({
  toast: !cardVisible,
  notify: !cardVisible && windowHidden,
});

// windowHidden-only gate: act (toast + notify) only while the window is
// hidden; the legacy surface covers a visible window.
const expectLegacySurface = (_cardVisible: boolean, windowHidden: boolean) => ({
  toast: windowHidden,
  notify: windowHidden,
});

// The pre-migration command-mode toast fires in every card/window state; the
// notification still waits for a hidden window.
const expectAlwaysToast = (_cardVisible: boolean, windowHidden: boolean) => ({
  toast: true,
  notify: windowHidden,
});

// The cap: toast stays card-gated, but the notification policy is
// independent of the toast policy - notify whenever the window is hidden.
const expectCap = (cardVisible: boolean, windowHidden: boolean) => ({
  toast: !cardVisible,
  notify: windowHidden,
});

function checkGroup(
  label: string,
  codes: readonly string[],
  expected: (
    cardVisible: boolean,
    windowHidden: boolean,
  ) => NoticeRoutingDecision,
): void {
  for (const code of codes) {
    for (const [cardVisible, windowHidden] of STATES) {
      const got = routerDecision(code, cardVisible, windowHidden);
      const want = expected(cardVisible, windowHidden);
      assert.equal(
        got.toast,
        want.toast,
        `${label}: ${code} (cardVisible=${cardVisible}, windowHidden=${windowHidden}) toast`,
      );
      assert.equal(
        got.notify,
        want.notify,
        `${label}: ${code} (cardVisible=${cardVisible}, windowHidden=${windowHidden}) notify`,
      );
    }
  }
}

checkGroup("card-gated (keep today's behavior)", CARD_GATED, expectCardGated);
checkGroup(
  "legacy-surface (act only while the window is hidden)",
  LEGACY_SURFACE,
  expectLegacySurface,
);
checkGroup(
  "always-toast (pre-migration command-mode toast)",
  ALWAYS_TOAST,
  expectAlwaysToast,
);
checkGroup(
  "session-cap (notify independent of the toast gate)",
  CAP,
  expectCap,
);
checkGroup("unrouted (legacy listeners own the surfaces)", UNROUTED, () => ({
  toast: false,
  notify: false,
}));

// Unknown codes stay inert in every state - today's silent skip
// (App.tsx:404); the router must not invent surfaces for unmapped codes.
for (const [cardVisible, windowHidden] of STATES) {
  assert.equal(
    routerDecision("something_unmapped", cardVisible, windowHidden).toast,
    false,
    "unknown code must not toast",
  );
  assert.equal(
    routerDecision("something_unmapped", cardVisible, windowHidden).notify,
    false,
    "unknown code must not notify",
  );
  assert.equal(
    routerDecision("", cardVisible, windowHidden).toast,
    false,
    "empty code must not toast",
  );
}

// ---- Table integrity: cross-pin the routing tables against noticeMessage
// (AUD-02 symptom 5 - two hand tables, no pin).

// Every code the routing side knows has a noticeMessage arm.
for (const code of ALL_NOTICE_CODES) {
  assert.notEqual(
    noticeMessage(t, code),
    null,
    `routing knows code ${code} but noticeMessage has no arm for it - the hand tables drifted`,
  );
}

// And vice versa: the policy table carries one entry per notice code, so a
// new noticeMessage arm cannot appear without a routing policy (and a code
// cannot be routed without the module acknowledging the whole universe).
assert.deepEqual(
  Object.keys(NOTICE_ROUTE_POLICIES).sort(),
  [...ALL_NOTICE_CODES].sort(),
  "NOTICE_ROUTE_POLICIES must have exactly one entry per notice code (the noticeMessage arm set) - no invented codes, no forgotten arms",
);

// ROUTED_NOTICE_CODES is exactly the non-unrouted codes: the three orphaned
// Info codes and nothing else join the table the router already had.
assert.deepEqual(
  [...ROUTED_NOTICE_CODES].sort(),
  [...CARD_GATED, ...LEGACY_SURFACE, ...ALWAYS_TOAST, ...CAP].sort(),
  "ROUTED_NOTICE_CODES must be exactly the 15 non-unrouted codes (12 today + model_fallback, post_process_download_missing, post_process_too_long)",
);

console.log("noticeRouting tests passed");
