import type { ModelStateEvent } from "../../lib/types/events";

/**
 * Pure helpers behind the onboarding memory-gate refusal card, kept out of
 * the component so the recovery logic is testable in the repo's bun style.
 * The backend attaches structured numbers to loading_failed events (never a
 * parseable string), and this module turns them into the card's data.
 */

export interface MemoryGateRefusal {
  forecastBytes: number;
  freeBytes: number;
  headroomBytes: number;
}

export type RefusalAction =
  | "retry"
  | "switch-model"
  | "disable-guard"
  | "continue-deferred";

/** Extract the structured refusal numbers from a model-state event; null
 * for anything that is not a gate refusal (other loading failures carry no
 * memory_gate payload). */
export function parseRefusal(event: ModelStateEvent): MemoryGateRefusal | null {
  if (event.event_type !== "loading_failed") {
    return null;
  }
  const gate = event.memory_gate;
  if (!gate) {
    return null;
  }
  const { forecast_bytes, free_bytes, headroom_bytes } = gate;
  if (
    typeof forecast_bytes !== "number" ||
    typeof free_bytes !== "number" ||
    typeof headroom_bytes !== "number"
  ) {
    return null;
  }
  return {
    forecastBytes: forecast_bytes,
    freeBytes: free_bytes,
    headroomBytes: headroom_bytes,
  };
}

/** Which recovery actions the refusal card offers: retry and switch-model
 * always apply once a refusal exists, disable-guard only while the guard is
 * on, continue-deferred always (the model stays selected and loads on the
 * first dictation). */
export function refusalActions(
  refusal: MemoryGateRefusal,
  canDisableGuard: boolean,
): RefusalAction[] {
  void refusal; // the refusal's existence enables the first two actions
  const actions: RefusalAction[] = ["retry", "switch-model"];
  if (canDisableGuard) {
    actions.push("disable-guard");
  }
  actions.push("continue-deferred");
  return actions;
}

const MIB = 1024 * 1024;
const GIB = 1024 * 1024 * 1024;

/** Forecast/free amounts for the card, mirroring the backend's message
 * rule exactly: integer MB below 1 GiB (minimum 1), one-decimal GB at or
 * above it. */
export function formatMemoryAmount(bytes: number): string {
  if (bytes < GIB) {
    return `${Math.max(1, Math.floor(bytes / MIB))} MB`;
  }
  return `${(bytes / GIB).toFixed(1)} GB`;
}

/** The margin always renders in integer MB (minimum 1) so the card echoes
 * the unit of the Advanced settings UI the user set the margin in. */
export function formatMarginMb(headroomBytes: number): string {
  return `${Math.max(1, Math.floor(headroomBytes / MIB))} MB`;
}
