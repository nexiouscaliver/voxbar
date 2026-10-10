import {
  isPermissionGranted,
  requestPermission,
  sendNotification,
} from "@tauri-apps/plugin-notification";

// System-notification wrapper for notices the app's own windows cannot or
// may not deliver: overlay notices that arrive while the card cannot show
// them (KB-020) and update-flow prompts that live in the often-hidden main
// window (KB-148).

// Permission is requested at most once per app run, EAGERLY at the first
// main-window show (AUD-03) — never lazily inside notifyDesktop. The old
// lazy request meant the macOS system prompt could appear with no window
// visible at all (the notice arrived while hidden), and one denial then
// silently disabled the surface forever. App.tsx calls
// primeNotificationPermission from the window-visibility store's first
// "visible" transition and owns the denial toast.
let permissionPrimed = false;

// Ask for notification permission once per app run. Already-granted runs
// resolve without any prompt. `onDenied` fires at most once per run — on
// the run whose request came back denied — so the caller can show its
// single explanatory toast. Never throws: permission is a courtesy
// surface.
export async function primeNotificationPermission(
  onDenied?: () => void,
): Promise<void> {
  if (permissionPrimed) return;
  permissionPrimed = true;
  try {
    let granted = await isPermissionGranted();
    if (!granted) {
      await requestPermission();
      granted = await isPermissionGranted();
    }
    if (!granted) onDenied?.();
  } catch (error) {
    console.warn("Notification permission request failed:", error);
  }
}

// Post one OS notification. Never throws: a notification is a courtesy
// surface, so a failure (missing permission, plugin error) is logged and
// swallowed rather than breaking the flow that tried to tell the user
// something. Permission is NOT requested here (see
// primeNotificationPermission): a denial simply means no notification, and
// the app's own toast surfaces still carry the message. "VoxBar" is the
// product name and intentionally not localized.
export async function notifyDesktop(body: string): Promise<void> {
  try {
    if (await isPermissionGranted()) {
      sendNotification({ title: "VoxBar", body });
    }
  } catch (error) {
    console.warn("Desktop notification failed:", error);
  }
}
