/**
 * A small pure text diff (v1.6.0 W1-L6): line-level add/remove/context with
 * word-level emphasis inside paired changed lines, the shape a GitHub-style
 * red/green renderer needs. No external dependency and no DOM, so a plain
 * bun script can unit test it (diff.test.ts), matching the noticeRouting /
 * skipToastDedupe pure-module convention.
 *
 * CJK safety: the intra-line tokenizer never assumes space-delimited words.
 * Space-separated scripts tokenize per word; CJK ideographs, kana, hangul
 * and CJK punctuation tokenize per character, so a one-character edit in a
 * Japanese or Chinese sentence is emphasized exactly as tightly as a
 * one-word edit in English.
 */

export type DiffLineKind = "context" | "added" | "removed";

/** One inline run of a changed line: `changed` marks the words (or CJK
 * characters) that actually differ inside the line; everything else on the
 * line is carried context. Context lines carry a single unchanged span. */
export interface DiffSpan {
  text: string;
  changed: boolean;
}

export interface DiffLine {
  kind: DiffLineKind;
  /** 0-based index into the OLD text's lines; null on added lines. */
  oldIndex: number | null;
  /** 0-based index into the NEW text's lines; null on removed lines. */
  newIndex: number | null;
  spans: DiffSpan[];
}

/** Coarse stats for summary chips ("+1 line -1 line, 2 chars changed"):
 * line counts plus the total character volume inside changed spans, the
 * number that makes a 1-char polish in a 2353-char wall of text visible. */
export interface DiffSummary {
  addedLines: number;
  removedLines: number;
  addedChars: number;
  removedChars: number;
}

export interface DiffResult {
  lines: DiffLine[];
  summary: DiffSummary;
}

/** Split on newlines with GitHub's trailing-newline convention: a text
 * ending in "\n" has that newline end its last line, not open an empty
 * one, so "abc\n" is one line. The empty string is zero lines. */
const splitLines = (text: string): string[] => {
  if (text === "") return [];
  const lines = text.split("\n");
  if (lines.length > 1 && lines[lines.length - 1] === "") lines.pop();
  return lines;
};

type Op = { type: "equal" | "delete" | "insert"; a: number; b: number };

/**
 * Classic LCS diff over two string arrays. Returns the op walk (equal /
 * delete from a / insert from b) in order. Prefix and suffix trimming keeps
 * the quadratic table small for the typical post-process edit (one changed
 * paragraph inside shared surroundings).
 *
 * Returns null instead of computing when the trimmed problem still exceeds
 * MAX_CELLS: the caller then falls back to whole-block replacement rather
 * than burning memory on a pathological input.
 */
const lcsOps = (a: readonly string[], b: readonly string[]): Op[] | null => {
  const aLen = a.length;
  const bLen = b.length;

  // Trim the shared prefix and suffix; they are equal ops by definition.
  const prefix: Op[] = [];
  let start = 0;
  while (start < aLen && start < bLen && a[start] === b[start]) {
    prefix.push({ type: "equal", a: start, b: start });
    start += 1;
  }
  let endA = aLen;
  let endB = bLen;
  const suffix: Op[] = [];
  while (endA > start && endB > start && a[endA - 1] === b[endB - 1]) {
    endA -= 1;
    endB -= 1;
    suffix.unshift({ type: "equal", a: endA, b: endB });
  }

  const middle: Op[] = [];
  const coreA = endA - start;
  const coreB = endB - start;
  if (coreA === 0 || coreB === 0) {
    // One side empty after trimming (identical texts are all prefix and
    // suffix): the core is pure deletes or pure inserts.
    for (let i = start; i < endA; i++)
      middle.push({ type: "delete", a: i, b: -1 });
    for (let j = start; j < endB; j++)
      middle.push({ type: "insert", a: -1, b: j });
    return [...prefix, ...middle, ...suffix];
  }

  const MAX_CELLS = 4_000_000;
  if (coreA * coreB > MAX_CELLS) return null;

  // dp[i * width + j] = LCS length of aCore[i..] vs bCore[j..]; the extra
  // final row and column are the all-zero sentinels.
  const width = coreB + 1;
  const dp = new Int32Array((coreA + 1) * width);
  for (let i = coreA - 1; i >= 0; i -= 1) {
    for (let j = coreB - 1; j >= 0; j -= 1) {
      dp[i * width + j] =
        a[start + i] === b[start + j]
          ? dp[(i + 1) * width + (j + 1)] + 1
          : Math.max(dp[(i + 1) * width + j], dp[i * width + (j + 1)]);
    }
  }

  // Walk the table: prefer the diagonal on a match (always optimal for
  // LCS length), then the side that keeps the longer subsequence.
  let i = 0;
  let j = 0;
  while (i < coreA && j < coreB) {
    if (a[start + i] === b[start + j]) {
      middle.push({ type: "equal", a: start + i, b: start + j });
      i += 1;
      j += 1;
    } else if (dp[(i + 1) * width + j] >= dp[i * width + (j + 1)]) {
      middle.push({ type: "delete", a: start + i, b: -1 });
      i += 1;
    } else {
      middle.push({ type: "insert", a: -1, b: start + j });
      j += 1;
    }
  }
  while (i < coreA) {
    middle.push({ type: "delete", a: start + i, b: -1 });
    i += 1;
  }
  while (j < coreB) {
    middle.push({ type: "insert", a: -1, b: start + j });
    j += 1;
  }
  return [...prefix, ...middle, ...suffix];
};

/** CJK blocks that tokenize per character: CJK symbols and punctuation,
 * kana, ext-A and unified ideographs, compat ideographs, hangul syllables.
 * Anything else that is not whitespace joins its neighboring run (a Latin
 * word, a number, a punctuation cluster). */
const CJK_RANGES =
  "\u3000-\u30ff\u3400-\u4dbf\u4e00-\u9fff\uf900-\ufaff\uac00-\ud7af";
const TOKEN_RE = new RegExp(
  `(\\s+|[^\\s${CJK_RANGES}]+|[${CJK_RANGES}])`,
  "gu",
);

/** Tokenize one line for inline emphasis. Whitespace runs are their own
 * tokens so spacing-only edits still emphasize. */
const tokenize = (line: string): string[] => line.match(TOKEN_RE) ?? [];

/** Merge adjacent spans carrying the same changed flag. */
const mergeSpans = (spans: DiffSpan[]): DiffSpan[] => {
  const merged: DiffSpan[] = [];
  for (const span of spans) {
    const last = merged[merged.length - 1];
    if (last && last.changed === span.changed) {
      last.text += span.text;
    } else {
      merged.push({ ...span });
    }
  }
  return merged;
};

/** Inline word-level emphasis for one removed/added line pair: each side
 * gets spans where tokens outside the token LCS are `changed`. Returns the
 * pair of span arrays. Falls back to fully-changed spans when the token
 * problem is too large to table. */
const inlineSpans = (
  oldLine: string,
  newLine: string,
): { removed: DiffSpan[]; added: DiffSpan[] } => {
  const oldTokens = tokenize(oldLine);
  const newTokens = tokenize(newLine);
  const ops = lcsOps(oldTokens, newTokens);
  if (ops === null) {
    return {
      removed: [{ text: oldLine, changed: true }],
      added: [{ text: newLine, changed: true }],
    };
  }
  const removed: DiffSpan[] = [];
  const added: DiffSpan[] = [];
  for (const op of ops) {
    if (op.type === "equal") {
      removed.push({ text: oldTokens[op.a], changed: false });
      added.push({ text: newTokens[op.b], changed: false });
    } else if (op.type === "delete") {
      removed.push({ text: oldTokens[op.a], changed: true });
    } else {
      added.push({ text: newTokens[op.b], changed: true });
    }
  }
  return { removed: mergeSpans(removed), added: mergeSpans(added) };
};

/** Whole-line emphasis for an unpaired removed/added line. */
const wholeLineSpans = (text: string): DiffSpan[] => [
  { text, changed: text.length > 0 },
];

/**
 * Diff two texts line by line with word-level emphasis inside paired
 * changed lines. Pure: same inputs, same output, no globals, no DOM.
 *
 * Pairing rule: inside each change block (a run of removed lines followed
 * by a run of added lines) the k-th removed line pairs with the k-th added
 * line; leftover lines on either side render fully changed with no pairing.
 */
export const diffText = (oldText: string, newText: string): DiffResult => {
  const oldLines = splitLines(oldText);
  const newLines = splitLines(newText);
  const ops = lcsOps(oldLines, newLines);

  const lines: DiffLine[] = [];
  let oldIndex = 0;
  let newIndex = 0;

  const pushBlock = (removedIdx: number[], addedIdx: number[]) => {
    const paired = Math.min(removedIdx.length, addedIdx.length);
    for (let k = 0; k < paired; k += 1) {
      const { removed, added } = inlineSpans(
        oldLines[removedIdx[k]],
        newLines[addedIdx[k]],
      );
      lines.push({
        kind: "removed",
        oldIndex: removedIdx[k],
        newIndex: null,
        spans: removed,
      });
      lines.push({
        kind: "added",
        oldIndex: null,
        newIndex: addedIdx[k],
        spans: added,
      });
    }
    for (let k = paired; k < removedIdx.length; k += 1) {
      lines.push({
        kind: "removed",
        oldIndex: removedIdx[k],
        newIndex: null,
        spans: wholeLineSpans(oldLines[removedIdx[k]]),
      });
    }
    for (let k = paired; k < addedIdx.length; k += 1) {
      lines.push({
        kind: "added",
        oldIndex: null,
        newIndex: addedIdx[k],
        spans: wholeLineSpans(newLines[addedIdx[k]]),
      });
    }
  };

  if (ops === null) {
    // Pathological size guard: describe the whole text as one replacement
    // block without pairing, so rendering stays possible and bounded.
    pushBlock(
      oldLines.map((_, i) => i),
      newLines.map((_, i) => i),
    );
  } else {
    let pendingRemoved: number[] = [];
    let pendingAdded: number[] = [];
    const flush = () => {
      if (pendingRemoved.length > 0 || pendingAdded.length > 0) {
        pushBlock(pendingRemoved, pendingAdded);
        pendingRemoved = [];
        pendingAdded = [];
      }
    };
    for (const op of ops) {
      if (op.type === "equal") {
        flush();
        lines.push({
          kind: "context",
          oldIndex,
          newIndex,
          spans: [{ text: oldLines[op.a], changed: false }],
        });
        oldIndex += 1;
        newIndex += 1;
      } else if (op.type === "delete") {
        pendingRemoved.push(op.a);
        oldIndex += 1;
      } else {
        pendingAdded.push(op.b);
        newIndex += 1;
      }
    }
    flush();
  }

  const summary: DiffSummary = {
    addedLines: 0,
    removedLines: 0,
    addedChars: 0,
    removedChars: 0,
  };
  for (const line of lines) {
    if (line.kind === "added") summary.addedLines += 1;
    if (line.kind === "removed") summary.removedLines += 1;
    for (const span of line.spans) {
      if (!span.changed) continue;
      if (line.kind === "added") summary.addedChars += span.text.length;
      if (line.kind === "removed") summary.removedChars += span.text.length;
    }
  }

  return { lines, summary };
};
