// Pure notice-routing decision for the App-level notice router (KB-020,
// AUD-02): given a notice code and whether the overlay card could render it
// at emit time (card_visible, transcription.rs:387-392), decide WHICH
// surfaces - the main-window toast and/or the macOS notification - the code
// may use. Everything with a side effect (rate limiting, the noticeMessage
// null check, toast durations, the actual toast/notifyDesktop calls) stays
// in App.tsx; this module only decides.
//
// The notify leg is deliberately NOT gated on a window-visibility input:
// notifyDesktop itself queries the live window state (getCurrentWindow,
// KB-195) and stays quiet when the window is visible. A pre-gate fed by the
// Rust-event visibility store proved strictly worse - minimize and Cmd+H
// emit no Rust event, so the store goes stale-visible and silently drops
// notifications the live query would have delivered.
//
// NOTICE_ROUTE_POLICIES carries exactly one entry per notice code - the
// NoticeCode::as_str universe mirrored from transcription.rs:498-529, the
// same arm set noticeMessage localizes - so the two hand tables are
// cross-pinned by noticeRouting.test.ts: a new Rust notice code must be
// added here (and to the test's ALL_NOTICE_CODES list) when it joins the
// enum, or the pin fails. NOTICE_CHANNEL_TABLE below (the v1.6.0 persistent
// OS-notification classification) is pinned to the same universe by
// noticeOsRouting.test.ts.

// The surfaces one notice code may use in a given card/window state.
export type NoticeRoutingDecision = { toast: boolean; notify: boolean };

// Per-code routing policies:
// - "card-gated"      the router behavior for the no-op key feedback:
//                     toast only when the card could not render; the
//                     notification leg is decided by notifyDesktop's live
//                     visibility query.
// - "legacy-surface"  a dedicated legacy listener owns the toast in every
//                     window state (it fires unconditionally), so the
//                     router never toasts - it only adds the notification
//                     leg, which notifyDesktop gates on real visibility.
//                     This kills the hidden-window dual toast (legacy
//                     listener + router stacking) while still giving
//                     hidden-window users the OS notification.
// - "always-toast"    the pre-migration dedicated listener fired in every
//                     state (Processing card included), so the toast is
//                     unconditional; the notification leg is live-gated.
// - "session-cap"     the cap's toast stays card-gated (the card row is the
//                     visible surface when present) but its notification is
//                     independent of the toast gate, because the finalize
//                     pipeline that emits right before the cap can
//                     overwrite the card row.
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
  // v1.6.0 W1-L7: the pp hotkey's mid-session mode toggles - transient
  // keypress feedback on a live session, the same card-gated class as the
  // cycle confirm.
  post_process_session_upgraded: "card-gated",
  post_process_session_downgraded: "card-gated",
  wayland_tauri_hotkeys: "card-gated",
  gnome_overlay_fallback: "card-gated",
  companion_disconnected_finalized: "card-gated",
  // Round 7: a requested polish that could not start (config gap) and one
  // that ran but changed nothing both fire at dictation end, where the
  // card row is the natural surface; a hidden window gets the toast.
  post_process_not_configured: "card-gated",
  post_process_unchanged: "card-gated",

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
  // pipeline can overwrite the card's notice row after the cap emits). The
  // zero-audio notice (v1.6.0) shares the hazard class: the stop pipeline
  // emits it and then retires the card via the error-read delayed hide, so
  // the notification must not ride on the card still being visible.
  companion_session_capped: "session-cap",
  companion_zero_audio: "session-cap",

  // Unrouted: every failure code with a dedicated legacy listener — those
  // keep their legacy toast plus the error sound, and routing them here too
  // would stack two toasts for one failure whenever the card was hidden.
  // Retiring those legacy listeners is follow-up work. KB-015/KB-122: the
  // memory-gate refusal rides the same model-state-changed listener as the
  // generic load failure, so it is unrouted with it.
  no_model_selected: "unrouted",
  microphone_permission_denied: "unrouted",
  no_input_device: "unrouted",
  recording_failed: "unrouted",
  transcription_failed: "unrouted",
  paste_failed: "unrouted",
  model_load_failed: "unrouted",
  model_load_failed_memory_gate: "unrouted",
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
// card could (cardVisible) or could not render it. Unknown, empty, and
// unrouted codes are inert in every state - the silent skip the old
// allowlist performed. The notify booleans are "offer the notification";
// notifyDesktop's internal live query makes the final visibility call.
export function routerDecision(
  code: string,
  cardVisible: boolean,
): NoticeRoutingDecision {
  switch (NOTICE_ROUTE_POLICIES[code]) {
    case "card-gated":
      return { toast: !cardVisible, notify: !cardVisible };
    case "legacy-surface":
      return { toast: false, notify: true };
    case "always-toast":
      return { toast: true, notify: true };
    case "session-cap":
      return { toast: !cardVisible, notify: true };
    default:
      return { toast: false, notify: false };
  }
}

// ---------------------------------------------------------------------------
// v1.6.0 W1-L4: the persistent OS-notification classification.
//
// The router above decides the TRANSIENT surfaces (the overlay card, the
// main-window toast, and today's hidden-window notification offer). This
// second, separate table classifies every notice code for the persistent
// macOS notification surface: a real Notification Center post that stays
// until the user dismisses it (the mail-notification model), independent of
// window visibility, so an error or a session outcome can no longer flash
// by in the fraction-of-a-second overlay and be lost.
//
// Exhaustive by construction, the same discipline as the Rust
// phone_visibility test (companion/mod.rs: every NoticeCode variant is
// listed once; a new code fails noticeOsRouting.test.ts until it joins a
// group here). Default stance: errors and final outcomes notify; transient
// states stay overlay-only.
//
// Channel classes:
// - "overlay-only"   the code keeps exactly today's surfaces (card, toasts,
//                    and whatever hidden-window offer its policy above
//                    already makes); the persistent surface never fires.
// - "notification"   the persistent OS notification is the code's ONLY
//                    durable surface. No code uses this today (every
//                    notifying code keeps its in-app surfaces too), but the
//                    class exists so a future code can choose it
//                    deliberately instead of overloading "both".
// - "both"           today's surfaces stay AND the persistent OS
//                    notification fires while the settings toggle is on.
//
// Group (required for notification/both codes) drives the settings
// sub-toggle: "error" codes notify whenever the master toggle is on;
// "outcome" codes additionally need the outcome-notifications sub-toggle.
// Target (required for notification/both codes) is where a notification
// click navigates: "notifications" is the in-app notification log section,
// "history" the session history.
export type NoticeChannelClass = "overlay-only" | "notification" | "both";

export type NoticeNotificationGroup = "error" | "outcome";

export type NoticeClickTarget = "notifications" | "history";

export interface NoticeChannelEntry {
  channel: NoticeChannelClass;
  group?: NoticeNotificationGroup;
  target?: NoticeClickTarget;
}

const NOTIFICATION = (
  group: NoticeNotificationGroup,
  target: NoticeClickTarget,
): NoticeChannelEntry => ({ channel: "both", group, target });

export const NOTICE_CHANNEL_TABLE: Record<string, NoticeChannelEntry> = {
  // Error group: every failure the user may not be watching for. All of
  // them navigate to the notification log (the durable in-app record); the
  // raw backend diagnostics stay out of the OS body (noticeNotificationBody)
  // but the log row keeps them.
  no_model_selected: NOTIFICATION("error", "notifications"),
  microphone_permission_denied: NOTIFICATION("error", "notifications"),
  no_input_device: NOTIFICATION("error", "notifications"),
  recording_failed: NOTIFICATION("error", "notifications"),
  transcription_failed: NOTIFICATION("error", "notifications"),
  paste_failed: NOTIFICATION("error", "notifications"),
  model_load_failed: NOTIFICATION("error", "notifications"),
  // KB-015/KB-122: the memory-gate refusal is the same failure class as
  // the generic load failure (the model did not load); its localized
  // notification body strips the interpolated amounts via the
  // detail-interpolating-code path.
  model_load_failed_memory_gate: NOTIFICATION("error", "notifications"),
  // A persistent misconfiguration, not a per-dictation condition: the
  // enabled polish silently never runs. Same class as a dead server.
  post_process_download_missing: NOTIFICATION("error", "notifications"),
  post_process_memory_gate: NOTIFICATION("error", "notifications"),
  post_process_engine_failed: NOTIFICATION("error", "notifications"),
  post_process_timeout: NOTIFICATION("error", "notifications"),
  post_process_length_guard: NOTIFICATION("error", "notifications"),
  post_process_output_invalid: NOTIFICATION("error", "notifications"),
  post_process_cloud_failed: NOTIFICATION("error", "notifications"),
  // Round 7: "post-processing is on but nothing ever runs" is a persistent
  // misconfiguration in the same class as a missing model download - it
  // never self-heals and the operator must change something in Settings,
  // so it posts like the other pp errors. The unchanged echo, by
  // contrast, is a transient per-dictation state (overlay-only below).
  post_process_not_configured: NOTIFICATION("error", "notifications"),
  companion_server_failed: NOTIFICATION("error", "notifications"),
  // Error tone, but a session outcome in shape: it navigates to the log
  // (not history) precisely because NO history entry exists - the log row
  // is the only durable record of why the phone session vanished.
  companion_zero_audio: NOTIFICATION("error", "notifications"),

  // Outcome group: final companion-session results. The finalized
  // transcript lives in history, so a click goes there.
  companion_session_capped: NOTIFICATION("outcome", "history"),
  companion_disconnected_finalized: NOTIFICATION("outcome", "history"),

  // Overlay-only: transient per-dictation states and advisory downgrades.
  // model_fallback and post_process_too_long are Info advisories that
  // recur per dictation (a persistent post for each would nag); the
  // no-op key feedback and Linux setup warnings are momentary. Every one
  // keeps whatever transient surfaces the policy table above already
  // gives it, including the hidden-window offers.
  model_fallback: { channel: "overlay-only" },
  post_process_too_long: { channel: "overlay-only" },
  // Round 7: the engine finished and changed nothing - a per-dictation
  // state the Debug table's run record already durables; a persistent
  // post per dictation would only nag.
  post_process_unchanged: { channel: "overlay-only" },
  delete_last_word_no_session: { channel: "overlay-only" },
  delete_last_word_no_buffer: { channel: "overlay-only" },
  undo_no_session: { channel: "overlay-only" },
  undo_no_buffer: { channel: "overlay-only" },
  binding_busy: { channel: "overlay-only" },
  post_process_prompt_cycled: { channel: "overlay-only" },
  // v1.6.0 W1-L7: the mid-session mode toggles are transient keypress
  // feedback (the badge and the notice row carry them); nothing durable
  // happened to persist.
  post_process_session_upgraded: { channel: "overlay-only" },
  post_process_session_downgraded: { channel: "overlay-only" },
  wayland_tauri_hotkeys: { channel: "overlay-only" },
  gnome_overlay_fallback: { channel: "overlay-only" },
  command_mode_no_session: { channel: "overlay-only" },
};

// The channel class of one code; unknown, empty, and unmapped codes are
// overlay-only (inert on the persistent surface, same silent-skip
// discipline as routerDecision).
export function noticeChannelClass(code: string): NoticeChannelClass {
  return NOTICE_CHANNEL_TABLE[code]?.channel ?? "overlay-only";
}

// Where a click on this code's persistent notification navigates, or null
// when the code never posts one. Unknown codes map to null, never a guess.
export function noticeClickTarget(code: string): NoticeClickTarget | null {
  const entry = NOTICE_CHANNEL_TABLE[code];
  if (!entry) return null;
  if (entry.channel === "overlay-only") return null;
  return entry.target ?? "notifications";
}

// The persistent-surface settings (notificationPrefs.ts owns the store and
// persistence; this is the pure input shape).
export interface OsNotificationPrefs {
  /** Master toggle: the persistent surface fires at all. Default on. */
  enabled: boolean;
  /** Sub-toggle: outcome-group codes notify too (vs errors only). */
  outcomes: boolean;
}

export const DEFAULT_OS_NOTIFICATION_PREFS: OsNotificationPrefs = {
  enabled: true,
  outcomes: true,
};

// Does this code post a persistent OS notification under these prefs?
// Pure; unknown and overlay-only codes never do. Error-group codes fire on
// the master toggle alone; outcome-group codes also need the sub-toggle.
export function osNoticeDecision(
  code: string,
  prefs: OsNotificationPrefs,
): { persistent: boolean } {
  const entry = NOTICE_CHANNEL_TABLE[code];
  if (!entry || entry.channel === "overlay-only") {
    return { persistent: false };
  }
  if (entry.group === "outcome") {
    return { persistent: prefs.enabled && prefs.outcomes };
  }
  return { persistent: prefs.enabled };
}

// The complete dispatch plan for one notice event: the single seam App.tsx
// consults. Composes the transient router decision with the persistent
// classification so the two surfaces can never double-post: when the
// persistent notification fires it REPLACES today's hidden-window offer
// (strictly more reach: it also fires while the window is visible). When
// the master toggle is off the plan degrades to exactly the pre-v1.6.0
// dispatch (toast + hidden-window offer per routerDecision alone) AND no
// log write: "off restores today's behavior exactly" includes the in-app
// notification log, which is new durable state v1.6.0 must not add while
// the feature is disabled.
export interface NoticeDispatchPlan {
  /** Main-window toast, unchanged semantics from routerDecision. */
  toast: boolean;
  /** Post the persistent OS notification (visibility-independent). */
  persistentOs: boolean;
  /** Today's notifyDesktop offer; delivery still gated by its live
   *  window-visibility query. Suppressed exactly when persistentOs fires. */
  legacyHiddenOsOffer: boolean;
  /** Append the notice to the in-app notification log (and persist it).
   *  True only while the master toggle is on: the log is part of the
   *  notification system, not an always-on recorder, so the off path adds
   *  no durable state the pre-v1.6.0 app did not write. */
  log: boolean;
}

export function planNoticeDispatch(
  code: string,
  cardVisible: boolean,
  prefs: OsNotificationPrefs,
): NoticeDispatchPlan {
  const routing = routerDecision(code, cardVisible);
  const { persistent } = osNoticeDecision(code, prefs);
  return {
    toast: routing.toast,
    persistentOs: persistent,
    legacyHiddenOsOffer: routing.notify && !persistent,
    log: prefs.enabled && noticeChannelClass(code) !== "overlay-only",
  };
}

// Structural shape desktopNotify's click bus delivers (kept local so this
// pure module does not import the side-effect module).
export interface NoticeNotificationClickLike {
  code?: string;
  target?: string;
}

// The navigation one notification click performs (App.tsx wiring): errors
// and anything unmapped land on the notifications log section; session
// outcomes (the history-target codes) land on history. Total by design:
// a garbage or missing target never throws and never strands the click.
export type NoticeClickSection = "notifications" | "history";

export function noticeClickSection(
  click: NoticeNotificationClickLike,
): NoticeClickSection {
  return click.target === "history" ? "history" : "notifications";
}

// KB-087: the tray copy actions' outcome dispatch. The tray menu itself
// gives no feedback (it closes and nothing further happens on any
// surface), so the Rust side emits one tray-copy-outcome event per copy
// attempt and App.tsx consults THIS decision before posting the OS
// notification: only while the notifications master toggle is on, the same
// gate the persistent surface honors. Delivery still funnels through
// notifyDesktop, whose live window-visibility query keeps a frontmost
// main window un-spammed (the tray click happens with the window hidden,
// which is exactly when the banner earns its place).
export function trayCopyOutcomeDispatch(prefs: OsNotificationPrefs): {
  notify: boolean;
} {
  return { notify: prefs.enabled };
}
