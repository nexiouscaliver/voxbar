// Pure notice-routing decision for the App-level notice router (KB-020,
// AUD-02): given a notice code, whether the overlay card could render it at
// emit time (card_visible, transcription.rs:387-392), and whether the main
// window is hidden, decide WHICH surfaces — the main-window toast and/or the
// macOS notification — the code may use. Everything with a side effect (rate
// limiting, the noticeMessage null check, toast durations, the actual
// toast/notifyDesktop calls) stays in App.tsx; this module only decides.
//
// NOTICE_ROUTE_POLICIES carries exactly one entry per notice code — the
// NoticeCode::as_str universe mirrored from transcription.rs:498-529, the
// same arm set noticeMessage localizes — so the two hand tables are
// cross-pinned by noticeRouting.test.ts: a new Rust notice code must be
// added here (and to the test's ALL_NOTICE_CODES list) when it joins the
// enum, or the pin fails.

// The surfaces one notice code may use in a given card/window state.
export type NoticeRoutingDecision = { toast: boolean; notify: boolean };

// Per-code routing policies:
// - "card-gated"      today's router behavior: act only when the card could
//                     not render (toast), and then only while the window is
//                     hidden (notify).
// - "legacy-surface"  a dedicated legacy listener or rollback toast owns a
//                     VISIBLE main window, so the router acts (toast +
//                     notify) only while the window is hidden: no dual toast
//                     when visible, but hidden-window users still get the
//                     toast-for-reopen and the notification.
// - "always-toast"    the pre-migration dedicated listener fired in every
//                     state (Processing card included), so the toast is
//                     unconditional; the notification still waits for a
//                     hidden window.
// - "session-cap"     the cap's toast stays card-gated (the card row is the
//                     visible surface when present) but its notification is
//                     independent of the toast gate: notify whenever the
//                     window is hidden, because the finalize pipeline that
//                     emits right before the cap can overwrite the card row.
// - "unrouted"        legacy listeners own every main-window surface; the
//                     router is inert (unknown and empty codes included).
export type NoticeRoutePolicy =
  | "card-gated"
  | "legacy-surface"
  | "always-toast"
  | "session-cap"
  | "unrouted";

export const NOTICE_ROUTE_POLICIES: Record<string, NoticeRoutePolicy> = {
  // Card-gated: the no-op key feedback and Linux setup warnings (yesterday's
  // allowlist) plus the finalized-companion notice — exactly the pre-AUD-02
  // router behavior, kept as-is.
  delete_last_word_no_session: "card-gated",
  delete_last_word_no_buffer: "card-gated",
  undo_no_session: "card-gated",
  undo_no_buffer: "card-gated",
  binding_busy: "card-gated",
  post_process_prompt_cycled: "card-gated",
  wayland_tauri_hotkeys: "card-gated",
  gnome_overlay_fallback: "card-gated",
  companion_disconnected_finalized: "card-gated",

  // Legacy-surface: the three orphaned Info codes (their tone plays no sound
  // and, off the old allowlist, they had NO surface once the card and window
  // were both hidden) plus companion_server_failed (whose settings rollback
  // toast already covers a visible window — routing it there too dual-toasted).
  // cardVisible is deliberately not consulted: the literal "act only when the
  // window is hidden" gate.
  model_fallback: "legacy-surface",
  post_process_download_missing: "legacy-surface",
  post_process_too_long: "legacy-surface",
  companion_server_failed: "legacy-surface",

  // Always-toast: command mode with no live dictation lost its always-fired
  // main-window toast when it joined the notice channel (the card_visible
  // early-return silenced it during the Processing window).
  command_mode_no_session: "always-toast",

  // Session-cap: notification independent of the card gate (the finalize
  // pipeline can overwrite the card's notice row after the cap emits).
  companion_session_capped: "session-cap",

  // Unrouted: every failure code with a dedicated legacy listener — those
  // keep their legacy toast plus the error sound, and routing them here too
  // would stack two toasts for one failure whenever the card was hidden.
  // Retiring those legacy listeners is follow-up work.
  no_model_selected: "unrouted",
  microphone_permission_denied: "unrouted",
  no_input_device: "unrouted",
  recording_failed: "unrouted",
  transcription_failed: "unrouted",
  paste_failed: "unrouted",
  model_load_failed: "unrouted",
  post_process_memory_gate: "unrouted",
  post_process_engine_failed: "unrouted",
  post_process_timeout: "unrouted",
  post_process_length_guard: "unrouted",
  post_process_output_invalid: "unrouted",
  post_process_cloud_failed: "unrouted",
};

// Every code the router owns: the non-unrouted policies (yesterday's twelve
// plus the three orphaned Info codes).
export const ROUTED_NOTICE_CODES: Set<string> = new Set(
  Object.entries(NOTICE_ROUTE_POLICIES)
    .filter(([, policy]) => policy !== "unrouted")
    .map(([code]) => code),
);

// Pure and synchronous: which surfaces may `code` use given that the overlay
// card could (cardVisible) or could not render it, and the main window's
// hidden state. Unknown, empty, and unrouted codes are inert in every state
// — the silent skip the old allowlist performed.
export function routerDecision(
  code: string,
  cardVisible: boolean,
  windowHidden: boolean,
): NoticeRoutingDecision {
  switch (NOTICE_ROUTE_POLICIES[code]) {
    case "card-gated":
      return { toast: !cardVisible, notify: !cardVisible && windowHidden };
    case "legacy-surface":
      return { toast: windowHidden, notify: windowHidden };
    case "always-toast":
      return { toast: true, notify: windowHidden };
    case "session-cap":
      return { toast: !cardVisible, notify: windowHidden };
    default:
      return { toast: false, notify: false };
  }
}
