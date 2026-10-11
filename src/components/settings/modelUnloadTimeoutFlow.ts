import type { ModelUnloadTimeout } from "@/bindings";

/**
 * Pure decisions for the "Unload After → Custom…" flow, extracted so they
 * are testable without component or store infrastructure (same convention
 * as settingsWriteOutcome.ts). Tauri command results arrive as Result
 * values ({status: "ok", data} or {status: "error", error}); a backend
 * rejection resolves instead of throwing, so awaiting the command is not
 * enough — the resolved value must be inspected before the optimistic
 * store update, or the UI keeps showing a custom timeout the backend
 * rejected (model_unload_timeout has no settingUpdaters entry, so
 * updateSetting's own mapCommandResult(null) short-circuit cannot catch
 * it).
 */

interface CommandResultLike {
  status: string;
  error?: string;
}

const isCustom = (
  value: ModelUnloadTimeout | undefined,
): value is { custom: { seconds: number } } =>
  typeof value === "object" && value !== null && "custom" in value;

export type UnloadTimeoutOutcome =
  | { apply: true; value: { custom: { seconds: number } } }
  | { apply: false; keep: ModelUnloadTimeout | undefined; error: string };

/**
 * Decide what the flow does after commands.setModelUnloadTimeoutCustomSeconds
 * resolves. Only an explicit {status: "error"} is a failure (ok results,
 * raw non-Result return values, and absent results are all successes —
 * mirroring mapCommandResult's tolerance); on failure the previous stored
 * value is kept and a surfaceable reason is carried, with the same
 * "Failed to update <key>" fallback when the result carries none.
 */
export const resolveUnloadTimeoutOutcome = (
  result: unknown,
  attemptedSeconds: number,
  prev: ModelUnloadTimeout | undefined,
): UnloadTimeoutOutcome => {
  if (
    result !== null &&
    typeof result === "object" &&
    (result as CommandResultLike).status === "error"
  ) {
    const error = (result as CommandResultLike).error;
    return {
      apply: false,
      keep: prev,
      error:
        error && error.length > 0
          ? error
          : "Failed to update model_unload_timeout",
    };
  }
  return { apply: true, value: { custom: { seconds: attemptedSeconds } } };
};

export type UnloadTimeoutFocusGate =
  | { action: "defer" }
  | { action: "switch" }
  | { action: "focus" };

/**
 * Decide what the tray focus handler ("Unload After → Custom…") does for
 * the stored model_unload_timeout. The three states it must distinguish:
 * an unhydrated store (getSetting returns undefined because settings is
 * null — defer, never optimistic-flip), a hydrated store with a preset
 * (switch to custom so the field renders, KB-185), and a hydrated store
 * already in custom mode (focus only — no redundant backend write).
 */
export const resolveUnloadTimeoutFocusGate = (
  storedValue: ModelUnloadTimeout | undefined,
  hydrated: boolean,
): UnloadTimeoutFocusGate => {
  if (!hydrated) {
    return { action: "defer" };
  }
  if (isCustom(storedValue)) {
    return { action: "focus" };
  }
  return { action: "switch" };
};
