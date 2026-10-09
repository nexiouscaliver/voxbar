import React, { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation, Trans } from "react-i18next";
import { toast } from "sonner";
import { RotateCcw, Search, X } from "lucide-react";
import { commands } from "@/bindings";
import type { CommandId, CommandMatrixEntry } from "@/bindings";
import { useSettings } from "../../../hooks/useSettings";
import { SettingsGroup } from "../../ui/SettingsGroup";
import { SettingContainer } from "../../ui/SettingContainer";
import { Input } from "../../ui/Input";
import { Button } from "../../ui/Button";
import { Dialog } from "../../ui/Dialog";
import { AutoInterpretCommands } from "../AutoInterpretCommands";
import { NumberFormatSetting } from "../NumberFormat";
import { SpokenPunctuation } from "../SpokenPunctuation";
import { TerminalPunctuation } from "../TerminalPunctuation";
import { VoiceDeletionCommands } from "../VoiceDeletionCommands";
import {
  INSERT_RESULTS,
  computeRowReset,
  filterCommandGroups,
  normalizePhrase,
  replaceCommandPhrases,
  validateNewPhrase,
  type PhraseValidationError,
  type RowResetCollision,
} from "./commandGroups";

interface CommandRowProps {
  command: CommandId;
  entries: CommandMatrixEntry[];
  defaultCount: number | null;
  collision: RowResetCollision | null;
  busy: boolean;
  onAdd: (command: CommandId, phrase: string) => void;
  onRemove: (command: CommandId, phrase: string) => void;
  onResetRow: (command: CommandId) => void;
}

/**
 * One command row: the translated name and the inserted result on top,
 * the phrase chips, the add-phrase input with inline validation, and the
 * per-row reset below. The chip body is inert; removal is its explicit X
 * button, guarded by an undo toast in the parent, so one mis-click can
 * never silently lose a phrase.
 */
const CommandRow: React.FC<CommandRowProps> = ({
  command,
  entries,
  defaultCount,
  collision,
  busy,
  onAdd,
  onRemove,
  onResetRow,
}) => {
  const { t } = useTranslation();
  const [draft, setDraft] = useState("");
  const [error, setError] = useState<PhraseValidationError | null>(null);

  const phrases =
    entries.find((entry) => entry.command === command)?.phrases ?? [];

  const tryAdd = () => {
    const normalized = normalizePhrase(draft);
    if (!normalized || busy) return;
    const failure = validateNewPhrase(draft, entries);
    if (failure) {
      setError(failure);
      return;
    }
    onAdd(command, normalized);
    setDraft("");
    setError(null);
  };

  const insertResult = INSERT_RESULTS[command];
  const description =
    insertResult !== undefined ? (
      <Trans
        i18nKey="settings.commands.inserts"
        values={{ result: insertResult }}
        components={{ code: <code className="font-mono" /> }}
      />
    ) : (
      t(`settings.commands.actions.${command}`)
    );

  return (
    <SettingContainer
      title={t(`settings.commands.names.${command}`)}
      description={description}
      descriptionMode="inline"
      layout="stacked"
      grouped
    >
      <div className="flex flex-col gap-2">
        <div className="flex flex-wrap items-center gap-1.5">
          {phrases.map((phrase) => (
            <span
              key={phrase}
              className="inline-flex max-w-64 items-center gap-1 rounded-md border border-mid-gray/20 bg-mid-gray/10 px-2 py-0.5 text-xs"
            >
              <span className="truncate" title={phrase}>
                {phrase}
              </span>
              <button
                type="button"
                onClick={() => onRemove(command, phrase)}
                disabled={busy}
                aria-label={t("settings.commands.removePhrase", { phrase })}
                className="shrink-0 cursor-pointer rounded-sm text-mid-gray transition-colors hover:text-error disabled:cursor-not-allowed disabled:opacity-50"
              >
                <X className="h-3 w-3" aria-hidden="true" />
              </button>
            </span>
          ))}
          <Input
            type="text"
            variant="compact"
            className="w-40 max-w-full"
            value={draft}
            onChange={(event) => {
              setDraft(event.target.value);
              setError(null);
            }}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                event.preventDefault();
                tryAdd();
              }
            }}
            placeholder={t("settings.commands.placeholder")}
            aria-label={t(`settings.commands.names.${command}`)}
            disabled={busy}
          />
          <Button
            variant="secondary"
            size="sm"
            onClick={tryAdd}
            disabled={busy || !normalizePhrase(draft)}
          >
            {t("settings.commands.addPhrase")}
          </Button>
        </div>
        {error === "tooLong" && (
          <p className="text-xs text-error">
            {t("settings.commands.tooLongError")}
          </p>
        )}
        {error === "duplicate" && (
          <p className="text-xs text-error">
            {t("settings.commands.duplicateError", {
              phrase: normalizePhrase(draft),
            })}
          </p>
        )}
        {collision && (
          <p className="text-xs text-error">
            {t("settings.commands.resetCollision", {
              phrase: collision.phrase,
              command: t(`settings.commands.names.${collision.command}`),
            })}
          </p>
        )}
        <div>
          <Button
            variant="ghost"
            size="sm"
            onClick={() => onResetRow(command)}
            disabled={busy || defaultCount === null}
          >
            <RotateCcw className="me-1 h-3 w-3" aria-hidden="true" />
            {t("settings.commands.resetRow")}
            {defaultCount !== null ? ` (${defaultCount})` : ""}
          </Button>
        </div>
      </div>
    </SettingContainer>
  );
};

export const CommandsSettings: React.FC = () => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, isUpdating } = useSettings();
  // The default phrases come from the backend (one read command) rather
  // than being duplicated in TypeScript.
  const [defaults, setDefaults] = useState<CommandMatrixEntry[] | null>(null);
  const [query, setQuery] = useState("");
  const [resetAllOpen, setResetAllOpen] = useState(false);
  const [collisions, setCollisions] = useState<
    Partial<Record<CommandId, RowResetCollision>>
  >({});

  useEffect(() => {
    let cancelled = false;
    commands
      .getDefaultCommandMatrix()
      .then((entries) => {
        if (!cancelled) setDefaults(entries);
      })
      .catch(() => {
        // Defaults unavailable (command failed): the editor still shows
        // and edits the stored phrases; only the reset actions wait.
      });
    return () => {
      cancelled = true;
    };
  }, []);

  // null (or not yet loaded) means the built-in defaults.
  const stored = getSetting("command_phrases");
  const entries = useMemo(() => stored ?? defaults ?? [], [stored, defaults]);

  // Latest table ref so a toast's Undo action never commits a stale table
  // when the user has edited other rows in the meantime.
  const entriesRef = useRef(entries);
  entriesRef.current = entries;

  const busy = isUpdating("command_phrases");
  const commit = (next: CommandMatrixEntry[]) =>
    updateSetting("command_phrases", next);

  const visibleGroups = useMemo(
    () =>
      filterCommandGroups(query, entries, (id) =>
        t(`settings.commands.names.${id}`),
      ),
    [query, entries, t],
  );

  const addPhrase = (command: CommandId, phrase: string) => {
    const current =
      entries.find((entry) => entry.command === command)?.phrases ?? [];
    commit(replaceCommandPhrases(command, [...current, phrase], entries, null));
    setCollisions((current2) => {
      const { [command]: _removed, ...rest } = current2;
      return rest;
    });
  };

  const removePhrase = (command: CommandId, phrase: string) => {
    commit(
      replaceCommandPhrases(
        command,
        (
          entries.find((entry) => entry.command === command)?.phrases ?? []
        ).filter((existing) => existing !== phrase),
        entries,
        null,
      ),
    );
    setCollisions((current) => {
      const { [command]: _removed, ...rest } = current;
      return rest;
    });
    toast(t("settings.commands.removedPhrase", { phrase }), {
      action: {
        label: t("settings.commands.undoRemove"),
        onClick: () => {
          const latest = entriesRef.current;
          const phrases =
            latest.find((entry) => entry.command === command)?.phrases ?? [];
          if (phrases.includes(phrase)) return;
          commit(
            replaceCommandPhrases(command, [...phrases, phrase], latest, null),
          );
        },
      },
    });
  };

  const resetRow = (command: CommandId) => {
    if (!defaults || busy) return;
    // computeRowReset is the guard: when one of the command's default
    // phrases now lives on another command, the backend's validate_matrix
    // would reject the entire update, so the collision is surfaced inline
    // naming the phrase and its current command, and nothing commits.
    const result = computeRowReset(command, entries, defaults);
    if (result.collision) {
      setCollisions((current) => ({ ...current, [command]: result.collision }));
      return;
    }
    setCollisions((current) => {
      const { [command]: _removed, ...rest } = current;
      return rest;
    });
    if (result.table) commit(result.table);
  };

  const confirmResetAll = () => {
    // None clears the persisted table: the next session compiles the
    // built-in defaults again.
    updateSetting("command_phrases", null);
    setCollisions({});
    setResetAllOpen(false);
  };

  const filtering = query.trim().length > 0;

  return (
    <div className="max-w-3xl w-full mx-auto space-y-6">
      <div className="flex flex-wrap items-center gap-2">
        <div className="flex min-w-40 flex-1 items-center gap-2">
          <Search
            className="h-4 w-4 shrink-0 text-mid-gray"
            aria-hidden="true"
          />
          <Input
            type="text"
            variant="compact"
            className="w-full"
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            placeholder={t("settings.commands.searchPlaceholder")}
            aria-label={t("settings.commands.searchPlaceholder")}
          />
        </div>
        <Button
          variant="secondary"
          size="sm"
          onClick={() => setResetAllOpen(true)}
          disabled={busy}
        >
          {t("settings.commands.resetAll")}
        </Button>
      </div>

      {!filtering && (
        <SettingsGroup title={t("settings.commands.behavior")}>
          <AutoInterpretCommands descriptionMode="tooltip" grouped={true} />
          <SpokenPunctuation descriptionMode="tooltip" grouped={true} />
          <NumberFormatSetting descriptionMode="tooltip" grouped={true} />
          <TerminalPunctuation descriptionMode="tooltip" grouped={true} />
          <VoiceDeletionCommands descriptionMode="tooltip" grouped={true} />
        </SettingsGroup>
      )}

      {visibleGroups.map((group) => (
        <SettingsGroup
          key={group.key}
          title={t(`settings.commands.groups.${group.key}`)}
        >
          {group.ids.map((command) => (
            <CommandRow
              key={command}
              command={command}
              entries={entries}
              defaultCount={
                defaults?.find((entry) => entry.command === command)?.phrases
                  .length ?? null
              }
              collision={collisions[command] ?? null}
              busy={busy}
              onAdd={addPhrase}
              onRemove={removePhrase}
              onResetRow={resetRow}
            />
          ))}
        </SettingsGroup>
      ))}

      {filtering && visibleGroups.length === 0 && (
        <p className="px-4 text-sm text-mid-gray">
          {t("settings.commands.noResults")}
        </p>
      )}

      <Dialog
        open={resetAllOpen}
        title={t("settings.commands.resetAllConfirmTitle")}
        closeLabel={t("common.close")}
        onOpenChange={setResetAllOpen}
        footer={
          <>
            <Button
              variant="secondary"
              size="sm"
              onClick={() => setResetAllOpen(false)}
            >
              {t("settings.commands.resetAllConfirmCancel")}
            </Button>
            <Button variant="primary" size="sm" onClick={confirmResetAll}>
              {t("settings.commands.resetAllConfirmConfirm")}
            </Button>
          </>
        }
      >
        <p className="text-sm">{t("settings.commands.resetAllConfirmBody")}</p>
      </Dialog>
    </div>
  );
};
