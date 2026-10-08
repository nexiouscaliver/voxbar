/**
 * Memory safety margin parsing and presets for the Advanced settings row.
 *
 * The rule, stated once: a user-chosen margin is either 0 (margin off) or
 * at least 5 MB. Values 1-4 are rejected by the UI with a validation
 * message rather than clamped or silently zeroed, so a deliberate 3 is
 * never rewritten behind the user's back. (The backend additionally
 * normalizes any stale stored 1-4 down to 0 on load, as a store guard.)
 */

export type MarginParseResult = { ok: true; valueMb: number } | { ok: false };

/** The presets the Advanced UI offers, in display order. 0 is the default. */
export const marginPresets: readonly number[] = [0, 256, 512, 1024, 1536];

/** Parse a raw margin input: trims, accepts an integer, ok only for 0 or >= 5. */
export function parseMarginMb(raw: string): MarginParseResult {
  const trimmed = raw.trim();
  if (!/^\d+$/.test(trimmed)) {
    return { ok: false };
  }
  const value = Number.parseInt(trimmed, 10);
  if (!Number.isSafeInteger(value)) {
    return { ok: false };
  }
  if (value !== 0 && value < 5) {
    return { ok: false };
  }
  return { ok: true, valueMb: value };
}
