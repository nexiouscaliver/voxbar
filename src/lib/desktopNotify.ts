import {
  isPermissionGranted,
  onAction,
  requestPermission,
  sendNotification,
} from "@tauri-apps/plugin-notification";
import { getCurrentWindow } from "@tauri-apps/api/window";

let permissionPrimed = false;

// Ask for notification permission once per app run, ideally while the main
// window is visible so the system prompt never appears over a hidden app.
// Already-granted runs resolve without any prompt. `onFailure` fires at most
// once per run - only when the permission request genuinely THREW (plugin
// error), so the caller can show its failure toast. A plain user DENIAL is
// silent (log only): the system prompt is the only way back and the user
// just declined it - on macOS a deny-once makes an in-app retry a no-op, so
// advising "try again" (or re-nagging every run) would be wrong advice.
// Never throws: permission is a courtesy surface.
export async function primeNotificationPermission(
  onFailure?: () => void,
): Promise<void> {
  if (permissionPrimed) return;
  permissionPrimed = true;
  let granted = false;
  try {
    granted = await isPermissionGranted();
    if (!granted) {
      await requestPermission();
      granted = await isPermissionGranted();
    }
  } catch (error) {
    console.warn("Notification permission request failed:", error);
    onFailure?.();
    return;
  }
  if (!granted) {
    console.info(
      "Notification permission denied; notifications stay off for this run " +
        "(re-enable via the system prompt in System Settings).",
    );
  }
}

/** Test-only: forget the one-per-run prime latch between scenarios. */
export function resetPrimingLatchForTests(): void {
  permissionPrimed = false;
}

// KB-195: the visibility gate lives HERE, not at the call sites. The old
// call-site checks read document.visibilityState, which WKWebView keeps
// reporting as "visible" for an ordered-out Tauri window - silently no-oping
// every notification. The window's real state is the only trustworthy
// signal. When even that cannot be queried (no Tauri window context), warn
// once and stay quiet: a context that cannot know must not notify.
let warnedVisibilityUnavailable = false;

// Post one OS notification, but only when this webview's window is not
// visible. Never throws: a notification is a courtesy surface, so a failure
// (missing permission, plugin error) is logged and swallowed rather than
// breaking the flow that tried to tell the user something. Permission is
// NOT requested here (see primeNotificationPermission): a denial simply
// means no notification, and the app's own toast surfaces still carry the
// message. "VoxBar" is the product name and intentionally not localized.
export async function notifyDesktop(body: string): Promise<void> {
  let visible: boolean;
  try {
    visible = await getCurrentWindow().isVisible();
  } catch (error) {
    if (!warnedVisibilityUnavailable) {
      warnedVisibilityUnavailable = true;
      console.warn(
        "Desktop notification skipped: window visibility unavailable:",
        error,
      );
    }
    return;
  }
  try {
    if (visible) return;
    if (await isPermissionGranted()) {
      sendNotification({ title: "VoxBar", body });
    }
  } catch (error) {
    console.warn("Desktop notification failed:", error);
  }
}

// ---------------------------------------------------------------------------
// v1.6.0 W1-L4: the persistent notification surface plus its click bus.
//
// notifyDesktop above is the TRANSIENT surface: gated on the window being
// hidden, delivered as a plain banner. The persistent surface below posts
// the notice to Notification Center regardless of window visibility, so an
// error or a session outcome cannot flash by in the overlay and vanish:
// the post stays in Notification Center until the user dismisses it (the
// mail-notification model). Honest platform note: whether the banner form
// itself stays on screen (alert style) or auto-hides into Notification
// Center (banner style) is the user's per-app System Settings choice; the
// app cannot force the alert style through this plugin. Either way the
// notification remains listed in Notification Center until dismissed.
//
// The group/extra fields ride along for click-through: the plugin's
// desktop bridge (tauri-plugin-notification 2.4.0 desktop.rs) currently
// forwards only title/body/icon/sound to the OS and drops both, but the
// JS Options model carries them and a future bridge that honors them (or
// a mobile surface) gets the routing payload for free.

export type NoticeNotificationTarget = "notifications" | "history";

export interface PersistentNoticeNotification {
  /** Detail-free localized body (noticeNotificationBody, KB-037). */
  body: string;
  /** The notice code, for click-through routing. */
  code: string;
  /** Where a click navigates (noticeRouting.noticeClickTarget). */
  target: NoticeNotificationTarget;
}

// Thread identifier so notice posts stack as one group wherever the
// platform honors grouping (iOS/macOS threadIdentifier, Android channels).
const NOTICE_GROUP = "voxbar-notices";

// Post one persistent notification. No visibility gate by design (that is
// the point of this surface); still permission-gated and never prompting,
// never throwing. Delivery is fire-and-forget like notifyDesktop.
export async function notifyDesktopPersistent(
  notification: PersistentNoticeNotification,
): Promise<void> {
  try {
    if (await isPermissionGranted()) {
      sendNotification({
        title: "VoxBar",
        body: notification.body,
        group: NOTICE_GROUP,
        extra: { code: notification.code, target: notification.target },
      });
    }
  } catch (error) {
    console.warn("Persistent desktop notification failed:", error);
  }
}

// ---- The click bus ----
//
// When a notification the app posted is activated, subscribers hear
// { code, target } and App.tsx focuses the main window and navigates to
// the target section. Honest platform note: the plugin's desktop side
// (tauri-plugin-notification 2.4.0) registers only the notify/permission
// commands and emits no actionPerformed events, so on macOS today the
// click bus is wired but dormant: the OS still brings the app forward on
// a notification click (the system default action); the in-webview
// navigation event will start flowing the moment a plugin build wires
// desktop action events, with no caller changes. attachNoticeNotificationClickSource
// therefore treats a rejected registration as an expected, log-once gap,
// not an error.

export interface NoticeNotificationClick {
  code?: string;
  target?: string;
}

type NoticeNotificationClickListener = (click: NoticeNotificationClick) => void;

const clickListeners = new Set<NoticeNotificationClickListener>();
let clickSourceAttached = false;

/** Hear notification activations; returns the unsubscribe function. */
export function subscribeNoticeNotificationClicks(
  listener: NoticeNotificationClickListener,
): () => void {
  clickListeners.add(listener);
  return () => {
    clickListeners.delete(listener);
  };
}

/**
 * Test/dispatch seam: deliver one activation to the current subscribers,
 * exactly as the plugin's onAction callback would. Not called by app code;
 * exported so the click-through wiring is testable without the OS.
 */
export function dispatchNoticeNotificationClickForTests(
  notification: { extra?: Record<string, unknown> } | null | undefined,
): void {
  const extra = notification?.extra;
  const click: NoticeNotificationClick = {};
  if (extra && typeof extra === "object") {
    if (typeof extra.code === "string") click.code = extra.code;
    if (typeof extra.target === "string") click.target = extra.target;
  }
  for (const listener of [...clickListeners]) {
    listener(click);
  }
}

// Wire the plugin's onAction into the bus, once per run. On platforms
// without plugin action events the registration rejects; that is logged
// once at info level (an expected capability gap, not a failure) and the
// bus stays dormant for the run. Never throws.
export async function attachNoticeNotificationClickSource(): Promise<void> {
  if (clickSourceAttached) return;
  clickSourceAttached = true;
  try {
    await onAction((notification) => {
      dispatchNoticeNotificationClickForTests(notification);
    });
  } catch (error) {
    console.info(
      "Notification click events unavailable on this platform; " +
        "notification clicks will focus the app but not deep-navigate:",
      error,
    );
  }
}

/** Test-only: forget the once-per-run attach latch between scenarios. */
export function resetClickSourceLatchForTests(): void {
  clickSourceAttached = false;
}
