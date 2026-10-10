/**
 * Pure decision for what a settings write should do after its backend
 * command resolves, extracted so it is testable without store
 * infrastructure. Tauri command results arrive as Result values
 * ({status: "ok", data} or {status: "error", error}); a thrown JS exception
 * and an error status must end in the same place: roll the optimistic value
 * back and tell the user, or the UI keeps showing a setting the backend
 * never accepted (e.g. update checks under HANDY_DISABLE_UPDATER).
 */

export interface CommandResultLike {
  status: string;
  error?: string;
}

export interface SettingsWriteOutcome {
  ok: boolean;
  rollback: boolean;
  error?: string;
}

export const mapCommandResult = (
  result: unknown,
  key: string,
): SettingsWriteOutcome => {
  if (
    result !== null &&
    typeof result === "object" &&
    (result as CommandResultLike).status === "error"
  ) {
    const error = (result as CommandResultLike).error;
    return {
      ok: false,
      rollback: true,
      error: error && error.length > 0 ? error : `Failed to update ${key}`,
    };
  }
  // Ok results, raw non-Result return values, and absent results are all
  // successes: only an explicit error status is a failure.
  return { ok: true, rollback: false, error: undefined };
};
