import {
  isPermissionGranted,
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
