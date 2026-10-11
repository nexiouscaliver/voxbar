// v1.6.0 W1-L4: the in-app notification log.
//
// The durable in-app backstop for notices that route to the persistent
// OS-notification surface (channel "notification"/"both" in
// noticeRouting.ts): every such notice is appended here too, so a missed
// toast (or a dismissed OS notification) is never lost. Kept in memory for
// the run and mirrored to localStorage so a relaunch still shows the tail;
// capped at the last NOTIFICATION_LOG_CAP entries, newest first.
//
// Pure store by design (the windowVisibility.ts pattern): importable in
// bun with no Tauri or DOM work at import time. Storage is a structural
// seam (localStorage-like) so tests drive persistence with a plain map.
//
// Scope note: the log records every error/outcome-classified notice
// regardless of the OS-notification settings toggles. The toggles govern
// the OS surface only; the log is the in-app list and stays populated even
// with notifications off (it then becomes the replacement surface).

export interface NotificationLogEntry {
  /** Monotonic id so React keys survive identical repeated notices. */
  id: number;
  code: string;
  /** Diagnostic detail (error text, device name); kept in-app only. */
  detail?: string | null;
  /** The event's tone, for row styling. */
  kind: "error" | "info";
  /** Epoch ms when the notice arrived. */
  receivedAt: number;
}

/** The log is capped: last ~50 entries, the brief's "missed toast" span. */
export const NOTIFICATION_LOG_CAP = 50;

/** The localStorage-like seam the store binds to lazily at call time. */
export type NotificationLogStorage = Pick<
  Storage,
  "getItem" | "setItem" | "removeItem"
>;

const STORAGE_KEY = "voxbar.notificationLog";

// ---- Pure helpers (unit-tested directly) ----

// Append one entry to the front of the log and cap the tail. Pure: returns
// a new array, never mutates the input.
export function appendNotificationLog(
  entries: readonly NotificationLogEntry[],
  entry: NotificationLogEntry,
): NotificationLogEntry[] {
  return [entry, ...entries].slice(0, NOTIFICATION_LOG_CAP);
}

// Serialize/deserialize for localStorage. Corrupt or shape-mismatched
// payloads yield an empty log rather than throwing (storage is a courtesy
// surface; the in-memory log is the source of truth for the run).
export function serializeNotificationLog(
  entries: readonly NotificationLogEntry[],
): string {
  return JSON.stringify(entries);
}

export function parseNotificationLog(
  raw: string | null,
): NotificationLogEntry[] {
  if (typeof raw !== "string" || raw.length === 0) return [];
  try {
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    const entries: NotificationLogEntry[] = [];
    for (const item of parsed) {
      if (
        typeof item === "object" &&
        item !== null &&
        typeof (item as NotificationLogEntry).code === "string" &&
        typeof (item as NotificationLogEntry).receivedAt === "number"
      ) {
        const typed = item as NotificationLogEntry;
        entries.push({
          id: typeof typed.id === "number" ? typed.id : 0,
          code: typed.code,
          detail: typed.detail ?? null,
          kind: typed.kind === "error" ? "error" : "info",
          receivedAt: typed.receivedAt,
        });
      }
    }
    return entries.slice(0, NOTIFICATION_LOG_CAP);
  } catch {
    return [];
  }
}

// ---- Live store (the windowVisibility.ts subscribe pattern) ----

let log: NotificationLogEntry[] = [];
let nextId = 1;
const listeners = new Set<(entries: NotificationLogEntry[]) => void>();

// The lazy storage binding: globalThis.localStorage when it exists, null in
// non-DOM contexts (bun tests) where the log stays memory-only. Once a
// caller passes an explicit seam (tests), that seam sticks for later
// writes; undefined means "not resolved yet".
let storageSeam: NotificationLogStorage | null | undefined;

function boundStorage(): NotificationLogStorage | null {
  try {
    const storage = (globalThis as { localStorage?: Storage }).localStorage;
    return storage ?? null;
  } catch {
    return null;
  }
}

function resolveStorage(): NotificationLogStorage | null {
  if (storageSeam === undefined) storageSeam = boundStorage();
  return storageSeam;
}

function persist(): void {
  const storage = resolveStorage();
  if (!storage) return;
  try {
    storage.setItem(STORAGE_KEY, serializeNotificationLog(log));
  } catch {
    // Storage full or unavailable: the in-memory log keeps working.
  }
}

/** Hydrate the log from storage once at boot; never throws. An explicit
 *  storage seam (tests) also sticks for subsequent writes. */
export function initNotificationLog(storage?: NotificationLogStorage): void {
  storageSeam = storage ?? boundStorage();
  const bound = storageSeam;
  if (!bound) return;
  try {
    log = parseNotificationLog(bound.getItem(STORAGE_KEY));
    nextId = log.reduce((max, entry) => Math.max(max, entry.id), 0) + 1;
  } catch {
    log = [];
  }
}

/** The current log, newest first. Returns a defensive copy. */
export function getNotificationLog(): NotificationLogEntry[] {
  return [...log];
}

/** Record one notice (classification already decided by the caller). */
export function pushNotificationLogEntry(
  entry: Omit<NotificationLogEntry, "id">,
): NotificationLogEntry[] {
  const full: NotificationLogEntry = { ...entry, id: nextId++ };
  log = appendNotificationLog(log, full);
  persist();
  for (const listener of [...listeners]) {
    listener(getNotificationLog());
  }
  return log;
}

/** The clear-all action: empties the log and the persisted copy. */
export function clearNotificationLog(): void {
  log = [];
  const storage = resolveStorage();
  if (storage) {
    try {
      storage.removeItem(STORAGE_KEY);
    } catch {
      // ignore: memory log is already cleared
    }
  }
  for (const listener of [...listeners]) {
    listener(getNotificationLog());
  }
}

/** Hear future log changes; returns the unsubscribe function. */
export function subscribeNotificationLog(
  listener: (entries: NotificationLogEntry[]) => void,
): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** Test-only: reset the store between scenarios. */
export function resetNotificationLogForTests(): void {
  log = [];
  nextId = 1;
  listeners.clear();
  storageSeam = undefined;
}
