import assert from "node:assert/strict";

// v1.6.0 W1-L4 contract tests: the persistent notification surface, the
// click-through bus (src/lib/desktopNotify.ts), the notification log
// (src/lib/notificationLog.ts) and the prefs store
// (src/lib/notificationPrefs.ts).
//
// Same stub discipline as desktopNotify.test.ts: the @tauri-apps packages
// resolve everything through window.__TAURI_INTERNALS__ /
// window.Notification at CALL time, and bun scripts have no window global,
// so a stub on globalThis.window stands in for Tauri without module mocks.
//
// Pinned contracts:
// 1. notifyDesktopPersistent posts regardless of window visibility (no
//    is_visible roundtrip at all), permission-gated and never prompting,
//    through ONE Rust seam (the post_persistent_notice command) that
//    carries the click-through routing payload.
// 2. attachNoticeNotificationClickSource wires the plugin's onAction into
//    the bus; a platform without plugin action events (the tauri-plugin-
//    notification 2.4.0 desktop build) rejects the registration, which is
//    swallowed as an expected gap, once per run. The NATIVE source
//    (attachNativeNoticeClickSource) wires the Rust click event the macOS
//    sender emits, and delivers its payload to the same bus.
// 3. Click-through: a click carrying { code, target } reaches subscribers
//    as { code, target } from both source shapes; garbage payloads degrade
//    to an empty click, never a throw; unsubscribed listeners hear nothing.
// 4. The notification log: newest-first append, hard cap at the last 50,
//    full clear, and a storage round-trip that survives corrupt input.
// 5. The prefs store: defaults, coercion of malformed stored values, and
//    the write+persist path.

// ---------------------------------------------------------------------------
// The Tauri stub
// ---------------------------------------------------------------------------

const sent: string[] = [];
const invokedCommands: { cmd: string; args?: Record<string, unknown> }[] = [];

class StubNotification {
  static permission: "default" | "granted" | "denied" = "granted";
  static requestPermission(): Promise<"default" | "granted" | "denied"> {
    return Promise.resolve(StubNotification.permission);
  }
  constructor(title: string) {
    sent.push(title);
  }
}

// The channel callbacks the plugin/event listeners register
// (transformCallback): keyed by id so the test can PLAY real plugin action
// events and native Rust click events into the wiring.
const channelCallbacks = new Map<number, (message: unknown) => void>();
let nextCallbackId = 1;

let windowVisible = true;
let registerListenerBehavior: "resolve" | "reject" = "reject";
let eventListenBehavior: "resolve" | "reject" = "resolve";
let eventListenFailure = "";

const stubWindow = {
  __TAURI_INTERNALS__: {
    metadata: { currentWindow: { label: "main" } },
    transformCallback: (cb: (message: unknown) => void): number => {
      const id = nextCallbackId++;
      channelCallbacks.set(id, cb);
      return id;
    },
    unregisterCallback: (id: number): void => {
      channelCallbacks.delete(id);
    },
    invoke: async (
      cmd: string,
      args?: Record<string, unknown>,
    ): Promise<unknown> => {
      invokedCommands.push({ cmd, args });
      if (cmd === "plugin:window|is_visible") return windowVisible;
      if (cmd === "plugin:notification|is_permission_granted")
        return StubNotification.permission === "granted";
      if (cmd === "notification_permission_state") {
        if (StubNotification.permission === "granted") return "granted";
        if (StubNotification.permission === "denied") return "denied";
        return "not_determined";
      }
      // The Rust seam refuses the persistent post when the real grant is
      // missing (fleet pass 2): simulate the Err the denied state returns.
      if (
        cmd === "post_persistent_notice" &&
        StubNotification.permission === "denied"
      ) {
        throw new Error("stub: persistent notice not delivered (denied)");
      }
      if (
        cmd === "plugin:notification|register_listener" ||
        cmd === "plugin:notification|registerListener"
      ) {
        if (registerListenerBehavior === "reject") {
          throw new Error(`stub: ${cmd} not registered on desktop`);
        }
        return null;
      }
      if (cmd === "plugin:event|listen") {
        if (eventListenBehavior === "reject") {
          throw new Error(eventListenFailure || "stub: event listen failed");
        }
        return null;
      }
      return null;
    },
  },
  Notification: StubNotification,
};

(globalThis as unknown as { window: typeof stubWindow }).window = stubWindow;

const desktop = await import("./desktopNotify");
const {
  notifyDesktopPersistent,
  attachNativeNoticeClickSource,
  attachNoticeNotificationClickSource,
  subscribeNoticeNotificationClicks,
  dispatchNoticeNotificationClick,
  resetClickSourceLatchForTests,
} = desktop;

// ---------------------------------------------------------------------------
// 1. The persistent surface is visibility-independent
// ---------------------------------------------------------------------------

// Window VISIBLE: the persistent surface still posts (this is the point of
// the surface), through the single Rust seam. Fleet pass 2: the permission
// gate lives in the RUST seam (the plugin's desktop stub always says
// granted, so a JS-side gate was vacuous), so the only IPC traffic is the
// command itself - and in particular there is deliberately NO
// plugin:window|is_visible roundtrip here (that query is the legacy
// surface's gate, pinned below).
invokedCommands.length = 0;
windowVisible = true;
await notifyDesktopPersistent({
  body: "Polishing failed and the raw transcript was kept",
  code: "post_process_cloud_failed",
  target: "notifications",
});
assert.deepEqual(
  invokedCommands,
  [
    {
      cmd: "post_persistent_notice",
      args: {
        body: "Polishing failed and the raw transcript was kept",
        code: "post_process_cloud_failed",
        target: "notifications",
      },
    },
  ],
  "the persistent post goes through the single Rust seam, carrying the routing payload",
);
assert.ok(
  !invokedCommands.some((entry) => entry.cmd === "plugin:window|is_visible"),
  "the persistent surface must not query window visibility (contrast notifyDesktop)",
);

// Window HIDDEN: identical post (visibility is simply not an input).
invokedCommands.length = 0;
windowVisible = false;
await notifyDesktopPersistent({
  body: "Companion server failed",
  code: "companion_server_failed",
  target: "notifications",
});
assert.equal(
  invokedCommands.filter((entry) => entry.cmd === "post_persistent_notice")
    .length,
  1,
  "hidden window posts the same persistent notice",
);

// Contrast pin: the legacy notifyDesktop surface DOES gate on visibility
// (pre-existing behavior this file keeps honest).
sent.length = 0;
invokedCommands.length = 0;
windowVisible = true;
await desktop.notifyDesktop("transient banner");
assert.deepEqual(sent, [], "legacy surface stays quiet while visible");

// Permission denied (fleet pass 2 contract): the command IS invoked - the
// real authorization gate lives in the Rust seam, which consults
// UNUserNotificationCenter and returns Err on denial (the old JS gate never
// fired because the plugin's desktop stub always said granted, so a
// System-Settings denial silently posted into a void). The Err lands in
// the catch and is logged, never thrown, and the JS surface never prompts
// (priming is primeNotificationPermission's job).
invokedCommands.length = 0;
StubNotification.permission = "denied";
const originalWarnDenied = console.warn;
let deniedWarns = 0;
console.warn = () => {
  deniedWarns++;
};
try {
  await notifyDesktopPersistent({ body: "b", code: "c", target: "history" });
} finally {
  console.warn = originalWarnDenied;
}
assert.ok(
  invokedCommands.some((entry) => entry.cmd === "post_persistent_notice"),
  "the Rust seam decides: the command is invoked even when the grant is missing",
);
assert.equal(
  deniedWarns,
  1,
  "the seam's refusal is LOUD on this surface (logged once), not silent",
);
assert.ok(
  !invokedCommands.some(
    (entry) => entry.cmd === "plugin:notification|request_permission",
  ),
  "the persistent surface must never prompt for permission",
);
StubNotification.permission = "granted";

// ---------------------------------------------------------------------------
// 2 + 3. The click-through bus
//
// Two sources feed one funnel (dispatchNoticeNotificationClick):
// A. NATIVE: attachNativeNoticeClickSource listens for the
//    notice-notification-click event the macOS Rust sender emits on a real
//    notification click (src-tauri/src/persistent_notice.rs). This is the
//    production macOS path, exercised below by playing the event exactly
//    as Tauri delivers it through the event Channel.
// B. PLUGIN: attachNoticeNotificationClickSource wires the plugin's
//    DOCUMENTED onAction contract (onAction -> addPluginListener ->
//    Channel callback with the notification Options, extra included). The
//    installed desktop backend implements none of it (the reject block
//    below pins that reality), so on desktop that registration refuses and
//    the native source carries the feature; on platforms whose plugin
//    build delivers actionPerformed (mobile, future desktop builds) the
//    wiring below works unchanged.
// ---------------------------------------------------------------------------

// The bus delivers a click payload to subscribers.
const clicks: unknown[] = [];
const unsubscribe = subscribeNoticeNotificationClicks((click) => {
  clicks.push(click);
});

// Source A, the production macOS path: attach registers the Tauri event
// listener, then the Rust click leg's event arrives through the event
// Channel with the { code, target } payload shape persistent_notice.rs
// emits (pinned by its Rust unit test).
resetClickSourceLatchForTests();
await attachNativeNoticeClickSource();
assert.ok(
  invokedCommands.some(
    (entry) =>
      entry.cmd === "plugin:event|listen" &&
      (entry.args as { event?: string } | undefined)?.event ===
        "notice-notification-click",
  ),
  "the native click source must listen for the Rust event by its wire name",
);
const nativeHandler = channelCallbacks.get(nextCallbackId - 1);
assert.ok(nativeHandler, "attach must have registered an event handler");
let nativeEventIndex = 0;
const playNativeClickEvent = (payload: unknown): void => {
  // listen registers a plain transformCallback (not a Channel), so the
  // handler receives the Event object itself: { event, id, payload }.
  nativeEventIndex++;
  nativeHandler({
    event: "notice-notification-click",
    id: nativeEventIndex,
    payload,
  });
};
playNativeClickEvent({ code: "companion_session_capped", target: "history" });
assert.deepEqual(
  clicks,
  [{ code: "companion_session_capped", target: "history" }],
  "the native click event routes its payload to click subscribers",
);
// Garbage native payloads degrade to an empty click; nothing throws.
playNativeClickEvent(null);
assert.deepEqual(
  clicks[1],
  {},
  "a null native payload degrades to an empty click",
);

// Source B, the plugin contract path: attach resolves the registration,
// then the plugin fires the channel callback with the notification
// (Options) whose extra carries the routing payload.
registerListenerBehavior = "resolve";
await attachNoticeNotificationClickSource();
const pluginHandler = channelCallbacks.get(nextCallbackId - 1);
assert.ok(pluginHandler, "attach must have registered a plugin action handler");
let pluginEventIndex = 0;
const playActionEvent = (message: unknown): void => {
  pluginHandler({ index: pluginEventIndex++, message });
};
playActionEvent({
  title: "VoxBar",
  extra: { code: "recording_failed", target: "notifications" },
});
assert.deepEqual(
  clicks[2],
  { code: "recording_failed", target: "notifications" },
  "a plugin action event routes its extra payload to click subscribers",
);

// Garbage payloads degrade to an empty click; nothing throws.
playActionEvent({ title: "VoxBar" });
playActionEvent(null);
playActionEvent({ extra: "not-an-object" });
playActionEvent({ extra: { code: 42, target: null } });
assert.equal(clicks.length, 7, "every activation notifies subscribers");
assert.deepEqual(
  clicks[6],
  {},
  "non-string extra fields are dropped, not coerced",
);

// The dispatch funnel behaves identically for both payload shapes (it is
// what both sources funnel through).
dispatchNoticeNotificationClick({
  extra: { code: "paste_failed", target: "notifications" },
});
assert.deepEqual(clicks[7], {
  code: "paste_failed",
  target: "notifications",
});
dispatchNoticeNotificationClick({ code: "c", target: "history" });
assert.deepEqual(clicks[8], { code: "c", target: "history" });
dispatchNoticeNotificationClick(undefined);
assert.deepEqual(clicks[9], {});

// Unsubscribed listeners hear nothing further.
unsubscribe();
playActionEvent({ extra: { code: "paste_failed", target: "notifications" } });
assert.equal(clicks.length, 10, "an unsubscribed click listener hears nothing");

// The plugin platform-gap path (what the installed macOS desktop build
// actually does: no register_listener command exists in the plugin's Rust
// commands.rs, so both spellings reject): attach swallows it as an expected
// gap (info log, no throw) and the once-per-run latch prevents retry
// storms. The console is silenced because the info line is expected
// output. The native source above is unaffected and remains the macOS
// delivery path.
let infos = 0;
const originalInfo = console.info;
console.info = () => {
  infos++;
};
try {
  registerListenerBehavior = "reject";
  resetClickSourceLatchForTests();
  invokedCommands.length = 0;
  await attachNoticeNotificationClickSource();
  assert.equal(infos, 1, "the expected-gap note is logged exactly once");
  await attachNoticeNotificationClickSource();
  assert.equal(infos, 1, "the once-per-run latch suppresses re-attachment");
  assert.deepEqual(
    invokedCommands.map((entry) => entry.cmd),
    [
      "plugin:notification|register_listener",
      "plugin:notification|registerListener",
    ],
    "both plugin registration spellings are attempted before giving up",
  );
} finally {
  console.info = originalInfo;
}
registerListenerBehavior = "resolve";

// The native attach also degrades safely when the event listener cannot be
// registered: swallowed once, never thrown.
let nativeWarns = 0;
const originalWarn = console.warn;
console.warn = () => {
  nativeWarns++;
};
try {
  eventListenBehavior = "reject";
  resetClickSourceLatchForTests();
  await attachNativeNoticeClickSource();
  await attachNativeNoticeClickSource();
} finally {
  console.warn = originalWarn;
  eventListenBehavior = "resolve";
}
assert.equal(nativeWarns, 1, "a failed native attach warns exactly once");

// ---------------------------------------------------------------------------
// 4. The notification log
// ---------------------------------------------------------------------------

const logModule = await import("./notificationLog");
const {
  NOTIFICATION_LOG_CAP,
  appendNotificationLog,
  serializeNotificationLog,
  parseNotificationLog,
  initNotificationLog,
  getNotificationLog,
  pushNotificationLogEntry,
  clearNotificationLog,
  subscribeNotificationLog,
  resetNotificationLogForTests,
} = logModule;

// Pure append: newest first, hard cap at NOTIFICATION_LOG_CAP.
assert.equal(NOTIFICATION_LOG_CAP, 50, "the brief pins the log at ~50 entries");
resetNotificationLogForTests();
let entries: ReturnType<typeof getNotificationLog> = [];
for (let i = 0; i < 60; i++) {
  entries = appendNotificationLog(entries, {
    id: i + 1,
    code: "recording_failed",
    detail: `failure ${i + 1}`,
    kind: "error",
    receivedAt: 1_000 + i,
  });
}
assert.equal(entries.length, NOTIFICATION_LOG_CAP, "the log is capped");
assert.equal(entries[0].id, 60, "newest entry is first after the cap");
assert.equal(
  entries[NOTIFICATION_LOG_CAP - 1].id,
  11,
  "the 10 oldest entries were dropped, not the newest",
);

// Storage round-trip.
const roundTripped = parseNotificationLog(serializeNotificationLog(entries));
assert.deepEqual(roundTripped, entries, "serialize/parse round-trips");

// Corrupt storage degrades to an empty log, never a throw.
assert.deepEqual(parseNotificationLog("not json"), []);
assert.deepEqual(parseNotificationLog('{"a":1}'), []);
assert.deepEqual(parseNotificationLog('[42, "x", null]'), []);
assert.deepEqual(parseNotificationLog(null), []);
// Shape-checked salvage: valid rows survive, invalid rows are dropped.
assert.deepEqual(
  parseNotificationLog(
    JSON.stringify([
      { id: 1, code: "paste_failed", receivedAt: 5 },
      { id: 2, broken: true },
      { id: 3, code: "binding_busy", kind: "info", receivedAt: 7 },
    ]),
  ),
  [
    { id: 1, code: "paste_failed", detail: null, kind: "info", receivedAt: 5 },
    { id: 3, code: "binding_busy", detail: null, kind: "info", receivedAt: 7 },
  ],
  "persisted rows are salvaged field-by-field with defaults",
);

// The live store: a Map-based storage seam (no DOM in bun).
class MapStorage {
  private map = new Map<string, string>();
  getItem(key: string): string | null {
    return this.map.get(key) ?? null;
  }
  setItem(key: string, value: string): void {
    this.map.set(key, value);
  }
  removeItem(key: string): void {
    this.map.delete(key);
  }
}

resetNotificationLogForTests();
const storage = new MapStorage();
initNotificationLog(storage);
assert.deepEqual(getNotificationLog(), [], "fresh storage starts empty");

const logUpdates: number[] = [];
const unsubscribeLog = subscribeNotificationLog((next) => {
  logUpdates.push(next.length);
});

pushNotificationLogEntry({
  code: "post_process_cloud_failed",
  detail: "provider 500",
  kind: "error",
  receivedAt: 1234,
});
assert.deepEqual(logUpdates, [1], "subscribers hear appends");
assert.deepEqual(getNotificationLog(), [
  {
    id: 1,
    code: "post_process_cloud_failed",
    detail: "provider 500",
    kind: "error",
    receivedAt: 1234,
  },
]);
pushNotificationLogEntry({
  code: "companion_session_capped",
  detail: "Pixel 8",
  kind: "info",
  receivedAt: 1300,
});
assert.deepEqual(logUpdates, [1, 2]);
assert.equal(getNotificationLog()[0].id, 2, "ids are monotonic");

// Cap through the live store too (no reset: the subscriber above stays
// attached for the clear-all assertion below; the cap applies regardless).
for (let i = 0; i < NOTIFICATION_LOG_CAP + 5; i++) {
  pushNotificationLogEntry({
    code: "transcription_failed",
    kind: "error",
    receivedAt: i,
  });
}
assert.equal(
  getNotificationLog().length,
  NOTIFICATION_LOG_CAP,
  "the live store enforces the cap",
);

// Clear-all empties memory AND the persisted copy.
clearNotificationLog();
assert.deepEqual(getNotificationLog(), []);
assert.equal(storage.getItem("voxbar.notificationLog"), null);
assert.deepEqual(logUpdates.slice(-1), [0], "subscribers hear the clear");

// Persistence: a fresh store instance (the relaunch stand-in) hydrates
// what the previous run persisted through the same seam.
resetNotificationLogForTests();
initNotificationLog(storage); // binds the seam; storage was cleared above
assert.deepEqual(getNotificationLog(), []);
pushNotificationLogEntry({
  code: "companion_zero_audio",
  detail: "iPhone",
  kind: "error",
  receivedAt: 99,
});
assert.equal(
  storage.getItem("voxbar.notificationLog")?.includes("companion_zero_audio"),
  true,
  "appends persist through the bound seam",
);
resetNotificationLogForTests(); // the relaunch: memory gone, storage kept
initNotificationLog(storage);
assert.equal(
  getNotificationLog().length,
  1,
  "hydration restores the persisted tail",
);
assert.equal(getNotificationLog()[0].code, "companion_zero_audio");

unsubscribeLog();
resetNotificationLogForTests();

// ---------------------------------------------------------------------------
// 5. The prefs store
// ---------------------------------------------------------------------------

const prefsModule = await import("./notificationPrefs");
const {
  parseNotificationPrefs,
  initNotificationPrefs,
  getOsNotificationPrefs,
  setOsNotificationPrefs,
  subscribeOsNotificationPrefs,
  resetNotificationPrefsForTests,
} = prefsModule;

// Malformed stored values fall back to the shipped defaults (on + on).
assert.deepEqual(parseNotificationPrefs(null), {
  enabled: true,
  outcomes: true,
});
assert.deepEqual(parseNotificationPrefs("garbage"), {
  enabled: true,
  outcomes: true,
});
assert.deepEqual(parseNotificationPrefs("[1,2]"), {
  enabled: true,
  outcomes: true,
});
assert.deepEqual(parseNotificationPrefs('{"enabled":"yes"}'), {
  enabled: true,
  outcomes: true,
});
assert.deepEqual(parseNotificationPrefs('{"enabled":false,"outcomes":false}'), {
  enabled: false,
  outcomes: false,
});

// The live store: hydrate, read, write, persist, subscribe.
resetNotificationPrefsForTests();
const prefsStorage = new MapStorage();
prefsStorage.setItem(
  "voxbar.notificationPrefs",
  '{"enabled":false,"outcomes":true}',
);
initNotificationPrefs(prefsStorage);
assert.deepEqual(getOsNotificationPrefs(), {
  enabled: false,
  outcomes: true,
});

const prefUpdates: unknown[] = [];
const unsubscribePrefs = subscribeOsNotificationPrefs((next) => {
  prefUpdates.push(next);
});
setOsNotificationPrefs({ enabled: true, outcomes: false });
assert.deepEqual(getOsNotificationPrefs(), { enabled: true, outcomes: false });
assert.deepEqual(prefUpdates, [{ enabled: true, outcomes: false }]);
assert.equal(
  prefsStorage.getItem("voxbar.notificationPrefs"),
  '{"enabled":true,"outcomes":false}',
  "writes persist to storage",
);
unsubscribePrefs();
setOsNotificationPrefs({ enabled: true, outcomes: true });
assert.equal(
  prefUpdates.length,
  1,
  "unsubscribed prefs listeners hear nothing",
);
resetNotificationPrefsForTests();

console.log("noticeNotificationSurface tests passed");
