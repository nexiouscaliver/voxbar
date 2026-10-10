import assert from "node:assert/strict";

// AUD-03 repro: notification delivery must not depend on
// document.visibilityState (unreliable in hidden native webviews, tauri#10592)
// and the OS notification body must not carry the raw interpolated backend
// error (KB-037 class).
//
// Two contracts are pinned here; both are missing from the product code today,
// so this file fails until the fix lands:
//
// 1. A shared main-window visibility store in src/lib/windowVisibility.ts.
//    App.tsx listens to the Rust window event(s) once and feeds
//    setMainWindowVisibility; every notifyDesktop gate (App.tsx router,
//    updaterFlow.ts) reads getMainWindowVisibility() instead of
//    document.visibilityState. The store itself must stay pure (importable in
//    bun with no Tauri side effects).
//
// 2. A detail-free notification body selector, noticeNotificationBody(t, code,
//    detail?), exported from src/lib/noticeMessage.ts. The router toast keeps
//    the interpolated detail (noticeMessage is unchanged); the OS notification
//    gets the same localized line with the interpolation stripped.

// ---------------------------------------------------------------------------
// Part 1: the window-visibility store (src/lib/windowVisibility.ts)
// ---------------------------------------------------------------------------

type MainWindowVisibility = "visible" | "hidden";

type VisibilityModule = {
  getMainWindowVisibility(): MainWindowVisibility;
  setMainWindowVisibility(next: MainWindowVisibility): void;
  subscribeMainWindowVisibility(
    listener: (state: MainWindowVisibility) => void,
  ): () => void;
};

let visibility: VisibilityModule;
try {
  visibility = (await import("./windowVisibility")) as VisibilityModule;
} catch {
  assert.fail(
    "AUD-03: src/lib/windowVisibility.ts must exist and stay importable in bun " +
      "(pure store, no Tauri work at import time). Exports: " +
      "getMainWindowVisibility(): 'visible' | 'hidden', " +
      "setMainWindowVisibility(next): void (the seam App.tsx's Rust-event " +
      "listener feeds), subscribeMainWindowVisibility(listener): () => void.",
  );
}

// Initial state: before any Rust hidden/shown signal has arrived the store
// must NOT claim the window is visible. document.visibilityState's optimistic
// "visible" is exactly the AUD-03 failure (a hidden window silently swallows
// the notification); unknown must be treated as hidden so the notification
// fires rather than expiring unseen.
assert.equal(
  visibility.getMainWindowVisibility(),
  "hidden",
  "initial state is hidden: no Rust signal yet means not provably visible",
);

// Transition semantics: subscribers hear every CHANGE with the new state,
// never a redundant repeat of the current state, and a fresh subscriber is
// not called back with the current state on subscribe (no replay).
const observed: MainWindowVisibility[] = [];
const unsubscribe = visibility.subscribeMainWindowVisibility((state) => {
  observed.push(state);
});

// The "shown" transition (main window revealed: tray open / Reopen / reveal
// from a command). This is also the transition that must trigger the eager
// one-time notification-permission request (permission UX part of AUD-03).
visibility.setMainWindowVisibility("visible");
assert.equal(
  visibility.getMainWindowVisibility(),
  "visible",
  "getState reflects the shown transition",
);
assert.deepEqual(
  observed,
  ["visible"],
  "the shown transition notifies subscribers with the new state",
);

// A redundant repeat of the current state (duplicate window event) must not
// re-notify: consumers like the one-shot permission request rely on hearing
// real transitions only.
visibility.setMainWindowVisibility("visible");
assert.deepEqual(
  observed,
  ["visible"],
  "a redundant same-state event does not re-notify subscribers",
);

// The "hidden" transition (CloseRequested -> hide, the tray-dwell path).
visibility.setMainWindowVisibility("hidden");
assert.equal(
  visibility.getMainWindowVisibility(),
  "hidden",
  "getState reflects the hidden transition",
);
assert.deepEqual(
  observed,
  ["visible", "hidden"],
  "the hidden transition notifies subscribers with the new state",
);

// Unsubscribe stops delivery; getState keeps tracking regardless.
unsubscribe();
visibility.setMainWindowVisibility("visible");
assert.deepEqual(
  observed,
  ["visible", "hidden"],
  "an unsubscribed listener hears nothing further",
);
assert.equal(
  visibility.getMainWindowVisibility(),
  "visible",
  "getState tracks the latest transition without any subscriber attached",
);

// A fresh subscriber is not replayed the current state on subscribe.
const lateObserved: MainWindowVisibility[] = [];
visibility.subscribeMainWindowVisibility((state) => {
  lateObserved.push(state);
});
assert.deepEqual(
  lateObserved,
  [],
  "subscribing does not replay the current state; only future transitions",
);

// ---------------------------------------------------------------------------
// Part 2: the detail-free notification body (src/lib/noticeMessage.ts)
// ---------------------------------------------------------------------------

// A translate fn backed by the real en strings for the codes in play,
// interpolating exactly like i18next does by default (verified against the
// bundled i18next: a variable present in options interpolates its value even
// when empty; a variable absent from options is LEFT AS THE LITERAL
// "{{var}}" placeholder because skipOnVariables defaults to true). Missing
// keys echo the key so a wrong key fails loudly.
const en: Record<string, string> = {
  "errors.recordingFailed": "Failed to start recording: {{error}}",
  "errors.modelLoadFailed": "Failed to load model: {{model}}",
  "errors.modelFallback": "Switched to {{model}} for this dictation",
  "app.commandNoSession":
    "The command key works while a dictation is live. Start recording first, then hold it and speak the command.",
  "toast.postProcessSkip.memoryGate":
    "Post-processing skipped to protect system memory. {{detail}}",
  "overlay.notice.bindingBusy":
    "Another dictation key is already recording.",
  "overlay.notice.postProcessCloudFailed":
    "Polishing failed and the raw transcript was kept: {{detail}}",
  "overlay.notice.postProcessPromptCycled": "Post-process template: {{name}}",
  "overlay.notice.companionDisconnected":
    "Phone disconnected. Finishing with what was captured.",
  "overlay.notice.companionServerFailed": "Companion server failed: {{error}}",
  "overlay.notice.companionSessionCapped":
    "Companion dictation ended: the 15-minute session limit was reached.",
};

const t = (key: string, options?: Record<string, unknown>): string => {
  const template = en[key] ?? key;
  return template.replace(/\{\{\s*(\w+)\s*\}\}/g, (raw, name: string) => {
    if (options && Object.prototype.hasOwnProperty.call(options, name)) {
      return String(options[name]);
    }
    return raw;
  });
};

type NoticeModule = {
  noticeMessage: (
    t: (key: string, options?: Record<string, unknown>) => string,
    code: string,
    detail?: string | null,
  ) => string | null;
  noticeNotificationBody: (
    t: (key: string, options?: Record<string, unknown>) => string,
    code: string,
    detail?: string | null,
  ) => string | null;
};

const notice = (await import("./noticeMessage")) as NoticeModule;
if (typeof notice.noticeNotificationBody !== "function") {
  assert.fail(
    "AUD-03: src/lib/noticeMessage.ts must export " +
      "noticeNotificationBody(t, code, detail?): string | null — the " +
      "detail-free body the OS notification shows while the toast keeps the " +
      "interpolated noticeMessage output.",
  );
}
const notificationBody = notice.noticeNotificationBody;

// The routed raw-backend-error code (the AUD-03 named offender): the OS
// notification body is the localized line WITHOUT the raw error and WITHOUT
// the orphaned ": " separator the removed interpolation leaves behind.
assert.equal(
  notificationBody(t, "companion_server_failed", "bind 0.0.0.0:8443: EADDRINUSE"),
  "Companion server failed",
  "companion_server_failed notification body strips the raw backend error " +
    "and its separator",
);
assert.equal(
  notificationBody(t, "companion_server_failed"),
  "Companion server failed",
  "without a detail the notification body is the same detail-free line — " +
    "never a literal {{error}} placeholder",
);

// The other routed interpolating code: template name stripped the same way.
assert.equal(
  notificationBody(t, "post_process_prompt_cycled", "Concise"),
  "Post-process template",
  "post_process_prompt_cycled notification body drops the interpolated name",
);

// Routed codes that never interpolate: the notification body is exactly the
// shared localized line (identical to noticeMessage's output).
assert.equal(
  notificationBody(t, "companion_session_capped"),
  "Companion dictation ended: the 15-minute session limit was reached.",
  "detail-free routed codes keep their full sentence verbatim",
);
assert.equal(
  notificationBody(t, "companion_disconnected_finalized"),
  "Phone disconnected. Finishing with what was captured.",
);
assert.equal(
  notificationBody(t, "binding_busy"),
  "Another dictation key is already recording.",
);
assert.equal(
  notificationBody(t, "command_mode_no_session"),
  "The command key works while a dictation is live. Start recording first, then hold it and speak the command.",
);

// Unknown codes stay null so the router keeps skipping them silently (same
// fallback discipline as noticeMessage).
assert.equal(
  notificationBody(t, "something_unmapped", "x"),
  null,
  "unknown codes return null for the notification body too",
);
assert.equal(notificationBody(t, "", "x"), null);

// Every other detail-interpolating code (not routed to the notification
// today, but the selector must not leak on them either): a non-empty body
// that contains neither the raw detail nor a literal "{{" placeholder.
const interpolatingCases: Array<[string, string]> = [
  ["recording_failed", "mic busy"],
  ["model_load_failed", "Whisper Large"],
  ["model_fallback", "whisper-tiny"],
  ["post_process_memory_gate", "need 4 GB, have 2 GB"],
  ["post_process_cloud_failed", "openai/gpt-4o-mini: 401"],
];
for (const [code, detail] of interpolatingCases) {
  const body = notificationBody(t, code, detail);
  assert.ok(
    typeof body === "string" && body.length > 0,
    `${code} notification body must be a non-empty string`,
  );
  assert.ok(
    !body.includes(detail),
    `${code} notification body must not contain the raw detail`,
  );
  assert.ok(
    !body.includes("{{"),
    `${code} notification body must not leak an uninterpolated placeholder`,
  );
}

// Contrast, documenting the split the fix intends (this part passes today):
// the TOAST keeps the interpolated detail via noticeMessage, unchanged.
assert.equal(
  notice.noticeMessage(t, "companion_server_failed", "bind 0.0.0.0:8443: EADDRINUSE"),
  "Companion server failed: bind 0.0.0.0:8443: EADDRINUSE",
  "the toast path (noticeMessage) keeps the interpolated detail",
);

console.log("windowVisibility + notification body tests passed");
