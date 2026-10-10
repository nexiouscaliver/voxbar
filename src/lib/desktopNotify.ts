import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  isPermissionGranted,
  requestPermission,
  sendNotification,
} from "@tauri-apps/plugin-notification";

// System-notification wrapper for notices the app's own windows cannot or
// may not deliver: overlay notices that arrive while the card cannot show
// them (KB-020) and update-flow prompts that live in the often-hidden main
// window (KB-148).

// Permission is requested at most once per app run. On macOS the system
// prompt only appears the first time anyway; this flag also stops a denial
// from re-prompting (a denied requestPermission call is a no-op that keeps
// returning "denied") on every subsequent notice.
let permissionRequested = false;

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
// breaking the flow that tried to tell the user something. "VoxBar" is the
// product name and intentionally not localized.
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
    let granted = await isPermissionGranted();
    if (!granted && !permissionRequested) {
      permissionRequested = true;
      await requestPermission();
      granted = await isPermissionGranted();
    }
    if (granted) {
      sendNotification({ title: "VoxBar", body });
    }
  } catch (error) {
    console.warn("Desktop notification failed:", error);
  }
}
