import {
  isPermissionGranted,
  onAction,
  requestPermission,
  sendNotification,
} from "@tauri-apps/plugin-notification";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { commands } from "@/bindings";

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
// hidden, delivered as a plain banner through the notification plugin.
// The persistent surface below posts the notice to Notification Center
// regardless of window visibility, so an error or a session outcome cannot
// flash by in the overlay and vanish: the post stays in Notification
// Center until the user dismisses it (the mail-notification model).
// Honest platform note: whether the banner form itself stays on screen
// (alert style) or auto-hides into Notification Center (banner style) is
// the user's per-app System Settings choice; no app API can force the
// alert style. Either way the notification remains listed in Notification
// Center until dismissed.
//
// Sending goes through ONE Rust seam (commands.postPersistentNotice,
// src-tauri/src/persistent_notice.rs): on macOS the Rust side sends via
// mac-notification-sys with wait_for_click, so a real content click raises
// the main window and arrives here as the notice-notification-click event
// (the plugin's desktop build cannot do this: it registers no action
// listener and drops the routing payload before the OS). On other desktop
// platforms the Rust seam forwards to the same notification plugin this
// file's transient surface uses, so nothing diverges.

export type NoticeNotificationTarget = "notifications" | "history";

export interface PersistentNoticeNotification {
  /** Detail-free localized body (noticeNotificationBody, KB-037). */
  body: string;
  /** The notice code, for click-through routing. */
  code: string;
  /** Where a click navigates (noticeRouting.noticeClickTarget). */
  target: NoticeNotificationTarget;
}

// Post one persistent notification. No visibility gate by design (that is
// the point of this surface); still permission-gated and never prompting,
// never throwing. Delivery is fire-and-forget like notifyDesktop: the
// command returns once the notice is handed to the platform.
export async function notifyDesktopPersistent(
  notification: PersistentNoticeNotification,
): Promise<void> {
  try {
    if (await isPermissionGranted()) {
      await commands.postPersistentNotice(
        notification.body,
        notification.code,
        notification.target,
      );
    }
  } catch (error) {
    console.warn("Persistent desktop notification failed:", error);
  }
}

// ---- The click bus ----
//
// When a notification the app posted is activated, subscribers hear
// { code, target } and App.tsx navigates via noticeClickSection
// (noticeRouting.ts). Two sources feed the bus:
// 1. NATIVE (macOS production path): the Rust send leg
//    (src-tauri/src/persistent_notice.rs) posts through mac-notification-sys
//    with wait_for_click; on a content click it raises the main window and
//    emits notice-notification-click, which attachNativeNoticeClickSource
//    funnels into the bus.
// 2. PLUGIN (mobile, or a future desktop plugin build that wires action
//    events): attachNoticeNotificationClickSource wires the plugin's
//    onAction channel. The installed desktop plugin (2.4.0) registers no
//    action listener (verified against its commands.rs), so on desktop
//    this registration rejects; that is logged once at info level as an
//    expected capability gap, not an error - source 1 already covers
//    desktop.

export interface NoticeNotificationClick {
  code?: string;
  target?: string;
}

type NoticeNotificationClickListener = (click: NoticeNotificationClick) => void;

const clickListeners = new Set<NoticeNotificationClickListener>();
let clickSourceAttached = false;
let nativeClickSourceAttached = false;

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
 * Deliver one activation to the current subscribers: the single funnel both
 * click sources (and the tests) feed. Accepts the plugin Options shape
 * (payload under `extra`) or the native event payload ({ code, target }
 * directly); non-string fields are dropped, never coerced.
 */
export function dispatchNoticeNotificationClick(
  notification:
    | { extra?: Record<string, unknown> }
    | NoticeNotificationClick
    | null
    | undefined,
): void {
  const click: NoticeNotificationClick = {};
  const source =
    notification && typeof notification === "object"
      ? "extra" in notification &&
        notification.extra &&
        typeof notification.extra === "object"
        ? (notification.extra as Record<string, unknown>)
        : (notification as Record<string, unknown>)
      : null;
  if (source) {
    if (typeof source.code === "string") click.code = source.code;
    if (typeof source.target === "string") click.target = source.target;
  }
  for (const listener of [...clickListeners]) {
    listener(click);
  }
}

// Wire the plugin's onAction channel into the bus, once per run. See the
// source list above for where this does and does not work. Never throws.
export async function attachNoticeNotificationClickSource(): Promise<void> {
  if (clickSourceAttached) return;
  clickSourceAttached = true;
  try {
    await onAction((notification) => {
      dispatchNoticeNotificationClick(notification);
    });
  } catch (error) {
    console.info(
      "Plugin notification action events unavailable on this platform; " +
        "using the native click event instead where the sender supports it:",
      error,
    );
  }
}

// Wire the native Rust click event (NOTICE_CLICK_EVENT in
// persistent_notice.rs) into the bus, once per run. The listener lives for
// the app's lifetime by design. On platforms whose Rust sender has no
// click leg (non-macOS desktop today) the event simply never fires. Never
// throws.
export async function attachNativeNoticeClickSource(): Promise<void> {
  if (nativeClickSourceAttached) return;
  nativeClickSourceAttached = true;
  try {
    await listen("notice-notification-click", (event) => {
      dispatchNoticeNotificationClick(event.payload ?? null);
    });
  } catch (error) {
    console.warn("Native notification click listener failed to attach:", error);
  }
}

/** Test-only: forget the once-per-run attach latches between scenarios. */
export function resetClickSourceLatchForTests(): void {
  clickSourceAttached = false;
  nativeClickSourceAttached = false;
}
