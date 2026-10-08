import assert from "node:assert/strict";
import { marginPresets, parseMarginMb } from "./memoryMarginInput";

// The 1-4 band and everything that is not a non-negative integer are
// rejected; nothing is clamped or silently zeroed.
const rejected = ["", "  ", "abc", "-5", "1", "2", "3", "4", "1.5", "5x"];
for (const raw of rejected) {
  assert.equal(
    parseMarginMb(raw).ok,
    false,
    `${JSON.stringify(raw)} must be rejected`,
  );
}

// 0 (off) and every value >= 5 parses; surrounding whitespace is trimmed.
const accepted: [string, number][] = [
  ["0", 0],
  ["5", 5],
  ["512", 512],
  ["1536", 1536],
  ["4096", 4096],
  [" 256 ", 256],
];
for (const [raw, expected] of accepted) {
  const result = parseMarginMb(raw);
  assert.equal(result.ok, true, `${JSON.stringify(raw)} must parse`);
  if (result.ok) {
    assert.equal(result.valueMb, expected);
  }
}

assert.deepEqual([...marginPresets], [0, 256, 512, 1024, 1536]);

console.log("memoryMarginInput: all assertions passed");
