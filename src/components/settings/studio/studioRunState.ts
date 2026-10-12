/**
 * Pure state for the Prompt Studio screen (v1.6.0 W1-L6): batch building,
 * card merging, and outcome classification, kept free of React and Tauri
 * so a plain bun script can unit test the loading / partial-failure
 * semantics (studioRunState.test.ts), matching the repo's pure-module
 * convention (noticeRouting, skipToastDedupe).
 *
 * Type-only imports from the bindings: nothing here touches Tauri at
 * runtime, which is exactly why the test file can execute it directly.
 */
import type { PromptTestOutcome, StudioRunRequest } from "@/bindings";

/** One (template, engine) pair the studio can run. */
export interface StudioPair {
  promptId: string;
  providerId: string;
}

/** Stable identity for a pair: card keys and result merging both use it. */
export const pairKey = (pair: StudioPair): string =>
  `${pair.promptId}::${pair.providerId}`;

/**
 * The batch is the cartesian product of the picked prompts and engines,
 * prompt-major so cards group by template when rendered in order. Either
 * empty side means an empty batch (the Run button disables on this).
 */
export const buildBatch = (
  promptIds: readonly string[],
  providerIds: readonly string[],
): StudioPair[] => {
  const pairs: StudioPair[] = [];
  for (const promptId of promptIds) {
    for (const providerId of providerIds) {
      pairs.push({ promptId, providerId });
    }
  }
  return pairs;
};

/** The coarse verdict a card renders: the token families the run records
 * emit (`applied`, `unchanged`, `skipped` | `skipped:<reason>`,
 * `failed:<class>`, and the bare pre-run `skipped`). `other` buckets any
 * future token so an unknown outcome still renders honestly instead of
 * pattern-matching into a wrong badge. */
export type OutcomeKind =
  | "applied"
  | "unchanged"
  | "skipped"
  | "failed"
  | "other";

export const outcomeKind = (outcome: string): OutcomeKind => {
  if (outcome === "applied") return "applied";
  if (outcome === "unchanged") return "unchanged";
  if (outcome === "skipped" || outcome.startsWith("skipped:")) return "skipped";
  if (outcome.startsWith("failed:")) return "failed";
  return "other";
};

/** A finished run kept for a pair. `outcome` rides along as the wire
 * outcome token; the card renders from `outcomeKind`. */
export interface StudioRunOutcome {
  request: StudioRunRequest;
  outcome: PromptTestOutcome;
}

export type StudioCardStatus = "running" | "done" | "cancelled";

export interface StudioCardState {
  key: string;
  promptId: string;
  providerId: string;
  status: StudioCardStatus;
  result: PromptTestOutcome | null;
}

/** Cards for a fresh batch: every pair starts in the running state. */
export const initCards = (pairs: readonly StudioPair[]): StudioCardState[] =>
  pairs.map((pair) => ({
    key: pairKey(pair),
    promptId: pair.promptId,
    providerId: pair.providerId,
    status: "running" as const,
    result: null,
  }));

/**
 * Merge a batch's results into a card list: each labeled result replaces
 * its pair's card in place, keeping the grid order stable; a re-run
 * (single-pair batch) therefore refreshes exactly its own card. Results
 * whose pair has no card append as finished cards rather than vanishing
 * (an honest shape for a defensive mismatch, not a silent drop).
 */
export const mergeResults = (
  cards: readonly StudioCardState[],
  results: readonly StudioRunOutcome[],
): StudioCardState[] => {
  const byKey = new Map(cards.map((card) => [card.key, { ...card }]));
  const merged: StudioCardState[] = [];
  for (const card of cards) {
    merged.push(byKey.get(card.key) as StudioCardState);
  }
  for (const result of results) {
    const key = pairKey({
      promptId: result.request.prompt_id,
      providerId: result.request.provider_id,
    });
    const finished: StudioCardState = {
      key,
      promptId: result.request.prompt_id,
      providerId: result.request.provider_id,
      status: "done",
      result: result.outcome,
    };
    const index = merged.findIndex((card) => card.key === key);
    if (index === -1) {
      merged.push(finished);
    } else {
      merged[index] = finished;
    }
  }
  return merged;
};

/** Flip the given pairs' cards back to running (a re-run), leaving every
 * other card untouched. Pairs with no card are ignored: a re-run only
 * refreshes what is on screen. */
export const markRunning = (
  cards: readonly StudioCardState[],
  pairs: readonly StudioPair[],
): StudioCardState[] => {
  const running = new Set(pairs.map(pairKey));
  return cards.map((card) =>
    running.has(card.key) ? { ...card, status: "running", result: null } : card,
  );
};

/**
 * KB-257: settle a finished (or cancelled) batch. Every card still in the
 * running state when the batch command resolved never got its result:
 * with `cancelled` that is because the cancel landed before its turn, so
 * it reads as cancelled; the flag is false only on the error paths that
 * keep today's behavior (the cards stay as the caller left them).
 */
export const settleUnfinished = (
  cards: readonly StudioCardState[],
  cancelled: boolean,
): StudioCardState[] => {
  if (!cancelled) return [...cards];
  return cards.map((card) =>
    card.status === "running" ? { ...card, status: "cancelled" } : card,
  );
};

/** Whether an outcome kept the raw transcript (engine failed, skipped, or
 * an unrecognized token): the card then renders the raw-kept note instead
 * of a diff. */
export const keptRaw = (outcome: string): boolean =>
  outcomeKind(outcome) !== "applied" && outcomeKind(outcome) !== "unchanged";

/**
 * Partial failure: at least one finished card kept the raw transcript
 * while at least one finished card produced output. That mixed state is
 * the studio's headline case (one dead engine must not read as "everything
 * failed", and one lucky engine must not hide its dead sibling).
 */
export const isPartialFailure = (
  cards: readonly StudioCardState[],
): boolean => {
  const done = cards.filter((card) => card.status === "done" && card.result);
  if (done.length < 2) return false;
  const anyKept = done.some((card) => keptRaw(card.result?.outcome ?? ""));
  const anyOutput = done.some((card) => !keptRaw(card.result?.outcome ?? ""));
  return anyKept && anyOutput;
};
