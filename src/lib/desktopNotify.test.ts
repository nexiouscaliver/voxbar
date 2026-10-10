import assert from "node:assert/strict";
import {
  notifyDesktop,
  primeNotificationPermission,
  resetPrimingLatchForTests,
} from "./desktopNotify";

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
    if (StubNotification.requestPermissionThrows) {
      return Promise.reject(new Error("stub: permission bridge broken"));
    }
    const result = StubNotification.permissionOnRequest;
    StubNotification.permission = result;
    return Promise.resolve(result);
  }
  static permissionOnRequest: "default" | "granted" | "denied" = "granted";
  static requestPermissionThrows = false;
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

// Permission-once logic lives in primeNotificationPermission (AUD-03): a
// "default" permission is resolved by ONE requestPermission call, a plain
// user DENIAL fires no callback (silent - the system prompt is the only
// way back, and macOS deny-once makes an in-app retry a no-op), only a
// genuine request FAILURE (the request threw) fires the callback, and
// notifyDesktop itself never prompts.
sent.length = 0;
StubNotification.permission = "default";
StubNotification.permissionOnRequest = "granted";
let failures = 0;
await primeNotificationPermission(() => {
  failures++;
});
assert.equal(requestPermissionCalls, 1);
assert.equal(failures, 0);
await notifyDesktop("first-ask notice");
assert.deepEqual(sent, [{ title: "VoxBar", body: "first-ask notice" }]);

// The prime latch is consumed, so a later "default"/denied state neither
// re-prompts nor sends: a denial must not re-prompt on every notice.
sent.length = 0;
StubNotification.permission = "default";
StubNotification.permissionOnRequest = "denied";
await notifyDesktop("denied-ask notice");
assert.equal(requestPermissionCalls, 1);
assert.deepEqual(sent, []);
await notifyDesktop("post-denial notice");
assert.equal(requestPermissionCalls, 1);
assert.deepEqual(sent, []);

// A fresh run (latch reset) whose request comes back DENIED is silent: the
// request happens once more, the failure callback never fires, and
// notifyDesktop stays quiet while ungranted. The denial's info log and the
// throwing scenario's warn below are the product's EXPECTED output - but a
// warn stack reads like a failure in CI logs, so both are silenced here.
const originalInfo = console.info;
const originalWarnBeforePermissionLogs = console.warn;
console.info = () => {};
console.warn = () => {};
try {
  resetPrimingLatchForTests();
  failures = 0;
  await primeNotificationPermission(() => {
    failures++;
  });
  assert.equal(requestPermissionCalls, 2);
  assert.equal(failures, 0);
  await notifyDesktop("post-denial-primed notice");
  assert.deepEqual(sent, []);

  // A fresh run whose permission request genuinely THREW fires the failure
  // callback exactly once - the once-per-run latch still applies, so a
  // second prime neither re-requests nor re-fires - and notifyDesktop stays
  // quiet while ungranted. This is the only path App.tsx toasts
  // requestFailed for.
  resetPrimingLatchForTests();
  StubNotification.requestPermissionThrows = true;
  failures = 0;
  await primeNotificationPermission(() => {
    failures++;
  });
  assert.equal(requestPermissionCalls, 3);
  assert.equal(failures, 1);
  await primeNotificationPermission(() => {
    failures++;
  });
  assert.equal(requestPermissionCalls, 3);
  assert.equal(failures, 1);
  await notifyDesktop("post-failure notice");
  assert.deepEqual(sent, []);
  StubNotification.requestPermissionThrows = false;
} finally {
  console.info = originalInfo;
  console.warn = originalWarnBeforePermissionLogs;
}

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
