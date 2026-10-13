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
  /** Phrases of the pristine backend default table (the seeded triggers). */
  defaultPhrases: ReadonlySet<string>;
  defaultCount: number | null;
  /** KB-022: true while the default phrases have not resolved yet; add
   * and remove wait so a pre-resolution edit cannot persist zero-phrase
   * rows for every command it does not name. */
  defaultsPending: boolean;
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
  defaultPhrases,
  defaultCount,
  defaultsPending,
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

  // A seeded Latin-script trigger is the recognizer's English vocabulary
  // (command_matrix.rs pins Latin seeds to English on purpose), which used
  // to leave bare English chips inside an otherwise translated panel. The
  // chip now leads with the command's translated name and keeps the literal
  // phrase as a dimmed gloss: the panel reads localized while the chip
  // still shows the exact spoken string. User-added phrases and the
  // non-Latin built-in aliases (already in the recognizer's output
  // language) render bare, and a locale whose name matches the literal
  // (English itself) skips the gloss.
  const localizedSeedLabel = (phrase: string): string | null => {
    if (!defaultPhrases.has(phrase)) return null;
    // eslint-disable-next-line no-control-regex
    if (!/^[\x20-\x7E]+$/.test(phrase)) return null;
    const localized: string = t(`settings.commands.names.${command}`);
    return localized.toLowerCase() === phrase.toLowerCase() ? null : localized;
  };

  // KB-022: edits disabled while defaults are unresolved (see the prop).
  const editsLocked = busy || defaultsPending;

  const tryAdd = () => {
    const normalized = normalizePhrase(draft);
    if (!normalized || editsLocked) return;
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
          {phrases.map((phrase) => {
            const label = localizedSeedLabel(phrase);
            return (
              <span
                key={phrase}
                className="inline-flex max-w-64 items-center gap-1 rounded-md border border-mid-gray/20 bg-mid-gray/10 px-2 py-0.5 text-xs"
              >
                {label ? (
                  <>
                    <span className="truncate">{label}</span>
                    <span className="truncate text-mid-gray" title={phrase}>
                      ({phrase})
                    </span>
                  </>
                ) : (
                  <span className="truncate" title={phrase}>
                    {phrase}
                  </span>
                )}
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
            );
          })}
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
            disabled={editsLocked}
          />
          <Button
            variant="secondary"
            size="sm"
            onClick={tryAdd}
            disabled={editsLocked || !normalizePhrase(draft)}
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

  // Every phrase of the pristine default table: the set that decides which
  // chips render with the translated-name + literal-gloss treatment.
  const defaultPhrases = useMemo(
    () => new Set(defaults?.flatMap((entry) => entry.phrases) ?? []),
    [defaults],
  );

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

  // KB-022: add/remove must not commit before the default phrases have
  // resolved. With the stored table absent and the defaults still null,
  // every command the edit does not name would persist as a zero-phrase
  // row (the ~29 empty commands bug); the rows wait for the defaults
  // instead. Reset was already gated the same way.
  const defaultsPending = defaults === null;

  const addPhrase = (command: CommandId, phrase: string) => {
    const current =
      entries.find((entry) => entry.command === command)?.phrases ?? [];
    commit(
      replaceCommandPhrases(command, [...current, phrase], entries, defaults),
    );
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
        defaults,
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
            replaceCommandPhrases(
              command,
              [...phrases, phrase],
              latest,
              defaults,
            ),
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
      {/* The pane's own filter reads as a scoped list filter, not a second
          copy of the boxed "Search settings" field pinned above it: compact,
          right-aligned, underline-only, glyph inside the field. The
          destructive reset-all sits opposite it, separated. */}
      <div className="flex flex-wrap items-center justify-between gap-2">
        <Button
          variant="danger-ghost"
          size="sm"
          onClick={() => setResetAllOpen(true)}
          disabled={busy}
        >
          {t("settings.commands.resetAll")}
        </Button>
        <div className="relative w-56 max-w-full">
          <Search
            className="pointer-events-none absolute start-2 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-text/40"
            aria-hidden="true"
          />
          <input
            type="text"
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            placeholder={t("settings.commands.searchPlaceholder")}
            aria-label={t("settings.commands.searchPlaceholder")}
            className="w-full rounded-none border-0 border-b border-mid-gray/50 bg-transparent py-1 ps-7 pe-2 text-sm transition-colors placeholder:text-text/40 hover:border-mid-gray focus:border-logo-primary focus:outline-none"
          />
        </div>
      </div>

      {!filtering && (
        <SettingsGroup title={t("settings.commands.behavior")}>
          {/* Behavior toggles explain themselves: the caption text under each
              row states what the toggle does, so no row needs the persistent
              ⓘ (the info-icon audit: the icon stays only on genuinely
              non-obvious rows). */}
          <AutoInterpretCommands descriptionMode="inline" grouped={true} />
          <SpokenPunctuation descriptionMode="inline" grouped={true} />
          <NumberFormatSetting descriptionMode="inline" grouped={true} />
          <TerminalPunctuation descriptionMode="inline" grouped={true} />
          <VoiceDeletionCommands descriptionMode="inline" grouped={true} />
        </SettingsGroup>
      )}

      {/* The seeded Latin-script trigger phrases are the recognizer's
          English defaults (command_matrix.rs documents why Latin seeds stay
          English), so each default chip renders the command's translated
          name with the literal phrase as a dimmed gloss instead of a bare
          English string; the caption still names the arrangement and points
          at the add-phrase field for other languages. */}
      {visibleGroups.length > 0 && (
        <p className="px-4 text-xs text-secondary">
          {t("settings.commands.defaultPhrasesNote")}
        </p>
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
              defaultPhrases={defaultPhrases}
              defaultCount={
                defaults?.find((entry) => entry.command === command)?.phrases
                  .length ?? null
              }
              defaultsPending={defaultsPending}
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
        <p className="px-4 text-sm text-secondary">
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
            <Button variant="danger" size="sm" onClick={confirmResetAll}>
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
