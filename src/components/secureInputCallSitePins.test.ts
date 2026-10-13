import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";

/* KB-276 call-site pin (fleet pass 1): the locale half of KB-276 is pinned
 * by pluralCoverage.test.ts (every category exists in every locale, no
 * English leak, numerals accurate), but that suite renders through
 * i18next.getFixedT over the locale files - it never touches the
 * component. The actual user-visible half is SecureInputWarning's `message`
 * expression passing count and letting i18next resolve the plural suffix;
 * nothing executed it, so a regression back to the hand-rolled
 * `blockedWithCulprit_${suffix}` key kept the entire suite green while
 * re-introducing the wrong grammar in ru/uk/pl/cs/he/ar.
 *
 * No React render harness exists in this repo, so this pin EXECUTES the
 * committed expression verbatim - the same technique as
 * directoryOpenErrorPins.test.ts - with a recording t() and all four
 * status/affectedCount combinations. */

const here = import.meta.dirname;

interface TCall {
  key: string;
  opts: Record<string, unknown> | undefined;
}

function makeT(): {
  calls: TCall[];
  t: (key: string, opts?: Record<string, unknown>) => string;
} {
  const calls: TCall[] = [];
  return {
    calls,
    t: (key: string, opts?: Record<string, unknown>) => {
      calls.push({ key, opts });
      return key;
    },
  };
}

// Slice the committed `const message = ...;` statement out of the
// component and turn it into a callable (the statement references only
// `status`, `affectedCount`, and `t`).
function makeMessageFn(): (
  status: { culprit_name: string | null },
  affectedCount: number,
  t: (key: string, opts?: Record<string, unknown>) => string,
) => string {
  const source = fs.readFileSync(
    path.join(here, "SecureInputWarning.tsx"),
    "utf8",
  );
  const start = source.indexOf("const message =");
  assert.ok(start >= 0, "the message expression must exist");
  const end = source.indexOf(";", start);
  assert.ok(end > start, "the message expression must close cleanly");
  const statement = source
    .slice(start, end + 1)
    .replace(/\s+as\s+[A-Za-z]+/g, "");
  assert.ok(!statement.includes(" as "), "cast stripping must be total");
  return new Function(
    "status",
    "affectedCount",
    "t",
    `"use strict";\n${statement}\nreturn message;`,
  ) as (
    status: { culprit_name: string | null },
    affectedCount: number,
    t: (key: string, opts?: Record<string, unknown>) => string,
  ) => string;
}

const messageOf = makeMessageFn();

// 1. Bindings affected + a culprit: the bare family key with count and
//    name - i18next derives the _one/_few/_many suffix from count. A
//    hand-rolled suffixed key (`blockedWithCulprit_one`) fails the exact
//    key equality here.
{
  const { calls, t } = makeT();
  const rendered = messageOf({ culprit_name: "Terminal" }, 21, t);
  assert.equal(rendered, "secureInput.blockedWithCulprit");
  assert.equal(calls.length, 1, "exactly one t() call");
  assert.equal(
    calls[0].key,
    "secureInput.blockedWithCulprit",
    "the key must be the bare plural family - no hand-rolled suffix",
  );
  assert.equal(
    calls[0].opts?.name,
    "Terminal",
    "the culprit name rides the call",
  );
  assert.equal(
    calls[0].opts?.count,
    21,
    "count rides the call so i18next resolves the plural category (ru 'one' fires at 21)",
  );
  assert.equal(typeof calls[0].opts?.count, "number", "count must be a number");
}

// 2. Bindings affected, no culprit: the NoCulprit family with count.
{
  const { calls, t } = makeT();
  const rendered = messageOf({ culprit_name: null }, 101, t);
  assert.equal(rendered, "secureInput.blockedNoCulprit");
  assert.equal(calls.length, 1, "exactly one t() call");
  assert.equal(calls[0].key, "secureInput.blockedNoCulprit");
  assert.equal(calls[0].opts?.count, 101, "count rides the call");
}

// 3. Recorder blocked + culprit: the non-count family with the name only.
{
  const { calls, t } = makeT();
  const rendered = messageOf({ culprit_name: "Terminal" }, 0, t);
  assert.equal(rendered, "secureInput.recorderBlockedWithCulprit");
  assert.equal(calls.length, 1, "exactly one t() call");
  assert.equal(calls[0].key, "secureInput.recorderBlockedWithCulprit");
  assert.equal(calls[0].opts?.name, "Terminal");
  assert.equal(
    calls[0].opts?.count,
    undefined,
    "the recorder-blocked family is not a count family",
  );
}

// 4. Recorder blocked, no culprit: the bare key with no options.
{
  const { calls, t } = makeT();
  const rendered = messageOf({ culprit_name: null }, 0, t);
  assert.equal(rendered, "secureInput.recorderBlockedNoCulprit");
  assert.equal(calls.length, 1, "exactly one t() call");
  assert.equal(calls[0].key, "secureInput.recorderBlockedNoCulprit");
  assert.equal(calls[0].opts, undefined, "no interpolation options");
}

// 5. The count really flows from the component's own computation: the
//    committed source must derive affectedCount from the two binding
//    lists (a stubbed-in constant would make facets 1-2 vacuous).
{
  const source = fs.readFileSync(
    path.join(here, "SecureInputWarning.tsx"),
    "utf8",
  );
  const countAt = source.indexOf("const affectedCount =");
  assert.ok(countAt >= 0, "the component must compute affectedCount");
  const countEnd = source.indexOf(";", countAt);
  const computation = source.slice(countAt, countEnd);
  assert.ok(
    computation.includes("uncovered_bindings") &&
      computation.includes("degraded_bindings"),
    "affectedCount must derive from the status binding lists",
  );
  // And the call site passes that variable, not a literal.
  const messageAt = source.indexOf("const message =");
  const messageSpan = source.slice(messageAt, source.indexOf(";", messageAt));
  assert.ok(
    messageSpan.includes("count: affectedCount"),
    "the message expression must pass the computed affectedCount as count",
  );
}

console.log(
  "secureInputCallSitePins (KB-276): the SecureInputWarning call site passes count and lets i18next resolve the suffix",
);
