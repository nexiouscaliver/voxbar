/**
 * The module-level Prompt Studio batch controller (KB-257): the batch
 * state lives OUTSIDE the React component because the studio is a
 * settings page the operator can leave at any instant while a cloud run
 * is still grinding (up to the provider's timeout). Component state died
 * with the unmount, so the still-running batch turned invisible, a
 * second Run stacked a NEW batch behind the orphaned one, and nothing
 * could cancel either. The controller owns the in-flight flag, the card
 * grid, and the run/cancel entry points; the component subscribes on
 * mount and reflects whatever is live on first mount and every remount.
 *
 * Kept free of React (a plain subscribe/getState store usable with
 * useSyncExternalStore) and of i18n (errors store their structured shape;
 * the component localizes for display) so the semantics stay
 * unit-testable the same way studioRunState is.
 */
import type { StudioSource, TestPromptError } from "@/bindings";
import { commands } from "@/bindings";
import {
  initCards,
  markRunning,
  mergeResults,
  settleUnfinished,
  type StudioCardState,
  type StudioPair,
} from "./studioRunState";

/** The structured batch error: the command's typed error, or the raw
 * string of an invoke-level failure (transport). */
export type StudioBatchError =
  | { kind: "test_prompt"; error: TestPromptError }
  | { kind: "raw"; message: string };

export interface StudioBatchState {
  /** True while a batch command is awaited: disables Run/Re-run and the
   * pickers on every mount (KB-257: no stacking behind an orphan). */
  inFlight: boolean;
  /** The card grid; survives remounts because it lives here. */
  cards: StudioCardState[];
  /** The last batch's error, or null. */
  error: StudioBatchError | null;
  /** True once the operator cancelled the in-flight (or last) batch. */
  cancelled: boolean;
}

const initial: StudioBatchState = {
  inFlight: false,
  cards: [],
  error: null,
  cancelled: false,
};

let state: StudioBatchState = initial;
const listeners = new Set<() => void>();

const set = (patch: Partial<StudioBatchState>): void => {
  state = { ...state, ...patch };
  for (const listener of listeners) listener();
};

/** Subscribe to batch state changes (useSyncExternalStore's contract). */
export const subscribeStudioBatch = (listener: () => void): (() => void) => {
  listeners.add(listener);
  return () => listeners.delete(listener);
};

/** The current batch state snapshot (stable reference between changes). */
export const studioBatchState = (): StudioBatchState => state;

/** Test-only: reset the controller to its pristine state. */
export const resetStudioBatchForTests = (): void => {
  state = initial;
};

/**
 * Run one batch (KB-257). Refuses while a batch is in flight - the
 * previous behavior of letting a second batch stack behind an unmounted
 * UI is exactly the bug. `fresh` resets the grid to the new pairs; a
 * re-run refreshes only its own cards.
 */
export const runStudioBatch = (
  pairs: readonly StudioPair[],
  source: StudioSource,
  fresh: boolean,
): void => {
  if (state.inFlight) return;
  set({
    inFlight: true,
    cancelled: false,
    error: null,
    cards: fresh ? initCards(pairs) : markRunning(state.cards, pairs),
  });
  commands
    .testPostProcessPromptBatch(
      source,
      pairs.map((pair) => ({
        prompt_id: pair.promptId,
        provider_id: pair.providerId,
      })),
    )
    .then((result) => {
      if (result.status === "ok") {
        set({ cards: mergeResults(state.cards, result.data) });
      } else {
        // A refused batch must not destroy finished results: a fresh run
        // resets to its idle cards, a re-run keeps the grid untouched.
        set({
          cards: fresh ? initCards(pairs) : state.cards,
          error: { kind: "test_prompt", error: result.error },
        });
      }
    })
    .catch((error) => {
      console.error("Studio batch failed:", error);
      set({
        cards: fresh ? initCards(pairs) : state.cards,
        error: { kind: "raw", message: String(error) },
      });
    })
    .finally(() => {
      // KB-257: every card still running never got its result - with a
      // cancel on record that is "cancelled before its turn".
      set({
        cards: settleUnfinished(state.cards, state.cancelled),
        inFlight: false,
      });
    });
};

/**
 * KB-257: the cancel affordance's backend leg. Bumps the backend's batch
 * generation so the in-flight command stops before its next run and
 * returns the results it already collected; the cancelled flag flips the
 * still-running cards immediately (the command's own resolution settles
 * the grid a moment later).
 */
export const cancelStudioBatch = (): void => {
  if (!state.inFlight) return;
  set({ cancelled: true });
  commands
    .cancelPostProcessPromptBatch()
    .catch((error) => console.error("Studio batch cancel failed:", error));
};
