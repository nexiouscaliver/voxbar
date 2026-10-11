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
//    carrying the click-through routing payload.
// 2. attachNoticeNotificationClickSource wires the plugin's onAction into
//    the bus; a platform without plugin action events (the tauri-plugin-
//    notification 2.4.0 desktop build) rejects the registration, which is
//    swallowed as an expected gap, once per run.
// 3. Click-through: a plugin action event carrying extra { code, target }
//    reaches subscribers as { code, target }; garbage payloads degrade to
//    an empty click, never a throw; unsubscribed listeners hear nothing.
// 4. The notification log: newest-first append, hard cap at the last 50,
//    full clear, and a storage round-trip that survives corrupt input.
// 5. The prefs store: defaults, coercion of malformed stored values, and
//    the write+persist path.

// ---------------------------------------------------------------------------
// The Tauri stub
// ---------------------------------------------------------------------------

interface RecordedNotification {
  title: string;
  body?: string;
  group?: string;
  extra?: Record<string, unknown>;
}

const sent: RecordedNotification[] = [];
const invokedCommands: string[] = [];

class StubNotification {
  static permission: "default" | "granted" | "denied" = "granted";
  static requestPermission(): Promise<"default" | "granted" | "denied"> {
    return Promise.resolve(StubNotification.permission);
  }
  constructor(_title: string, options: RecordedNotification) {
    sent.push({ ...options });
  }
}

// The channel callbacks the plugin registers (transformCallback): keyed by
// id so the test can PLAY a real plugin action event into the wiring.
const channelCallbacks = new Map<number, (message: unknown) => void>();
let nextCallbackId = 1;

let windowVisible = true;
let registerListenerBehavior: "resolve" | "reject" = "reject";

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
    invoke: async (cmd: string): Promise<unknown> => {
      invokedCommands.push(cmd);
      if (cmd === "plugin:window|is_visible") return windowVisible;
      if (cmd === "plugin:notification|is_permission_granted")
        return StubNotification.permission === "granted";
      if (
        cmd === "plugin:notification|register_listener" ||
        cmd === "plugin:notification|registerListener"
      ) {
        if (registerListenerBehavior === "reject") {
          throw new Error(`stub: ${cmd} not registered on desktop`);
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
  attachNoticeNotificationClickSource,
  subscribeNoticeNotificationClicks,
  dispatchNoticeNotificationClickForTests,
  resetClickSourceLatchForTests,
} = desktop;

// ---------------------------------------------------------------------------
// 1. The persistent surface is visibility-independent
// ---------------------------------------------------------------------------

// Window VISIBLE: the persistent surface still posts (this is the point of
// the surface). With the web Notification permission already "granted" the
// permission check short-circuits on the static property, so the IPC log
// stays empty - and in particular there is deliberately NO
// plugin:window|is_visible roundtrip here (that query is the legacy
// surface's gate, pinned below).
invokedCommands.length = 0;
sent.length = 0;
windowVisible = true;
await notifyDesktopPersistent({
  body: "Polishing failed and the raw transcript was kept",
  code: "post_process_cloud_failed",
  target: "notifications",
});
assert.deepEqual(
  sent,
  [
    {
      title: "VoxBar",
      body: "Polishing failed and the raw transcript was kept",
      group: "voxbar-notices",
      extra: { code: "post_process_cloud_failed", target: "notifications" },
    },
  ],
  "the persistent post carries the routing payload under the product title",
);
assert.ok(
  !invokedCommands.includes("plugin:window|is_visible"),
  "the persistent surface must not query window visibility (contrast notifyDesktop)",
);

// Window HIDDEN: identical post (visibility is simply not an input).
sent.length = 0;
windowVisible = false;
await notifyDesktopPersistent({
  body: "Companion server failed",
  code: "companion_server_failed",
  target: "notifications",
});
assert.equal(sent.length, 1, "hidden window posts the same persistent notice");

// Contrast pin: the legacy notifyDesktop surface DOES gate on visibility
// (pre-existing behavior this file keeps honest).
sent.length = 0;
invokedCommands.length = 0;
windowVisible = true;
await desktop.notifyDesktop("transient banner");
assert.deepEqual(sent, [], "legacy surface stays quiet while visible");

// Permission denied: silent, no prompt (a notification is a courtesy
// surface; priming is primeNotificationPermission's job).
sent.length = 0;
invokedCommands.length = 0;
StubNotification.permission = "denied";
await notifyDesktopPersistent({ body: "b", code: "c", target: "history" });
assert.deepEqual(sent, [], "denied permission means no post");
assert.ok(
  !invokedCommands.includes("plugin:notification|request_permission"),
  "the persistent surface must never prompt for permission",
);
StubNotification.permission = "granted";

// ---------------------------------------------------------------------------
// 2 + 3. The click-through bus
// ---------------------------------------------------------------------------

// The bus delivers a plugin action event's extra payload to subscribers.
const clicks: unknown[] = [];
const unsubscribe = subscribeNoticeNotificationClicks((click) => {
  clicks.push(click);
});

// Play a REAL plugin action event through the wiring: attach resolves the
// registration, then the plugin fires the channel callback with the
// notification (Options) whose extra carries the routing payload.
registerListenerBehavior = "resolve";
resetClickSourceLatchForTests();
await attachNoticeNotificationClickSource();
const handler = channelCallbacks.get(nextCallbackId - 1);
assert.ok(handler, "attach must have registered a plugin action handler");
// The Channel callback receives { index, message } envelopes (message order
// is part of the Channel contract), so events are played in that shape.
let eventIndex = 0;
const playActionEvent = (message: unknown): void => {
  handler({ index: eventIndex++, message });
};
playActionEvent({
  title: "VoxBar",
  extra: { code: "companion_session_capped", target: "history" },
});
assert.deepEqual(
  clicks,
  [{ code: "companion_session_capped", target: "history" }],
  "a plugin action event routes its extra payload to click subscribers",
);

// Garbage payloads degrade to an empty click; nothing throws.
playActionEvent({ title: "VoxBar" });
playActionEvent(null);
playActionEvent({ extra: "not-an-object" });
playActionEvent({ extra: { code: 42, target: null } });
assert.equal(clicks.length, 5, "every activation notifies subscribers");
assert.deepEqual(
  clicks[4],
  {},
  "non-string extra fields are dropped, not coerced",
);

// The dispatch seam behaves identically (it is what the plugin callback
// funnels through).
dispatchNoticeNotificationClickForTests({
  extra: { code: "recording_failed", target: "notifications" },
});
assert.deepEqual(clicks[5], {
  code: "recording_failed",
  target: "notifications",
});
dispatchNoticeNotificationClickForTests(undefined);
assert.deepEqual(clicks[6], {});

// Unsubscribed listeners hear nothing further.
unsubscribe();
playActionEvent({ extra: { code: "paste_failed", target: "notifications" } });
assert.equal(clicks.length, 7, "an unsubscribed click listener hears nothing");

// The platform-gap path (today's macOS desktop build): the plugin rejects
// the registration; attach swallows it as an expected gap (info log, no
// throw) and the once-per-run latch prevents retry storms. The console is
// silenced because the info line is expected output.
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
    invokedCommands,
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
