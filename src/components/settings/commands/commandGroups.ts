import type { CommandId, CommandMatrixEntry } from "../../../bindings";

/**
 * Pure data and logic for the Commands tab. Grouping, search filtering and
 * the per-row reset pre-check live here so they are unit-testable without
 * React (commandGroups.test.ts runs as a plain bun script).
 *
 * The persisted surface is untouched: the same `command_phrases` setting
 * key and the same CommandMatrixEntry shape the backend validates.
 */

export type CommandGroupKey = "punctuation" | "symbols" | "editing" | "control";

/**
 * The four command groups in display order: punctuation (9), symbols (16),
 * editing (3), control (2). All 30 CommandId variants, each exactly once.
 * The totals are pinned by commandGroups.test.ts; every command the
 * backend knows (bindings.ts CommandId) must appear in exactly one group.
 */
export const COMMAND_GROUPS: ReadonlyArray<{
  key: CommandGroupKey;
  ids: readonly CommandId[];
}> = [
  {
    key: "punctuation",
    ids: [
      "period",
      "comma",
      "questionMark",
      "exclamation",
      "colon",
      "semicolon",
      "dash",
      "newLine",
      "newParagraph",
    ],
  },
  {
    key: "symbols",
    ids: [
      "atSign",
      "hash",
      "dollarSign",
      "percent",
      "star",
      "ampersand",
      "caret",
      "openParen",
      "closeParen",
      "openBracket",
      "closeBracket",
      "openBrace",
      "closeBrace",
      "slash",
      "backslash",
      "pipe",
    ],
  },
  { key: "editing", ids: ["deleteWord", "deleteLine", "clearAll"] },
  { key: "control", ids: ["undo", "paste"] },
];

/** Every command in display order (the concatenation of COMMAND_GROUPS). */
export const ALL_COMMAND_IDS: readonly CommandId[] = COMMAND_GROUPS.flatMap(
  (group) => [...group.ids],
);

/**
 * The literal each punctuation/symbol command inserts (command_action in
 * src-tauri/src/audio_toolkit/command_matrix.rs). The newline commands show
 * the escaped form so the inserted result stays readable on one line.
 * Commands missing here (deleteWord, deleteLine, clearAll, undo, paste)
 * have no insert literal; their rows describe the action instead
 * (settings.commands.actions.<id>).
 */
export const INSERT_RESULTS: Partial<Record<CommandId, string>> = {
  period: ".",
  comma: ",",
  questionMark: "?",
  exclamation: "!",
  colon: ":",
  semicolon: ";",
  dash: "-",
  newLine: "\\n",
  newParagraph: "\\n\\n",
  atSign: "@",
  hash: "#",
  dollarSign: "$",
  percent: "%",
  star: "*",
  ampersand: "&",
  caret: "^",
  openParen: "(",
  closeParen: ")",
  openBracket: "[",
  closeBracket: "]",
  openBrace: "{",
  closeBrace: "}",
  slash: "/",
  backslash: "\\",
  pipe: "|",
};

/**
 * Mirror of the backend's phrase normalization (normalize_phrase in
 * src-tauri/src/audio_toolkit/command_matrix.rs): lowercase, trim and
 * collapse inner whitespace.
 */
export const normalizePhrase = (phrase: string): string =>
  phrase.split(/\s+/).join(" ").trim().toLowerCase();

export const MAX_PHRASE_CHARS = 60;

export type PhraseValidationError = "tooLong" | "duplicate";

/**
 * Validation shown inline next to a row's input. A phrase (after
 * normalization) may not exceed MAX_PHRASE_CHARS and may not exist on ANY
 * command, matching the backend's validate_matrix, which rejects the whole
 * table on a cross-command duplicate. An empty draft is not an error: the
 * Add button stays disabled until something is typed.
 */
export const validateNewPhrase = (
  draft: string,
  entries: readonly CommandMatrixEntry[],
): PhraseValidationError | null => {
  const normalized = normalizePhrase(draft);
  if (!normalized) return null;
  if (normalized.length > MAX_PHRASE_CHARS) return "tooLong";
  for (const entry of entries) {
    if (
      entry.phrases.some((phrase) => normalizePhrase(phrase) === normalized)
    ) {
      return "duplicate";
    }
  }
  return null;
};

export interface FilteredCommandGroup {
  key: CommandGroupKey;
  ids: CommandId[];
}

/**
 * Case-insensitive search over each command's translated display name and
 * every one of its phrases. Groups left empty are dropped. An empty query
 * returns every group untouched.
 */
export const filterCommandGroups = (
  query: string,
  entries: readonly CommandMatrixEntry[],
  displayName: (id: CommandId) => string,
): FilteredCommandGroup[] => {
  const needle = query.trim().toLowerCase();
  const groups = COMMAND_GROUPS.map((group) => ({
    key: group.key,
    ids: [...group.ids],
  }));
  if (!needle) return groups;
  return groups
    .map((group) => ({
      key: group.key,
      ids: group.ids.filter((id) => {
        if (displayName(id).toLowerCase().includes(needle)) return true;
        const phrases = entries.find((entry) => entry.command === id);
        return (
          phrases?.phrases.some((phrase) =>
            phrase.toLowerCase().includes(needle),
          ) ?? false
        );
      }),
    }))
    .filter((group) => group.ids.length > 0);
};

/**
 * Builds the next table to commit with one command's phrases replaced,
 * listing every command in ALL_COMMAND_IDS order. Commands missing from
 * the current entries (a partial store) fall back to the defaults, or to
 * an empty phrase list when defaults have not loaded yet.
 */
export const replaceCommandPhrases = (
  command: CommandId,
  phrases: string[],
  entries: readonly CommandMatrixEntry[],
  defaults: readonly CommandMatrixEntry[] | null,
): CommandMatrixEntry[] =>
  ALL_COMMAND_IDS.map((id) => {
    if (id === command) return { command, phrases: [...phrases] };
    const current = entries.find((entry) => entry.command === id);
    if (current) return { command: id, phrases: [...current.phrases] };
    const fallback = defaults?.find((entry) => entry.command === id);
    return { command: id, phrases: fallback ? [...fallback.phrases] : [] };
  });

export interface RowResetCollision {
  /** The default phrase that now lives on another command. */
  phrase: string;
  /** The command currently holding the phrase. */
  command: CommandId;
}

export interface RowResetResult {
  /**
   * The next table to commit, or null when the reset is blocked. Null with
   * no collision means the defaults are unavailable (still loading or the
   * read failed): there is nothing to restore.
   */
  table: CommandMatrixEntry[] | null;
  collision: RowResetCollision | null;
}

/**
 * Per-row "Reset to default" pre-check. The backend's validate_matrix
 * (command_matrix.rs) rejects the ENTIRE update when any phrase appears on
 * two commands, so restoring one command's defaults must first confirm
 * that none of those phrases was since moved to a DIFFERENT command. On a
 * collision nothing commits; the caller surfaces the phrase and its
 * current command inline so the user can remove the moved phrase first.
 * A matrix-wide reset (command_phrases = null) cannot collide: it clears
 * to the internally consistent built-in defaults.
 */
export const computeRowReset = (
  command: CommandId,
  entries: readonly CommandMatrixEntry[],
  defaults: readonly CommandMatrixEntry[],
): RowResetResult => {
  const defaultPhrases = defaults.find(
    (entry) => entry.command === command,
  )?.phrases;
  if (!defaultPhrases) {
    return { table: null, collision: null };
  }
  for (const phrase of defaultPhrases) {
    const normalized = normalizePhrase(phrase);
    const holder = entries.find(
      (entry) =>
        entry.command !== command &&
        entry.phrases.some(
          (existing) => normalizePhrase(existing) === normalized,
        ),
    );
    if (holder) {
      return { table: null, collision: { phrase, command: holder.command } };
    }
  }
  return {
    table: replaceCommandPhrases(
      command,
      [...defaultPhrases],
      entries,
      defaults,
    ),
    collision: null,
  };
};
