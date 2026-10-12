// KB-179: the launch-time history-database recovery notice.
//
// When the stored history.db is corrupted or unreadable, the backend
// quarantines it (history.db.corrupt, timestamped when that name is taken),
// starts a fresh database, and records the recovery. The record cannot be
// PUSHED: it happens during app setup, before any webview listener exists,
// so the frontend PULLS it once on mount (the get_history_db_recovery
// command) and posts one persistent Notification Center notice naming the
// quarantined file. The once-per-session flag keeps a window reload (or a
// second surface mounting this effect) from re-posting the same recovery.

import { commands, type HistoryDbRecovery } from "@/bindings";
import { notifyDesktopPersistent } from "./desktopNotify";

// The translate function react-i18next's useTranslation hands to
// components, typed structurally as the other shared modules take it.
export type RecoveryTranslateFn = (
  key: string,
  options?: Record<string, unknown>,
) => string;

/** The recovery record's wire shape the pure seam reasons over (the
 * generated HistoryDbRecovery type, field-for-field). */
export interface HistoryDbRecoveryInfo {
  /** The file name the unreadable database was quarantined under. */
  quarantined_file: string;
}

let postedThisSession = false;

/** Pure decision seam: post only for an actual recovery that has not been
 * posted yet (unit-tested in historyDbRecovery.test.ts). */
export const shouldPostRecoveryNotice = (
  recovery: HistoryDbRecoveryInfo | null,
  alreadyPosted: boolean,
): boolean => recovery !== null && !alreadyPosted;

/** Pull the recovery record once per session and post the persistent
 * notice when one exists. Never throws; a failed command read just means
 * no notice (the recovery is still visible in the app log). */
export async function postHistoryDbRecoveryNoticeOnce(
  t: RecoveryTranslateFn,
): Promise<void> {
  if (postedThisSession) return;
  let recovery: HistoryDbRecovery | null = null;
  try {
    const result = await commands.getHistoryDbRecovery();
    if (result.status === "ok") {
      recovery = result.data ?? null;
    }
  } catch (error) {
    console.warn("History database recovery check failed:", error);
    return;
  }
  if (
    recovery === null ||
    !shouldPostRecoveryNotice(recovery, postedThisSession)
  ) {
    return;
  }
  postedThisSession = true;
  await notifyDesktopPersistent({
    body: t("settings.history.dbRecoveredNotice", {
      file: recovery.quarantined_file,
    }),
    code: "history_db_recovered",
    target: "history",
  });
}
