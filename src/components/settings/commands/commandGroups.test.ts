import assert from "node:assert/strict";
import {
  ALL_COMMAND_IDS,
  COMMAND_GROUPS,
  MAX_PHRASE_CHARS,
  computeRowReset,
  filterCommandGroups,
  normalizePhrase,
  replaceCommandPhrases,
  validateNewPhrase,
} from "./commandGroups";
import type { CommandId, CommandMatrixEntry } from "../../../bindings";

// Grouping: the four groups hold all 30 commands, each exactly once, in
// the display order punctuation (9), symbols (16), editing (3), control (2).
assert.equal(COMMAND_GROUPS.length, 4);
assert.deepEqual(
  COMMAND_GROUPS.map((group) => group.key),
  ["punctuation", "symbols", "editing", "control"],
);
assert.deepEqual(
  COMMAND_GROUPS.map((group) => group.ids.length),
  [9, 16, 3, 2],
);
assert.equal(ALL_COMMAND_IDS.length, 30);
assert.equal(new Set(ALL_COMMAND_IDS).size, 30, "every command exactly once");
assert.equal(
  ALL_COMMAND_IDS.includes("period") && ALL_COMMAND_IDS.includes("paste"),
  true,
);

// Phrase normalization mirrors the backend: lowercase, trim, collapse
// inner whitespace.
assert.equal(normalizePhrase("  Question   MARK "), "question mark");
assert.equal(normalizePhrase("comma"), "comma");

// A small table shared by the search and reset tests.
const table: CommandMatrixEntry[] = [
  { command: "comma", phrases: ["comma"] },
  { command: "questionMark", phrases: ["question mark"] },
  { command: "exclamation", phrases: ["exclamation mark"] },
  { command: "deleteWord", phrases: ["delete word", "scratch that"] },
  { command: "clearAll", phrases: ["scratch everything"] },
];
const complete = replaceCommandPhrases("comma", ["comma"], table, null);
assert.equal(complete.length, 30);

// Search: empty query returns every group untouched.
const all = filterCommandGroups("", table, () => "name");
assert.equal(all.length, 4);
assert.deepEqual(
  all.map((group) => group.ids.length),
  [9, 16, 3, 2],
);

// Search by translated display name, case-insensitive.
const nameOf = (id: CommandId) =>
  id === "questionMark"
    ? "Question mark"
    : id === "exclamation"
      ? "Exclamation mark"
      : id;
const byName = filterCommandGroups("ques", table, nameOf);
assert.deepEqual(
  byName.flatMap((group) => group.ids),
  ["questionMark"],
);

// Search by phrase substring, case-insensitive; "mark" hits the phrases of
// questionMark and exclamation and the display names of both as well.
const byPhrase = filterCommandGroups("MARK", table, nameOf);
assert.deepEqual(
  byPhrase.flatMap((group) => group.ids),
  ["questionMark", "exclamation"],
);

// "scratch" only matches phrases, spanning the editing group's deleteWord
// and clearAll; the group keeps only the matching rows.
const byScratch = filterCommandGroups("scratch", table, nameOf);
assert.deepEqual(
  byScratch.map((group) => group.key),
  ["editing"],
);
assert.deepEqual(
  byScratch.flatMap((group) => group.ids),
  ["deleteWord", "clearAll"],
);

// No match: every group is dropped, the caller renders "no results".
assert.deepEqual(filterCommandGroups("zzz", table, nameOf), []);

// Inline validation: duplicates are detected across ALL commands after
// normalization; over-length drafts report tooLong; empty is not an error.
assert.equal(validateNewPhrase("comma", table), "duplicate");
assert.equal(validateNewPhrase("  Scratch   That ", table), "duplicate");
assert.equal(validateNewPhrase("new phrase", table), null);
assert.equal(
  validateNewPhrase(`${"x".repeat(MAX_PHRASE_CHARS + 1)}`, table),
  "tooLong",
);
assert.equal(validateNewPhrase("", table), null);
assert.equal(validateNewPhrase("   ", table), null);

// Per-row reset, clean case: only the reset command changes, the other
// commands keep their current phrases.
const defaults: CommandMatrixEntry[] = [
  { command: "comma", phrases: ["comma"] },
  { command: "period", phrases: ["period", "full stop"] },
];
const edited = replaceCommandPhrases(
  "comma",
  ["comma", "pauses"],
  defaults,
  null,
);
const clean = computeRowReset("comma", edited, defaults);
assert.equal(clean.collision, null);
assert.ok(clean.table);
assert.deepEqual(
  clean.table.find((entry) => entry.command === "comma")?.phrases,
  ["comma"],
);
assert.equal(
  clean.table.find((entry) => entry.command === "deleteWord")?.phrases.length,
  0,
);

// Per-row reset collision: the default phrase "comma" was moved to period,
// so resetting comma must NOT produce a table; it reports the phrase and
// the command now holding it instead.
const moved = replaceCommandPhrases(
  "period",
  ["period", "full stop", "comma"],
  replaceCommandPhrases("comma", [], defaults, null),
  null,
);
const blocked = computeRowReset("comma", moved, defaults);
assert.equal(blocked.table, null);
assert.deepEqual(blocked.collision, { phrase: "comma", command: "period" });

// A command absent from the defaults cannot be reset: no table, no
// collision (the caller disables the reset button instead).
const noDefaults = computeRowReset("comma", edited, []);
assert.equal(noDefaults.table, null);
assert.equal(noDefaults.collision, null);

console.log("commandGroups: all assertions passed");
