import assert from "node:assert/strict";
import { diffText, type DiffLine } from "./diff";

// v1.6.0 W1-L6: the pure diff util behind History's "View changes" expander
// and the Prompt Studio result cards. Line-level add/remove/context with
// word-level emphasis inside paired changed lines, no space-segmentation
// assumptions (CJK tokenizes per character).

const simplified = (lines: DiffLine[]) =>
  lines.map((line) => ({
    kind: line.kind,
    oldIndex: line.oldIndex,
    newIndex: line.newIndex,
    spans: line.spans.map((span) => [span.text, span.changed ? 1 : 0]),
  }));

// 1. Identical text: every line is context, nothing is emphasized, and the
// summary is all zeros.
{
  const text = "hello world\nsecond line\nthird line";
  const result = diffText(text, text);
  assert.deepEqual(
    simplified(result.lines),
    [
      {
        kind: "context",
        oldIndex: 0,
        newIndex: 0,
        spans: [["hello world", 0]],
      },
      {
        kind: "context",
        oldIndex: 1,
        newIndex: 1,
        spans: [["second line", 0]],
      },
      { kind: "context", oldIndex: 2, newIndex: 2, spans: [["third line", 0]] },
    ],
    "identical text renders as pure context",
  );
  assert.deepEqual(result.summary, {
    addedLines: 0,
    removedLines: 0,
    addedChars: 0,
    removedChars: 0,
  });
}

// 2. English word deletion mid-line: one removed/added pair, the deleted
// word emphasized on the removed side, the added side all unchanged.
{
  const result = diffText(
    "hey um world this is a test",
    "hey world this is a test",
  );
  assert.deepEqual(
    simplified(result.lines),
    [
      {
        kind: "removed",
        oldIndex: 0,
        newIndex: null,
        spans: [
          ["hey ", 0],
          ["um ", 1],
          ["world this is a test", 0],
        ],
      },
      {
        kind: "added",
        oldIndex: null,
        newIndex: 0,
        spans: [["hey world this is a test", 0]],
      },
    ],
    "a deleted word is a changed span on the removed line only",
  );
  assert.equal(result.summary.removedChars, "um ".length);
  assert.equal(result.summary.addedChars, 0);
}

// 3. Word substitution (not just deletion): both sides carry a changed
// span around the shared words.
{
  const result = diffText("the quick brown fox", "the slow brown fox");
  assert.deepEqual(
    simplified(result.lines),
    [
      {
        kind: "removed",
        oldIndex: 0,
        newIndex: null,
        spans: [
          ["the ", 0],
          ["quick", 1],
          [" brown fox", 0],
        ],
      },
      {
        kind: "added",
        oldIndex: null,
        newIndex: 0,
        spans: [
          ["the ", 0],
          ["slow", 1],
          [" brown fox", 0],
        ],
      },
    ],
    "a substitution emphasizes exactly the swapped word on both sides",
  );
}

// 4. Pure line addition and pure line removal with surrounding context.
{
  const added = diffText("alpha\nbeta", "alpha\nbeta\ngamma");
  assert.deepEqual(
    added.lines.map((line) => line.kind),
    ["context", "context", "added"],
  );
  assert.deepEqual(
    added.lines[2].spans,
    [{ text: "gamma", changed: true }],
    "an unpaired added line is fully changed",
  );
  assert.equal(added.summary.addedLines, 1);
  assert.equal(added.summary.addedChars, "gamma".length);

  const removed = diffText("alpha\nbeta\ngamma", "alpha\nbeta");
  assert.deepEqual(
    removed.lines.map((line) => line.kind),
    ["context", "context", "removed"],
  );
  assert.equal(removed.summary.removedLines, 1);
}

// 5. CJK with no spaces: tokenization is per character, so a substitution
// inside a Japanese sentence emphasizes exactly the differing characters.
{
  const result = diffText(
    "こんにちは世界、よろしく",
    "こんにちは皆さん、よろしく",
  );
  assert.deepEqual(
    simplified(result.lines),
    [
      {
        kind: "removed",
        oldIndex: 0,
        newIndex: null,
        spans: [
          ["こんにちは", 0],
          ["世界", 1],
          ["、よろしく", 0],
        ],
      },
      {
        kind: "added",
        oldIndex: null,
        newIndex: 0,
        spans: [
          ["こんにちは", 0],
          ["皆さん", 1],
          ["、よろしく", 0],
        ],
      },
    ],
    "CJK emphasis isolates the changed characters, not the whole line",
  );
  assert.equal(result.summary.removedChars, 2);
  assert.equal(result.summary.addedChars, 3);
}

// 6. A one-character CJK edit: the shared prefix is context, only the
// differing tail characters are emphasized.
{
  const result = diffText("解決します", "解決しました");
  assert.deepEqual(simplified(result.lines), [
    {
      kind: "removed",
      oldIndex: 0,
      newIndex: null,
      spans: [
        ["解決しま", 0],
        ["す", 1],
      ],
    },
    {
      kind: "added",
      oldIndex: null,
      newIndex: 0,
      spans: [
        ["解決しま", 0],
        ["した", 1],
      ],
    },
  ]);
  assert.equal(result.summary.removedChars, 1);
  assert.equal(result.summary.addedChars, 2);
}

// 7. Mixed CJK and Latin in one line: each script tokenizes its own way
// (CJK per character, "3pm" as one word token).
{
  const result = diffText("会議は 3pm に始まります", "会議は 4pm に始まります");
  assert.deepEqual(simplified(result.lines), [
    {
      kind: "removed",
      oldIndex: 0,
      newIndex: null,
      spans: [
        ["会議は ", 0],
        ["3pm", 1],
        [" に始まります", 0],
      ],
    },
    {
      kind: "added",
      oldIndex: null,
      newIndex: 0,
      spans: [
        ["会議は ", 0],
        ["4pm", 1],
        [" に始まります", 0],
      ],
    },
  ]);
}

// 8. The audit's byte-level case: a bare paragraph newline rewritten into
// ". " plus a capitalized word. Two old lines join into one new line, so
// the first lines pair (the join tail is emphasized; "well" glues to the
// period as one word token), and the leftover old line renders fully
// changed, exactly like a GitHub line-join hunk.
{
  const old = "resolve them as well\nunattended workflow run";
  const now = "resolve them as well. Unattended workflow run";
  const result = diffText(old, now);
  assert.deepEqual(simplified(result.lines), [
    {
      kind: "removed",
      oldIndex: 0,
      newIndex: null,
      spans: [
        ["resolve them as ", 0],
        ["well", 1],
      ],
    },
    {
      kind: "added",
      oldIndex: null,
      newIndex: 0,
      spans: [
        ["resolve them as ", 0],
        ["well. Unattended workflow run", 1],
      ],
    },
    {
      kind: "removed",
      oldIndex: 1,
      newIndex: null,
      spans: [["unattended workflow run", 1]],
    },
  ]);
  assert.equal(result.summary.removedLines, 2);
  assert.equal(result.summary.addedLines, 1);
}

// 9. Trailing newline convention: a text ending in "\n" treats that
// newline as ending its last line, so a trailing-newline-only difference
// renders as identical (no phantom empty-line churn).
{
  const result = diffText("abc\n", "abc");
  assert.deepEqual(
    result.lines.map((line) => line.kind),
    ["context"],
  );
  assert.deepEqual(result.summary, {
    addedLines: 0,
    removedLines: 0,
    addedChars: 0,
    removedChars: 0,
  });
}

// 10. Empty inputs: empty old text is all additions, empty new text is all
// removals, and both-empty is no lines at all.
{
  const fromEmpty = diffText("", "fresh\ntext");
  assert.deepEqual(
    fromEmpty.lines.map((line) => line.kind),
    ["added", "added"],
  );
  const toEmpty = diffText("gone\nsoon", "");
  assert.deepEqual(
    toEmpty.lines.map((line) => line.kind),
    ["removed", "removed"],
  );
  assert.deepEqual(diffText("", "").lines, []);
}

// 11. Many-to-one blocks: an old line edited plus a neighbor dropped pair
// by index (first with first), and the leftover old line renders fully
// changed.
{
  const result = diffText("a\nb\nc", "a2\nc");
  assert.deepEqual(simplified(result.lines), [
    { kind: "removed", oldIndex: 0, newIndex: null, spans: [["a", 1]] },
    { kind: "added", oldIndex: null, newIndex: 0, spans: [["a2", 1]] },
    { kind: "removed", oldIndex: 1, newIndex: null, spans: [["b", 1]] },
    { kind: "context", oldIndex: 2, newIndex: 1, spans: [["c", 0]] },
  ]);
}

// 12. Prefix and suffix trimming keeps shared surroundings as context even
// when the middle churns.
{
  const oldText = "intro line\nmiddle one\nmiddle two\noutro line";
  const newText = "intro line\nmiddle alpha\noutro line";
  const result = diffText(oldText, newText);
  assert.deepEqual(
    result.lines.map((line) => line.kind),
    ["context", "removed", "added", "removed", "context"],
  );
  assert.equal(result.lines[0].oldIndex, 0);
  assert.equal(result.lines[0].newIndex, 0);
  assert.equal(result.lines[4].oldIndex, 3);
  assert.equal(result.lines[4].newIndex, 2);
}

console.log("diff.test.ts: all assertions passed");
