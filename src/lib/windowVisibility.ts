// Shared main-window visibility store (AUD-03).
//
// document.visibilityState is unreliable in hidden native webviews
// (tauri#10592): it can keep reading "visible" while the window is hidden,
// which silently swallowed exactly the OS notifications that state gates.
// The Rust side instead emits main-window-shown / main-window-hidden
// (show_main_window and the CloseRequested->hide arm in lib.rs); App.tsx
// listens once and feeds the setter. The store feeds the permission-prime-
// on-show logic in App.tsx; it does NOT gate notification delivery -
// notifyDesktop (desktopNotify.ts) queries the live window state itself
// (getCurrentWindow, KB-195), because this event-fed store goes stale for
// minimize and Cmd+H (no Rust event fires for either) and would drop
// notifications the live query delivers.
//
// Pure store by design: importable in bun with no Tauri work at import
// time, so tests and non-Tauri callers drive it through the setter.

export type MainWindowVisibility = "visible" | "hidden";

// Before any Rust hidden/shown signal has arrived the window must NOT be
// claimed visible: unknown is treated as hidden so a notification fires
// rather than expiring unseen — the optimistic-visible failure mode this
// module exists to replace.
let visibility: MainWindowVisibility = "hidden";

const listeners = new Set<(state: MainWindowVisibility) => void>();

export function getMainWindowVisibility(): MainWindowVisibility {
  return visibility;
}

// Record a transition from a Rust window event. A redundant repeat of the
// current state (e.g. show on an already-shown window) does not re-notify:
// one-shot consumers like the notification-permission request rely on
// hearing real transitions only. State keeps tracking whether or not any
// subscriber is attached.
export function setMainWindowVisibility(next: MainWindowVisibility): void {
  if (next === visibility) return;
  visibility = next;
  // Snapshot so a listener that unsubscribes mid-delivery cannot skew the
  // iteration.
  for (const listener of [...listeners]) {
    listener(next);
  }
}

// Hear FUTURE transitions only: subscribing never replays the current
// state (read getMainWindowVisibility() for that). Returns the unsubscribe
// function.
export function subscribeMainWindowVisibility(
  listener: (state: MainWindowVisibility) => void,
): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}
