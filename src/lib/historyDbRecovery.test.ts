import assert from "node:assert/strict";
import {
  postHistoryDbRecoveryNoticeOnce,
  shouldPostRecoveryNotice,
  type HistoryDbRecoveryInfo,
} from "./historyDbRecovery";

// KB-179: the launch-time history-database recovery notice.
//
// The backend quarantines an unreadable history.db before any webview
// exists, so the frontend PULLS the recovery record once on mount and
// posts one persistent notice naming the quarantined file. These pins
// cover the pure decision seam and the end-to-end post through the same
// Tauri-invoke stub desktopNotify.test.ts uses (bun scripts have no
// window global; the stub stands in for the IPC bridge).

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
invokedCommands.length = 0;
recoveryRecord = null;
await postHistoryDbRecoveryNoticeOnce(translate);
assert.deepEqual(invokedCommands, ["get_history_db_recovery"]);
assert.deepEqual(persistentPosts, []);

// A recovery: the notice posts through the persistent surface with the
// notification click routing that lands on the History section.
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

// The once-per-session flag: a remount (or a second surface mounting the
// same effect) pulls nothing and posts nothing again.
invokedCommands.length = 0;
await postHistoryDbRecoveryNoticeOnce(translate);
assert.deepEqual(invokedCommands, []);
assert.equal(persistentPosts.length, 1);

console.log("historyDbRecovery: all assertions passed");
