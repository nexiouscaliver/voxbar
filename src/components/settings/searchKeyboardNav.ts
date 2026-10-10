/**
 * Pure keyboard-selection logic for the settings search dropdown, extracted
 * so it is testable without component infrastructure (this repo's tests are
 * plain node:assert scripts).
 *
 * Actions:
 *  - "none": no commit; the highlight may still have moved.
 *  - "choose": commit the row at the returned selectedIndex.
 *  - "close": dismiss the dropdown and clear the query.
 *  - "reset": nothing is selectable (empty result list); clear the highlight.
 */

export interface SearchKeyboardState {
  selectedIndex: number;
  resultCount: number;
}

export type SearchKey = "ArrowUp" | "ArrowDown" | "Enter" | "Escape";

export type SearchKeyboardAction = "none" | "choose" | "close" | "reset";

export interface SearchKeyboardResult {
  selectedIndex: number;
  action: SearchKeyboardAction;
}

const clamp = (index: number, resultCount: number): number =>
  resultCount <= 0 ? 0 : Math.min(Math.max(index, 0), resultCount - 1);

export const searchKeyboardReducer = (
  state: SearchKeyboardState,
  key: SearchKey,
): SearchKeyboardResult => {
  const { resultCount } = state;

  if (key === "Escape") {
    return {
      selectedIndex: clamp(state.selectedIndex, resultCount),
      action: "close",
    };
  }

  if (resultCount <= 0) {
    return { selectedIndex: 0, action: "reset" };
  }

  const selectedIndex = clamp(state.selectedIndex, resultCount);

  switch (key) {
    case "ArrowDown":
      return {
        selectedIndex: clamp(selectedIndex + 1, resultCount),
        action: "none",
      };
    case "ArrowUp":
      return {
        selectedIndex: clamp(selectedIndex - 1, resultCount),
        action: "none",
      };
    case "Enter":
      return { selectedIndex, action: "choose" };
  }
};
