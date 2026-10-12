import assert from "node:assert/strict";
import {
  buildBatch,
  initCards,
  isPartialFailure,
  markRunning,
  mergeResults,
  outcomeKind,
  pairKey,
  keptRaw,
  type StudioCardState,
  type StudioRunOutcome,
} from "./studioRunState";
import type { PromptTestOutcome, StudioRunRequest } from "@/bindings";

// v1.6.0 W1-L6: the Prompt Studio's pure state semantics. The component
// stays thin; everything testable about loading, merging, and partial
// failures lives here.

const outcome = (over: Partial<PromptTestOutcome>): PromptTestOutcome => ({
  before: "source text",
  after: "polished text",
  outcome: "applied",
  latency_ms: 1200,
  ...over,
});

const runOutcome = (
  promptId: string,
  providerId: string,
  token: string,
): StudioRunOutcome => ({
  request: { prompt_id: promptId, provider_id: providerId } as StudioRunRequest,
  outcome: outcome({ outcome: token }),
});

// 1. Batch building: the cartesian product, prompt-major, and empty on
// either empty side.
assert.deepEqual(buildBatch(["p1", "p2"], ["e1", "e2"]), [
  { promptId: "p1", providerId: "e1" },
  { promptId: "p1", providerId: "e2" },
  { promptId: "p2", providerId: "e1" },
  { promptId: "p2", providerId: "e2" },
]);
assert.deepEqual(buildBatch([], ["e1"]), []);
assert.deepEqual(buildBatch(["p1"], []), []);

// 2. pairKey: the "prompt::provider" join. Real ids cannot contain "::"
// (templates are "prompt_<timestamp>" or fixed seed slugs, providers are
// fixed registry ids), so the join is unambiguous for every id the app
// can produce.
assert.equal(
  pairKey({ promptId: "prompt_1712", providerId: "custom" }),
  "prompt_1712::custom",
);
assert.notEqual(
  pairKey({ promptId: "prompt_1", providerId: "custom" }),
  pairKey({ promptId: "prompt_2", providerId: "custom" }),
);
assert.notEqual(
  pairKey({ promptId: "prompt_1", providerId: "openai" }),
  pairKey({ promptId: "prompt_1", providerId: "custom" }),
);

// 3. initCards: every pair starts running with no result.
const batch = buildBatch(["p1", "p2"], ["local", "custom"]);
const cards = initCards(batch);
assert.equal(cards.length, 4);
assert.ok(
  cards.every((card) => card.status === "running" && card.result === null),
);

// 4. Loading state: markRunning flips only the re-run pair back to
// running and clears its stale result, leaving siblings untouched.
const partiallyDone: StudioCardState[] = [
  {
    key: "p1::local",
    promptId: "p1",
    providerId: "local",
    status: "done",
    result: outcome({ outcome: "applied" }),
  },
  {
    key: "p1::custom",
    promptId: "p1",
    providerId: "custom",
    status: "done",
    result: outcome({ outcome: "failed:network" }),
  },
];
const reRunning = markRunning(partiallyDone, [
  { promptId: "p1", providerId: "custom" },
]);
assert.equal(
  reRunning[0].status,
  "done",
  "the untouched card keeps its result",
);
assert.ok(reRunning[0].result, "the untouched card keeps its result payload");
assert.equal(reRunning[1].status, "running");
assert.equal(reRunning[1].result, null, "a re-run clears its stale result");

// 5. mergeResults: each labeled result replaces its own card in place, so
// the grid order is stable across runs and re-runs.
const merged = mergeResults(initCards(batch), [
  runOutcome("p1", "custom", "applied"),
  runOutcome("p1", "local", "failed:network"),
  runOutcome("p2", "custom", "skipped:memory_gate"),
  runOutcome("p2", "local", "unchanged"),
]);
assert.deepEqual(
  merged.map((card) => card.key),
  ["p1::local", "p1::custom", "p2::local", "p2::custom"],
  "card order follows the batch order, not the results order",
);
assert.equal(merged[1].result?.outcome, "applied");
assert.equal(merged[0].result?.outcome, "failed:network");

// A single-pair re-run replaces exactly its card and no other.
const rerun = mergeResults(merged, [runOutcome("p1", "local", "applied")]);
assert.equal(rerun[0].result?.outcome, "applied", "the re-run pair updates");
assert.equal(
  rerun[1].result?.outcome,
  "applied",
  "its sibling from the earlier batch survives",
);
assert.equal(rerun.length, merged.length);

// 6. Partial failure: one engine dead while another produced output is a
// partial failure; all-dead and all-output are not.
assert.ok(
  isPartialFailure(merged),
  "two applied plus a failed and a skipped is partial",
);
assert.ok(
  isPartialFailure([
    {
      key: "a::x",
      promptId: "a",
      providerId: "x",
      status: "done",
      result: outcome({ outcome: "applied" }),
    },
    {
      key: "b::x",
      promptId: "b",
      providerId: "x",
      status: "done",
      result: outcome({ outcome: "failed:auth" }),
    },
  ]),
  "one applied plus one failed is the minimal partial case",
);
// All engines kept raw: not partial, just failed.
assert.ok(
  !isPartialFailure([
    {
      key: "a::x",
      promptId: "a",
      providerId: "x",
      status: "done",
      result: outcome({ outcome: "failed:network" }),
    },
    {
      key: "b::x",
      promptId: "b",
      providerId: "x",
      status: "done",
      result: outcome({ outcome: "skipped:memory_gate" }),
    },
  ]),
);
// All engines produced output: not partial.
assert.ok(
  !isPartialFailure([
    {
      key: "a::x",
      promptId: "a",
      providerId: "x",
      status: "done",
      result: outcome({ outcome: "applied" }),
    },
    {
      key: "b::x",
      promptId: "b",
      providerId: "x",
      status: "done",
      result: outcome({ outcome: "unchanged" }),
    },
  ]),
);
// A single card can never be partial.
assert.ok(
  !isPartialFailure([
    {
      key: "a::x",
      promptId: "a",
      providerId: "x",
      status: "done",
      result: outcome({ outcome: "failed:auth" }),
    },
  ]),
);
// Running cards do not count toward either side.
assert.ok(
  !isPartialFailure([
    {
      key: "a::x",
      promptId: "a",
      providerId: "x",
      status: "running",
      result: null,
    },
    {
      key: "b::x",
      promptId: "b",
      providerId: "x",
      status: "done",
      result: outcome({ outcome: "applied" }),
    },
  ]),
);

// 7. Outcome classification: every token family the run records emit,
// plus the fallback for an unknown future token.
assert.equal(outcomeKind("applied"), "applied");
assert.equal(outcomeKind("unchanged"), "unchanged");
assert.equal(outcomeKind("skipped"), "skipped");
assert.equal(outcomeKind("skipped:memory_gate"), "skipped");
assert.equal(outcomeKind("skipped:engine_failed"), "skipped");
assert.equal(outcomeKind("failed:network"), "failed");
assert.equal(outcomeKind("failed:output_invalid"), "failed");
assert.equal(outcomeKind("something_new"), "other");
assert.ok(keptRaw("failed:timeout"));
assert.ok(keptRaw("skipped:memory_gate"));
assert.ok(!keptRaw("applied"));
assert.ok(!keptRaw("unchanged"));

console.log("studioRunState.test.ts: all assertions passed");
