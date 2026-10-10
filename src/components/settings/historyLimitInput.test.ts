import assert from "node:assert/strict";
import { parseHistoryLimit } from "./historyLimitInput";

// The history limit is destructive on the backend (every committed value
// immediately deletes unsaved entries and their WAV files down to that
// count), so the parse is strict: only a finished non-negative integer may
// ever be committed. Partial keystrokes ("1" while typing "15") parse to
// null at the UI layer and are held in the draft, never sent.
assert.deepEqual(parseHistoryLimit("15"), { ok: true, value: 15 });
assert.deepEqual(parseHistoryLimit("0"), { ok: true, value: 0 });
assert.deepEqual(parseHistoryLimit("  42  "), { ok: true, value: 42 });
assert.deepEqual(parseHistoryLimit("1000"), { ok: true, value: 1000 });

// Keystroke-in-progress and invalid states never produce a value.
assert.deepEqual(parseHistoryLimit(""), { ok: false });
assert.deepEqual(parseHistoryLimit("1."), { ok: false });
assert.deepEqual(parseHistoryLimit("-3"), { ok: false });
assert.deepEqual(parseHistoryLimit("abc"), { ok: false });
assert.deepEqual(parseHistoryLimit("12px"), { ok: false });
assert.deepEqual(parseHistoryLimit("1e3"), { ok: false });

console.log("historyLimitInput: all assertions passed");
