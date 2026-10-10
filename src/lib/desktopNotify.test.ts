import assert from "node:assert/strict";
import { notifyDesktop } from "./desktopNotify";

// KB-195: notifyDesktop gates itself on the current Tauri window's REAL
// visibility (an IPC roundtrip), not document.visibilityState - WKWebView
// keeps reporting "visible" for an ordered-out window, which used to no-op
// every notification when the gate lived at the call sites.
//
// Both @tauri-apps packages resolve everything through
// window.__TAURI_INTERNALS__ / window.Notification at CALL time, and bun
// scripts have no window global, so a stub on globalThis.window stands in
// for Tauri without module mocks.

interface RecordedNotification {
  title: string;
  body: string;
}

const sent: RecordedNotification[] = [];
const invokedCommands: string[] = [];
let requestPermissionCalls = 0;

// Tauri's notification plugin bridges the webview Notification API, so the
// stub class controls both the permission state machine and delivery.
class StubNotification {
  static permission: "default" | "granted" | "denied" = "granted";
  static requestPermission(): Promise<"default" | "granted" | "denied"> {
    requestPermissionCalls++;
    const result = StubNotification.permissionOnRequest;
    StubNotification.permission = result;
    return Promise.resolve(result);
  }
  static permissionOnRequest: "default" | "granted" | "denied" = "granted";
  constructor(title: string, options: { body?: string }) {
    sent.push({ title, body: options.body ?? "" });
  }
}

let windowVisible = true;

const stubWindow = {
  __TAURI_INTERNALS__: {
    metadata: { currentWindow: { label: "main" } },
    invoke: async (cmd: string): Promise<unknown> => {
      invokedCommands.push(cmd);
      if (cmd === "plugin:window|is_visible") return windowVisible;
      // "default" is not granted yet - only an explicit grant is.
      if (cmd === "plugin:notification|is_permission_granted")
        return StubNotification.permission === "granted";
      return null;
    },
  },
  Notification: StubNotification,
};

(globalThis as unknown as { window: typeof stubWindow }).window = stubWindow;

// Visible window: the only IPC traffic is the visibility query - no
// permission check, no delivery.
invokedCommands.length = 0;
windowVisible = true;
await notifyDesktop("visible-window notice");
assert.deepEqual(sent, []);
assert.deepEqual(invokedCommands, ["plugin:window|is_visible"]);

// Hidden window (the ordered-out case visibilityState cannot see): the
// notice is delivered under the product title.
windowVisible = false;
await notifyDesktop("hidden-window notice");
assert.deepEqual(sent, [{ title: "VoxBar", body: "hidden-window notice" }]);

// Permission-once logic survives the gate move: a "default" permission is
// resolved by ONE requestPermission call, and the notification still goes
// out only after the grant.
sent.length = 0;
StubNotification.permission = "default";
StubNotification.permissionOnRequest = "granted";
await notifyDesktop("first-ask notice");
assert.equal(requestPermissionCalls, 1);
assert.deepEqual(sent, [{ title: "VoxBar", body: "first-ask notice" }]);

// The one-ask latch is already consumed by the first request above, so a
// later "default"/denied state neither re-prompts nor sends: a denial must
// not re-prompt on every subsequent notice.
sent.length = 0;
StubNotification.permission = "default";
StubNotification.permissionOnRequest = "denied";
await notifyDesktop("denied-ask notice");
assert.equal(requestPermissionCalls, 1);
assert.deepEqual(sent, []);
await notifyDesktop("post-denial notice");
assert.equal(requestPermissionCalls, 1);
assert.deepEqual(sent, []);

// No Tauri window context (isVisible throws): treated as visible - no
// delivery - and the warning fires exactly once no matter how many calls.
StubNotification.permission = "granted";
let warns = 0;
const originalWarn = console.warn;
console.warn = () => {
  warns++;
};
try {
  const internalsHolder = stubWindow as unknown as {
    __TAURI_INTERNALS__?: unknown;
  };
  internalsHolder.__TAURI_INTERNALS__ = undefined;
  await notifyDesktop("contextless notice 1");
  await notifyDesktop("contextless notice 2");
} finally {
  console.warn = originalWarn;
}
assert.deepEqual(sent, []);
assert.equal(warns, 1);

console.log("desktopNotify: all assertions passed");
