import type { AppSettings as Settings } from "@/bindings";

// W1-L4 hydration guard: the stash-and-replay queue for updateSetting calls
// that land while the store is still unhydrated (settings === null).
//
// Why this exists: before the guard, a pre-hydration updateSetting performed
// no optimistic write (the spread kept settings null) but STILL ran the
// backend updater. The boot-time getAppSettings roundtrip was usually
// already in flight, so the stale response then overwrote the store with
// the pre-write value and the user's change vanished from the UI even
// though the backend had accepted it. The store now stashes the intent
// here instead of touching the backend, and replays it through the normal
// updateSetting path once hydration completes.
//
// This module is deliberately pure (no store access, no invokes, no
// console): it decides what is retained and in what order, the store
// performs the effects. Same split as settingsWriteOutcome / updateCheckGate.

/** Distinct-key bound for the stash. Repeat updates to an already-stashed
 * key never count against it (last write per key wins), so the bound caps
 * memory and replay work, not a user rapidly refining one setting. 16 is
 * far above any realistic pre-hydration burst (the window is one boot
 * roundtrip) while keeping a runaway caller (a render-loop bug firing
 * updateSetting on distinct keys before hydration) bounded and visible.
 */
export const PRE_HYDRATION_STASH_LIMIT = 16;

export type StashOutcome =
  | { stashed: true }
  | { stashed: false; reason: "limit" };

/** One replayable intent: the setting key and the latest value for it. */
export type PreHydrationReplayEntry = [key: keyof Settings, value: unknown];

export class PreHydrationUpdateQueue {
  #entries = new Map<keyof Settings, unknown>();

  /** Distinct stashed keys (the map size, exposed for the warn log). */
  get size(): number {
    return this.#entries.size;
  }

  /** Retain an intent for replay. Overwriting an existing key is always
   * allowed (the old value is discarded, last write wins); adding a NEW
   * key beyond PRE_HYDRATION_STASH_LIMIT is refused so the queue stays
   * bounded. The caller decides how to surface a refusal.
   */
  stash<K extends keyof Settings>(key: K, value: Settings[K]): StashOutcome {
    if (
      !this.#entries.has(key) &&
      this.#entries.size >= PRE_HYDRATION_STASH_LIMIT
    ) {
      return { stashed: false, reason: "limit" };
    }
    this.#entries.set(key, value);
    return { stashed: true };
  }

  /** Take every retained intent for replay, in first-touch order with the
   * latest value per key, and empty the queue. Taking (not peeking) makes
   * replay one-shot: a second hydration or settings-changed refresh finds
   * an empty queue and cannot double-apply.
   */
  drain(): PreHydrationReplayEntry[] {
    const drained = [...this.#entries.entries()];
    this.#entries.clear();
    return drained;
  }
}
