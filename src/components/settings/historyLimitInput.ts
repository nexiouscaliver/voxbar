// History-limit parsing for the Debug settings number field.
//
// The rule, stated once: the limit is a non-negative integer. Anything
// else (empty, partial, non-numeric) parses to null and never reaches the
// backend, because update_history_limit is destructive - it deletes
// unsaved entries and their WAV recordings down to the new limit on every
// call. The UI must only ever commit a value the user finished typing.

export type HistoryLimitParseResult =
  | { ok: true; value: number }
  | { ok: false };

/** Parse a raw history-limit input: trims, accepts a non-negative integer. */
export function parseHistoryLimit(raw: string): HistoryLimitParseResult {
  const trimmed = raw.trim();
  if (!/^\d+$/.test(trimmed)) {
    return { ok: false };
  }
  const value = Number.parseInt(trimmed, 10);
  if (!Number.isSafeInteger(value) || value < 0) {
    return { ok: false };
  }
  return { ok: true, value };
}
