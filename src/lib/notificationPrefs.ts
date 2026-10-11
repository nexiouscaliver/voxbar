// v1.6.0 W1-L4: the OS-notification preferences store.
//
// The pure input shape (OsNotificationPrefs) and its default live in
// noticeRouting.ts next to the classification that consumes them; this
// module is the live store plus localStorage persistence, following the
// windowVisibility.ts subscribe pattern (pure store, importable in bun
// with no DOM work at import time).
//
// Why localStorage and not AppSettings: the app's settings object is owned
// by the Rust side (bindings.ts AppSettings), and this lane does not touch
// Rust; the app already ships the same pattern for boot-time preferences
// (accent/theme pre-hydration, lib/utils/accent.ts). Default ON: errors
// and outcomes notify out of the box; turning the toggle off restores the
// pre-v1.6.0 notification behavior exactly (see planNoticeDispatch).

import {
  DEFAULT_OS_NOTIFICATION_PREFS,
  type OsNotificationPrefs,
} from "./noticeRouting";

// Re-exported so prefs consumers have a single import point.
export type { OsNotificationPrefs };

const STORAGE_KEY = "voxbar.notificationPrefs";

/** The localStorage-like seam (tests inject a plain map). */
export type NotificationPrefsStorage = Pick<Storage, "getItem" | "setItem">;

let prefs: OsNotificationPrefs = { ...DEFAULT_OS_NOTIFICATION_PREFS };
const listeners = new Set<(next: OsNotificationPrefs) => void>();

// Same seam discipline as notificationLog.ts: the global localStorage when
// it exists, an explicit test seam once passed, null in non-DOM contexts.
let storageSeam: NotificationPrefsStorage | null | undefined;

function boundStorage(): NotificationPrefsStorage | null {
  try {
    const storage = (globalThis as { localStorage?: Storage }).localStorage;
    return storage ?? null;
  } catch {
    return null;
  }
}

function resolveStorage(): NotificationPrefsStorage | null {
  if (storageSeam === undefined) storageSeam = boundStorage();
  return storageSeam;
}

/** Coerce unknown stored JSON to valid prefs; anything malformed is the default. */
export function parseNotificationPrefs(
  raw: string | null,
): OsNotificationPrefs {
  if (typeof raw !== "string" || raw.length === 0) {
    return { ...DEFAULT_OS_NOTIFICATION_PREFS };
  }
  try {
    const parsed: unknown = JSON.parse(raw);
    if (typeof parsed !== "object" || parsed === null) {
      return { ...DEFAULT_OS_NOTIFICATION_PREFS };
    }
    const candidate = parsed as Partial<OsNotificationPrefs>;
    return {
      enabled:
        typeof candidate.enabled === "boolean"
          ? candidate.enabled
          : DEFAULT_OS_NOTIFICATION_PREFS.enabled,
      outcomes:
        typeof candidate.outcomes === "boolean"
          ? candidate.outcomes
          : DEFAULT_OS_NOTIFICATION_PREFS.outcomes,
    };
  } catch {
    return { ...DEFAULT_OS_NOTIFICATION_PREFS };
  }
}

/** Hydrate once at boot; never throws. An explicit storage seam (tests)
 *  also sticks for subsequent writes. */
export function initNotificationPrefs(
  storage?: NotificationPrefsStorage,
): void {
  storageSeam = storage ?? boundStorage();
  const bound = storageSeam;
  if (!bound) return;
  try {
    prefs = parseNotificationPrefs(bound.getItem(STORAGE_KEY));
  } catch {
    prefs = { ...DEFAULT_OS_NOTIFICATION_PREFS };
  }
}

/** Current prefs (fresh read; emit-time callers must not cache). */
export function getOsNotificationPrefs(): OsNotificationPrefs {
  return { ...prefs };
}

/** Update (whole-object replace), persist, and notify subscribers. */
export function setOsNotificationPrefs(next: OsNotificationPrefs): void {
  prefs = {
    enabled: next.enabled === true,
    outcomes: next.outcomes === true,
  };
  const storage = resolveStorage();
  if (storage) {
    try {
      storage.setItem(STORAGE_KEY, JSON.stringify(prefs));
    } catch {
      // Storage unavailable: the preference still applies for this run.
    }
  }
  for (const listener of [...listeners]) {
    listener(getOsNotificationPrefs());
  }
}

/** Hear future changes only (no replay of the current value). */
export function subscribeOsNotificationPrefs(
  listener: (next: OsNotificationPrefs) => void,
): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** Test-only: reset to defaults between scenarios. */
export function resetNotificationPrefsForTests(): void {
  prefs = { ...DEFAULT_OS_NOTIFICATION_PREFS };
  listeners.clear();
  storageSeam = undefined;
}
