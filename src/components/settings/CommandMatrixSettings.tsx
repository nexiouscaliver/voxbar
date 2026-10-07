import React, { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { commands } from "@/bindings";
import type { CommandId, CommandMatrixEntry } from "@/bindings";
import { useSettings } from "../../hooks/useSettings";
import { Input } from "../ui/Input";
import { Button } from "../ui/Button";
import { SettingContainer } from "../ui/SettingContainer";

interface CommandMatrixSettingsProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

/** The fixed command groups, in display order. Command display names are
 * English data (derived from the CommandId), not i18n keys. */
const SYMBOL_GROUP: CommandId[] = [
  "period",
  "comma",
  "questionMark",
  "exclamation",
  "colon",
  "semicolon",
  "dash",
  "newLine",
  "newParagraph",
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
];
const EDITING_GROUP: CommandId[] = ["deleteWord", "deleteLine", "clearAll"];
const CONTROL_GROUP: CommandId[] = ["undo", "paste"];

const GROUPS: Array<{ key: string; ids: CommandId[] }> = [
  { key: "symbols", ids: SYMBOL_GROUP },
  { key: "editing", ids: EDITING_GROUP },
  { key: "control", ids: CONTROL_GROUP },
];

/** "questionMark" -> "Question Mark" (English display name). */
const humanizeCommand = (id: CommandId) =>
  (id.charAt(0).toUpperCase() + id.slice(1).replace(/([A-Z])/g, " $1")).trim();

/** Mirror of the backend's phrase normalization: lowercase, trim, collapse
 * inner whitespace. */
const normalizePhrase = (phrase: string) =>
  phrase
    .split(/\s+/)
    .join(" ")
    .trim()
    .toLowerCase();

const MAX_PHRASE_CHARS = 60;

export const CommandMatrixSettings: React.FC<CommandMatrixSettingsProps> =
  React.memo(({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();
    // The default phrases come from the backend (one read command) rather
    // than being duplicated in TypeScript.
    const [defaults, setDefaults] = useState<CommandMatrixEntry[] | null>(null);
    // Per-command add-phrase input drafts.
    const [drafts, setDrafts] = useState<Record<string, string>>({});

    useEffect(() => {
      let cancelled = false;
      commands
        .getDefaultCommandMatrix()
        .then((entries) => {
          if (!cancelled) setDefaults(entries);
        })
        .catch(() => {
          // Defaults unavailable (command failed): the editor still shows
          // and edits the stored phrases.
        });
      return () => {
        cancelled = true;
      };
    }, []);

    // null (or not yet loaded) means the built-in defaults.
    const stored = getSetting("command_phrases");
    const entries = useMemo(
      () => stored ?? defaults ?? [],
      [stored, defaults],
    );
    const phrasesByCommand = useMemo(() => {
      const map = new Map<CommandId, string[]>();
      for (const entry of entries) map.set(entry.command, entry.phrases);
      return map;
    }, [entries]);
    const everyPhrase = useMemo(() => {
      const all = new Set<string>();
      for (const entry of entries) {
        for (const phrase of entry.phrases) all.add(phrase);
      }
      return all;
    }, [entries]);

    const busy = isUpdating("command_phrases");
    const commit = (next: CommandMatrixEntry[]) =>
      updateSetting("command_phrases", next);

    const addPhrase = (command: CommandId) => {
      const normalized = normalizePhrase(drafts[command] ?? "");
      if (!normalized || busy) return;
      if (normalized.length > MAX_PHRASE_CHARS) {
        toast.error(t("settings.advanced.commandMatrix.tooLongError"));
        return;
      }
      // Duplicates are rejected across ALL commands, matching the backend
      // validation (a phrase belongs to exactly one command).
      if (everyPhrase.has(normalized)) {
        toast.error(
          t("settings.advanced.commandMatrix.duplicateError", {
            phrase: normalized,
          }),
        );
        return;
      }
      commit([
        ...entries.filter((entry) => entry.command !== command),
        {
          command,
          phrases: [...(phrasesByCommand.get(command) ?? []), normalized],
        },
      ]);
      setDrafts((current) => ({ ...current, [command]: "" }));
    };

    const removePhrase = (command: CommandId, phrase: string) => {
      commit(
        entries.map((entry) =>
          entry.command === command
            ? { ...entry, phrases: entry.phrases.filter((p) => p !== phrase) }
            : entry,
        ),
      );
    };

    const resetToDefaults = () => {
      // None clears the persisted table: the next session compiles the
      // built-in defaults again.
      updateSetting("command_phrases", null);
      setDrafts({});
    };

    return (
      <SettingContainer
        title={t("settings.advanced.commandMatrix.title")}
        description={t("settings.advanced.commandMatrix.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      >
        <div className="space-y-4">
          {GROUPS.map((group) => (
            <div key={group.key} className="space-y-2">
              <div className="text-xs font-semibold uppercase tracking-wide text-mid-gray">
                {t(`settings.advanced.commandMatrix.groups.${group.key}`)}
              </div>
              {group.ids.map((command) => {
                const phrases = phrasesByCommand.get(command) ?? [];
                return (
                  <div key={command} className="flex flex-wrap items-center gap-1">
                    <span className="w-36 shrink-0 text-sm">
                      {humanizeCommand(command)}
                    </span>
                    {phrases.map((phrase) => (
                      <Button
                        key={phrase}
                        onClick={() => removePhrase(command, phrase)}
                        disabled={busy}
                        variant="secondary"
                        size="sm"
                        className="inline-flex items-center gap-1 cursor-pointer"
                        aria-label={t(
                          "settings.advanced.commandMatrix.removePhrase",
                          { phrase },
                        )}
                      >
                        <span>{phrase}</span>
                        <svg
                          className="w-3 h-3"
                          fill="none"
                          stroke="currentColor"
                          viewBox="0 0 24 24"
                        >
                          <path
                            strokeLinecap="round"
                            strokeLinejoin="round"
                            strokeWidth={2}
                            d="M6 18L18 6M6 6l12 12"
                          />
                        </svg>
                      </Button>
                    ))}
                    <Input
                      type="text"
                      className="max-w-40"
                      value={drafts[command] ?? ""}
                      onChange={(e) =>
                        setDrafts((current) => ({
                          ...current,
                          [command]: e.target.value,
                        }))
                      }
                      onKeyDown={(e) => {
                        if (e.key === "Enter") {
                          e.preventDefault();
                          addPhrase(command);
                        }
                      }}
                      placeholder={t(
                        "settings.advanced.commandMatrix.placeholder",
                      )}
                      variant="compact"
                      disabled={busy}
                    />
                    <Button
                      onClick={() => addPhrase(command)}
                      disabled={
                        !normalizePhrase(drafts[command] ?? "") ||
                        normalizePhrase(drafts[command] ?? "").length >
                          MAX_PHRASE_CHARS ||
                        busy
                      }
                      variant="secondary"
                      size="sm"
                    >
                      {t("settings.advanced.commandMatrix.addPhrase")}
                    </Button>
                  </div>
                );
              })}
            </div>
          ))}
          <Button onClick={resetToDefaults} disabled={busy} variant="secondary" size="sm">
            {t("settings.advanced.commandMatrix.reset")}
          </Button>
        </div>
      </SettingContainer>
    );
  });
