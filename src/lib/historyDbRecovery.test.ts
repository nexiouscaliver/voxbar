import assert from "node:assert/strict";
import {
  postHistoryDbRecoveryNoticeOnce,
  recoveryLogRowsForTests,
  resetRecoveryLatchForTests,
  shouldPostRecoveryNotice,
  type HistoryDbRecoveryInfo,
} from "./historyDbRecovery";
import {
  resetNotificationPrefsForTests,
  setOsNotificationPrefs,
} from "./notificationPrefs";
import { resetNotificationLogForTests } from "./notificationLog";

// KB-179: the launch-time history-database recovery notice.
//
// The backend quarantines an unreadable history.db before any webview
// exists, so the frontend PULLS the recovery record once on mount and
// routes it through the SAME discipline as every other persistent notice.
// These pins cover the pure decision seam and the end-to-end dispatch
// through the same Tauri-invoke stub desktopNotify.test.ts uses (bun
// scripts have no window global; the stub stands in for the IPC bridge).
//
// Fleet pass 2: the master toggle gates BOTH durable legs (no OS post and
// no log write while off), and the in-app notification log row rides with
// the post while on - the backstop that survives a denied macOS
// notification permission.

interface PersistentNoticeCall {
  body: string;
  code: string;
  target: string;
}

let recoveryRecord: HistoryDbRecoveryInfo | null = null;
const persistentPosts: PersistentNoticeCall[] = [];
const invokedCommands: string[] = [];

const stubWindow = {
  __TAURI_INTERNALS__: {
    metadata: { currentWindow: { label: "main" } },
    invoke: async (cmd: string, args?: Record<string, unknown>) => {
      invokedCommands.push(cmd);
      if (cmd === "get_history_db_recovery") {
        return recoveryRecord;
      }
      if (cmd === "post_persistent_notice") {
        persistentPosts.push({
          body: String(args?.body ?? ""),
          code: String(args?.code ?? ""),
          target: String(args?.target ?? ""),
        });
        return null;
      }
      return null;
    },
  },
};

(globalThis as unknown as { window: typeof stubWindow }).window = stubWindow;

const translate = (key: string, options?: Record<string, unknown>) =>
  key === "settings.history.dbRecoveredNotice"
    ? `recovered notice about ${String(options?.file ?? "")}`
    : key;

// The pure decision seam: post only for an actual recovery that has not
// been posted yet.
const sample: HistoryDbRecoveryInfo = {
  quarantined_file: "history.db.corrupt",
};
assert.equal(shouldPostRecoveryNotice(sample, false), true);
assert.equal(shouldPostRecoveryNotice(sample, true), false, "posted already");
assert.equal(shouldPostRecoveryNotice(null, false), false, "no recovery");
assert.equal(shouldPostRecoveryNotice(null, true), false);

// No recovery on this launch: one command pull, nothing posted.
resetNotificationPrefsForTests();
resetNotificationLogForTests();
resetRecoveryLatchForTests();
invokedCommands.length = 0;
persistentPosts.length = 0;
recoveryRecord = null;
await postHistoryDbRecoveryNoticeOnce(translate);
assert.deepEqual(invokedCommands, ["get_history_db_recovery"]);
assert.deepEqual(persistentPosts, []);
assert.equal(recoveryLogRowsForTests(), 0);

// A recovery with the notifications master toggle ON (the default): the
// in-app log row is written FIRST (the backstop that survives a denied
// macOS notification permission) and the persistent notice posts with the
// click routing that lands on the History section.
resetRecoveryLatchForTests();
invokedCommands.length = 0;
recoveryRecord = { quarantined_file: "history.db.corrupt.1700000000" };
await postHistoryDbRecoveryNoticeOnce(translate);
assert.deepEqual(persistentPosts, [
  {
    body: "recovered notice about history.db.corrupt.1700000000",
    code: "history_db_recovered",
    target: "history",
  },
]);
assert.equal(
  recoveryLogRowsForTests(),
  1,
  "the recovery must leave an in-app notification log row: with macOS notifications denied, that row is the only visible surface",
);

// The once-per-session flag: a remount (or a second surface mounting the
// same effect) pulls nothing, posts nothing, and writes no second log
// row.
invokedCommands.length = 0;
await postHistoryDbRecoveryNoticeOnce(translate);
assert.deepEqual(invokedCommands, []);
assert.equal(persistentPosts.length, 1);
assert.equal(recoveryLogRowsForTests(), 1);

// Fleet pass 2: the notifications master toggle OFF restores the off
// contract exactly - no persistent post AND no log write (the log is part
// of the notification system; the off path adds no durable state).
resetRecoveryLatchForTests();
resetNotificationLogForTests();
setOsNotificationPrefs({ enabled: false, outcomes: true });
invokedCommands.length = 0;
persistentPosts.length = 0;
recoveryRecord = { quarantined_file: "history.db.corrupt.1800000000" };
await postHistoryDbRecoveryNoticeOnce(translate);
assert.deepEqual(persistentPosts, [], "the master toggle off means no OS post");
assert.equal(
  recoveryLogRowsForTests(),
  0,
  "the master toggle off means no log write either",
);
// The pull still happened (the once-per-session latch consumed the record
// either way).
assert.deepEqual(invokedCommands, ["get_history_db_recovery"]);

// Scenario cleanup for any test that runs after this file.
resetNotificationPrefsForTests();
resetNotificationLogForTests();
resetRecoveryLatchForTests();

console.log("historyDbRecovery: all assertions passed");
