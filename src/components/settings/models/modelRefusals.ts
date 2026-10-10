// Pure error-classification helpers for the voice Models settings tab.
// Extracted so the component and the colocated test share one source of
// truth, mirroring the post-processing tab's localLlmRouting.ts matchers.

// Stable prefix of the live-session delete-refusal error string
// (SESSION_REFUSAL_PREFIX in src-tauri/src/commands/models.rs, KB-117/KB-220).
// Must stay in sync: the row matches it to word the "VoxBar is dictating with
// this model" refusal as a localized retry hint instead of the raw refusal
// text, which reads like a delete failure.
export const SESSION_REFUSAL_PREFIX = "dictation-in-progress";

// Whether a delete-command error is the transient live-session refusal
// (KB-117/KB-220: a dictation session is still using this model), rather
// than a real failure (disk error, missing file) that must show its actual
// cause.
export function isSessionRefusalError(error: string): boolean {
  return error.startsWith(SESSION_REFUSAL_PREFIX);
}
