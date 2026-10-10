import assert from "node:assert/strict";
import {
  searchKeyboardReducer,
  type SearchKeyboardState,
} from "./searchKeyboardNav";

const state = (
  selectedIndex: number,
  resultCount: number,
): SearchKeyboardState => ({
  selectedIndex,
  resultCount,
});

// ArrowDown moves the highlight down one row and stops at the last row.
assert.deepEqual(searchKeyboardReducer(state(0, 5), "ArrowDown"), {
  selectedIndex: 1,
  action: "none",
});
assert.deepEqual(searchKeyboardReducer(state(3, 5), "ArrowDown"), {
  selectedIndex: 4,
  action: "none",
});
assert.deepEqual(searchKeyboardReducer(state(4, 5), "ArrowDown"), {
  selectedIndex: 4,
  action: "none",
});

// ArrowUp moves the highlight up one row and stops at the first row.
assert.deepEqual(searchKeyboardReducer(state(4, 5), "ArrowUp"), {
  selectedIndex: 3,
  action: "none",
});
assert.deepEqual(searchKeyboardReducer(state(1, 5), "ArrowUp"), {
  selectedIndex: 0,
  action: "none",
});
assert.deepEqual(searchKeyboardReducer(state(0, 5), "ArrowUp"), {
  selectedIndex: 0,
  action: "none",
});

// Enter commits the highlighted row, not always the first one.
assert.deepEqual(searchKeyboardReducer(state(0, 5), "Enter"), {
  selectedIndex: 0,
  action: "choose",
});
assert.deepEqual(searchKeyboardReducer(state(3, 5), "Enter"), {
  selectedIndex: 3,
  action: "choose",
});

// Escape closes the dropdown whatever is selected.
for (const selectedIndex of [0, 2, 4]) {
  assert.deepEqual(searchKeyboardReducer(state(selectedIndex, 5), "Escape"), {
    selectedIndex,
    action: "close",
  });
}

// With no results, navigation resets the selection and Enter commits
// nothing; Escape still closes.
for (const key of ["ArrowDown", "ArrowUp", "Enter"] as const) {
  assert.deepEqual(searchKeyboardReducer(state(2, 0), key), {
    selectedIndex: 0,
    action: "reset",
  });
}
assert.deepEqual(searchKeyboardReducer(state(0, 0), "Escape"), {
  selectedIndex: 0,
  action: "close",
});

// A stale index beyond the result count is clamped back into range.
assert.deepEqual(searchKeyboardReducer(state(9, 3), "ArrowDown"), {
  selectedIndex: 2,
  action: "none",
});
assert.deepEqual(searchKeyboardReducer(state(9, 3), "Enter"), {
  selectedIndex: 2,
  action: "choose",
});

console.log("searchKeyboardNav: all assertions passed");
