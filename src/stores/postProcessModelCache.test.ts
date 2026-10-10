// Workstream 2: pin the model-list cache semantics (cache hit = no
// refetch; a failed fetch never clobbers a shown list). The HTTP layer
// never appears here: these helpers run on the store state shapes only.
import assert from "node:assert/strict";
import {
  hasSessionList,
  hydrateModelOptions,
  shouldFetchOnOpen,
  withModelFetchError,
} from "./postProcessModelCache";
import type { CachedModelList, PostProcessModelError } from "@/bindings";

function cached(models: string[], fetchedAt = 1_760_000_000): CachedModelList {
  return { models, fetched_at_unix: fetchedAt };
}

const authError: PostProcessModelError = {
  kind: "auth",
  detail: "Model list request failed (401 Unauthorized): bad key",
};

// --- hydration ------------------------------------------------------

// Reopening the panel is instant: the persisted cache fills the dropdown
// for providers the session has no list for.
{
  const cache = { openai: cached(["gpt-4o-mini", "gpt-4o"]) };
  const hydrated = hydrateModelOptions(cache, {});
  assert.deepEqual(hydrated.openai, ["gpt-4o-mini", "gpt-4o"]);
}

// A list fetched this session wins over an older cached one.
{
  const cache = { openai: cached(["stale-from-cache"]) };
  const session = { openai: ["fresh-this-session"] };
  const hydrated = hydrateModelOptions(cache, session);
  assert.deepEqual(hydrated.openai, ["fresh-this-session"]);
}

// A cleared (empty) session entry does not shadow the cache: the store
// clears entries by writing [], and the cache legitimately refills them
// (the base-URL change clears the backend cache itself).
{
  const cache = { custom: cached(["llama3.1:8b"]) };
  const session = { custom: [] as string[] };
  const hydrated = hydrateModelOptions(cache, session);
  assert.deepEqual(hydrated.custom, ["llama3.1:8b"]);
}

// Empty cache entries are skipped, other providers pass through untouched.
{
  const cache = { openai: cached([]), groq: cached(["llama-3.3-70b"]) };
  const session = { openai: ["session-list"] };
  const hydrated = hydrateModelOptions(cache, session);
  assert.deepEqual(hydrated.openai, ["session-list"]);
  assert.deepEqual(hydrated.groq, ["llama-3.3-70b"]);
  assert.equal(hydrated.anthropic, undefined);
}

// A missing cache (pre-cache settings) keeps the session state as-is.
{
  const session = { openai: ["session-list"] };
  assert.equal(hydrateModelOptions(undefined, session), session);
}

// --- cache hit / no refetch ------------------------------------------

// A cache hit means no refetch on open.
assert.equal(
  shouldFetchOnOpen({ openai: cached(["gpt-4o-mini"]) }, {}, "openai"),
  false,
  "a cached list must not trigger an on-open refetch",
);

// A session list (this tab already fetched) is equally a hit.
assert.equal(
  shouldFetchOnOpen({}, { openai: ["gpt-4o-mini"] }, "openai"),
  false,
);

// No session list and no cache: fetch on open.
assert.equal(shouldFetchOnOpen({}, {}, "openai"), true);

// An empty cache entry or a cleared session entry is not a hit.
assert.equal(shouldFetchOnOpen({ openai: cached([]) }, {}, "openai"), true);
assert.equal(
  shouldFetchOnOpen(
    { openai: cached(["gpt-4o-mini"]) },
    { openai: [] },
    "openai",
  ),
  false,
  "the cache still serves when the session entry was cleared",
);

// One provider's hit does not leak into another provider's decision.
assert.equal(
  shouldFetchOnOpen({ openai: cached(["gpt-4o-mini"]) }, {}, "groq"),
  true,
);

// --- failure handling -------------------------------------------------

// A failed fetch records its classified error and leaves the shown list
// untouched: the error, never a blank dropdown, is what the user sees.
{
  const errors = withModelFetchError({}, "openai", authError);
  assert.equal(errors.openai?.kind, "auth");
  assert.ok(errors.openai?.detail.includes("401"));

  // A later success clears the error.
  const cleared = withModelFetchError(errors, "openai", null);
  assert.equal(cleared.openai, null);

  // Other providers' errors are preserved.
  const two = withModelFetchError(
    withModelFetchError({}, "groq", { kind: "network", detail: "down" }),
    "openai",
    authError,
  );
  assert.equal(two.groq?.kind, "network");
  assert.equal(two.openai?.kind, "auth");
}

// The session-list predicate agrees with the shape the store writes.
assert.equal(hasSessionList({ openai: ["m"] }, "openai"), true);
assert.equal(hasSessionList({ openai: [] }, "openai"), false);
assert.equal(hasSessionList({}, "openai"), false);

console.log("postProcessModelCache.test.ts: all assertions passed");
