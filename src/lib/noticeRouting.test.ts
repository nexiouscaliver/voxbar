import assert from "node:assert/strict";
import { noticeMessage } from "./noticeMessage";

// Contract test for the pure notice router (KB-020, AUD-02):
// routerDecision(code, cardVisible) decides WHICH surfaces - the main-window
// toast and/or the macOS notification - one notice code may use, given only
// whether the overlay card could render it at emit time (card_visible,
// transcription.rs:387-392). Everything with a side effect (rate limiting,
// the noticeMessage null check, toast durations, the actual toast /
// notifyDesktop calls) stays in App.tsx; this module only decides.
//
// The notify booleans are OFFERS, not delivery decisions: notifyDesktop
// itself queries the live window state (getCurrentWindow, KB-195) and stays
// quiet while the window is really visible. The router deliberately never
// sees window state - an earlier design fed it from the Rust-event
// visibility store, but minimize and Cmd+H emit no Rust event, so that store
// goes stale-visible and silently drops notifications the live query would
// have delivered. A { notify: true } decision means "the notification may
// fire if the live visibility query agrees", nothing more.
//
// The decision table also encodes the AUD-02 symptom fixes:
//   1. the three orphaned Info codes (model_fallback,
//      post_process_download_missing, post_process_too_long) plus
//      companion_server_failed never toast from the router (their legacy
//      listeners and rollback toasts own the main window in every state)
//      but always offer the notification - the only surface that reaches a
//      hidden window;
//   2. command_mode_no_session toasts in every card state (its pre-KB-038
//      dedicated listener always fired, Processing card included);
//   3. companion_session_capped offers the notification regardless of the
//      card gate, because the finalize pipeline that emits right before the
//      cap can overwrite the card's notice row;
//   4. NOTICE_ROUTE_POLICIES cross-pins the notice universe so the hand
//      tables cannot drift (one entry per noticeMessage arm).

// ---- Contract the module must satisfy (src/lib/noticeRouting.ts)
//
// export type NoticeRoutingDecision = { toast: boolean; notify: boolean };
//
// routerDecision(code, cardVisible) is pure and synchronous. `cardVisible`
// is the event payload's card_visible (could the overlay card render this
// notice at emit time, transcription.rs:387-392). Unknown, empty, and
// unrouted codes are inert: { toast: false, notify: false }.
//
// Exports:
//   routerDecision(code, cardVisible) -> NoticeRoutingDecision
//   ROUTED_NOTICE_CODES: Set<string>    - every code the router owns
//   NOTICE_ROUTE_POLICIES: Record<string, string> - policy per code, one key
//     for every code noticeMessage has an arm for (the cross-pinned table)

interface NoticeRoutingDecision {
  toast: boolean;
  notify: boolean;
}

interface NoticeRoutingModule {
  routerDecision: (code: string, cardVisible: boolean) => NoticeRoutingDecision;
  ROUTED_NOTICE_CODES: Set<string>;
  NOTICE_ROUTE_POLICIES: Record<string, string>;
}

// Loaded dynamically so a missing/broken module surfaces as a clear
// assertion message instead of a bare resolver crash.
let routing: NoticeRoutingModule | null = null;
let loadError = "";
try {
  routing = (await import("./noticeRouting")) as unknown as NoticeRoutingModule;
} catch (error) {
  loadError = error instanceof Error ? error.message : String(error);
}
if (routing === null) {
  assert.fail(
    "src/lib/noticeRouting.ts must exist and export routerDecision(code, " +
      "cardVisible), ROUTED_NOTICE_CODES and NOTICE_ROUTE_POLICIES " +
      "(import failed: " +
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
// companion server failure (symptom 2): legacy surfaces own toasts in every
// window state, so the router never toasts - it only adds the live-gated
// notification offer.
const ORPHANED_INFO = [
  "model_fallback",
  "post_process_download_missing",
  "post_process_too_long",
] as const;
const LEGACY_SURFACE = [...ORPHANED_INFO, "companion_server_failed"];

// The eight no-op codes App.tsx already routes (key feedback + Linux setup
// warnings) plus companion_disconnected_finalized: keep the pre-AUD-02
// card-gated behavior.
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
// router stays inert for them.
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

// ---- The AUD-02 symptoms, stated as decisions (the exhaustive table below
// covers every cell; these make each regression legible).

// Symptom 1: the orphaned Info codes gain a hidden-window surface - the
// notification offer - in EVERY card state. Their legacy toasts own the
// main window; when the window is hidden those toasts expire unseen, and
// the notification (delivery decided by notifyDesktop's live query) is the
// only surface that still reaches the user.
for (const code of ORPHANED_INFO) {
  for (const cardVisible of [true, false]) {
    const d = routerDecision(code, cardVisible);
    assert.equal(
      d.notify,
      true,
      `${code} must offer the notification in every card state (its legacy toast expires unseen in a hidden window; notifyDesktop's live query decides delivery)`,
    );
    assert.equal(
      d.toast,
      false,
      `${code} must never toast from the router - its legacy listener owns the main-window toast in every state`,
    );
  }
}

// Symptom 2: no dual toast - the settings rollback toast
// (settingsStore.ts:418) alone owns companion_server_failed's main window;
// the router only adds the notification offer.
const companionVisible = routerDecision("companion_server_failed", false);
assert.equal(
  companionVisible.toast,
  false,
  "companion_server_failed must never toast from the router - the settings rollback toast already covers a visible window (dual toast)",
);
assert.equal(
  companionVisible.notify,
  true,
  "companion_server_failed must still offer the notification - a hidden window hears nothing from the rollback toast",
);

// Symptom 3: command_mode_no_session keeps its pre-migration always-fired
// main-window toast during the Processing window (cardVisible=true).
const cmdProcessingWindow = routerDecision("command_mode_no_session", true);
assert.equal(
  cmdProcessingWindow.toast,
  true,
  "command_mode_no_session must toast even while the Processing card is visible - the card_visible early-return silenced the pre-migration toast (transcription_coordinator.rs:1005)",
);
assert.equal(
  cmdProcessingWindow.notify,
  true,
  "command_mode_no_session must also offer the notification (delivery gated by notifyDesktop's live visibility query)",
);

// Symptom 4: the cap offers the notification even while the card is showing
// - the finalize pipeline emitted right before the cap notice can overwrite
// the card row, so the notification is the surviving surface.
const capCardShowing = routerDecision("companion_session_capped", true);
assert.equal(
  capCardShowing.notify,
  true,
  "companion_session_capped must offer the notification even though the card is showing (server.rs:401 finalizes before the cap emit at :413; RecordingOverlay.tsx:266-274 can overwrite the row)",
);
assert.equal(
  capCardShowing.toast,
  false,
  "companion_session_capped toast stays card-gated - the card row is the visible surface when present",
);

// ---- Exhaustive decision table: every routed code in both cardVisible
// states. There is no windowHidden input by design (see header): the notify
// leg is an offer that notifyDesktop's live getCurrentWindow query gates at
// delivery time.

const STATES: readonly boolean[] = [true, false];

// Card-gated: exactly the pre-AUD-02 router behavior - both surfaces act
// only when the card could not render the notice. The notify offer is
// likewise card-gated so a card-visible emit never spawns a notification
// for code the user is already reading.
const expectCardGated = (cardVisible: boolean) => ({
  toast: !cardVisible,
  notify: !cardVisible,
});

// Legacy-surface: legacy listeners own toasts in every state; the router
// only adds the live-gated notification.
const expectLegacySurface = (_cardVisible: boolean) => ({
  toast: false,
  notify: true,
});

// The pre-migration command-mode toast fires in every card state; the
// notification is always offered (live-gated at delivery).
const expectAlwaysToast = (_cardVisible: boolean) => ({
  toast: true,
  notify: true,
});

// The cap: toast stays card-gated, but the notification offer is
// independent of the toast gate.
const expectCap = (cardVisible: boolean) => ({
  toast: !cardVisible,
  notify: true,
});

function checkGroup(
  label: string,
  codes: readonly string[],
  expected: (cardVisible: boolean) => NoticeRoutingDecision,
): void {
  for (const code of codes) {
    for (const cardVisible of STATES) {
      const got = routerDecision(code, cardVisible);
      const want = expected(cardVisible);
      assert.equal(
        got.toast,
        want.toast,
        `${label}: ${code} (cardVisible=${cardVisible}) toast`,
      );
      assert.equal(
        got.notify,
        want.notify,
        `${label}: ${code} (cardVisible=${cardVisible}) notify`,
      );
    }
  }
}

checkGroup("card-gated (pre-AUD-02 behavior)", CARD_GATED, expectCardGated);
checkGroup(
  "legacy-surface (toast stays with the legacy listener)",
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

// Unknown codes stay inert in every state - the silent skip; the router
// must not invent surfaces for unmapped codes.
for (const cardVisible of STATES) {
  assert.equal(
    routerDecision("something_unmapped", cardVisible).toast,
    false,
    "unknown code must not toast",
  );
  assert.equal(
    routerDecision("something_unmapped", cardVisible).notify,
    false,
    "unknown code must not notify",
  );
  assert.equal(
    routerDecision("", cardVisible).toast,
    false,
    "empty code must not toast",
  );
  assert.equal(
    routerDecision("", cardVisible).notify,
    false,
    "empty code must not notify",
  );
}

// ---- Table integrity: cross-pin the routing tables against noticeMessage
// (AUD-02 symptom - two hand tables, no pin).

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
// Info codes and companion_server_failed join the table the router already
// had.
assert.deepEqual(
  [...ROUTED_NOTICE_CODES].sort(),
  [...CARD_GATED, ...LEGACY_SURFACE, ...ALWAYS_TOAST, ...CAP].sort(),
  "ROUTED_NOTICE_CODES must be exactly the 16 non-unrouted codes (12 pre-AUD-02 + model_fallback, post_process_download_missing, post_process_too_long, companion_server_failed)",
);

console.log("noticeRouting tests passed");
