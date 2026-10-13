import React, {
  useCallback,
  useEffect,
  useMemo,
  useState,
  useSyncExternalStore,
} from "react";
import { useTranslation } from "react-i18next";
import { FlaskConical, Play, RotateCcw, Square, Loader2 } from "lucide-react";
import { commands, type HistoryEntry, type StudioSource } from "@/bindings";
import { useSettings } from "@/hooks/useSettings";
import { formatDateTime } from "@/utils/dateFormat";
import { Alert } from "../../ui/Alert";
import Badge from "../../ui/Badge";
import { Button } from "../../ui/Button";
import { Dropdown, SettingContainer } from "@/components/ui";
import { DiffView } from "./DiffView";
import {
  buildBatch,
  isPartialFailure,
  keptRaw,
  outcomeKind,
  type StudioCardState,
  type StudioPair,
} from "./studioRunState";
import {
  cancelStudioBatch,
  runStudioBatch,
  studioBatchState,
  subscribeStudioBatch,
  type StudioBatchError,
} from "./studioBatch";
import { PRELOAD_PROMPT_STUDIO_EVENT } from "./studioEvents";
import type { AppleIntelligenceStatus } from "../PostProcessingSettingsApi/usePostProcessProviderState";

const STUDIO_HISTORY_PAGE_SIZE = 50;
const LOCAL_PROVIDER_ID = "local";
const APPLE_PROVIDER_ID = "apple_intelligence";

/* The display form of a model id: the segment after the last '/', falling
 * back to the whole id (the History chip convention). */
const shortModelId = (id: string): string => id.split("/").pop()?.trim() || id;

const LAST_TRANSCRIPT_VALUE = "last";

/** One checkbox row shared by the prompt and engine pickers. */
const CheckRow: React.FC<{
  checked: boolean;
  onToggle: () => void;
  label: string;
  note?: string;
  disabled?: boolean;
}> = ({ checked, onToggle, label, note, disabled }) => (
  <label
    className={`flex items-center gap-2.5 px-2.5 py-1.5 rounded-md border transition-colors ${
      checked
        ? "border-logo-primary bg-logo-primary/5"
        : "border-mid-gray/20 hover:bg-mid-gray/10"
    } ${disabled ? "opacity-50 cursor-not-allowed" : "cursor-pointer"}`}
  >
    <input
      type="checkbox"
      checked={checked}
      onChange={onToggle}
      disabled={disabled}
      className="h-3.5 w-3.5 shrink-0 accent-[var(--color-background-ui)]"
    />
    <span className="min-w-0 flex-1">
      <span className="block text-sm font-medium truncate">{label}</span>
      {note ? (
        <span className="block text-xs text-secondary truncate">{note}</span>
      ) : null}
    </span>
  </label>
);

/** One result card: the pair it ran, its outcome and latency, and the diff
 * against the source text (or the raw-kept note when the engine never
 * produced output). Re-runnable from its header. */
const ResultCard: React.FC<{
  card: StudioCardState;
  promptName: string;
  engineLabel: string;
  onRerun: (pair: StudioPair) => void;
  rerunDisabled: boolean;
}> = ({ card, promptName, engineLabel, onRerun, rerunDisabled }) => {
  const { t } = useTranslation();
  const outcomeToken = card.result?.outcome ?? "";
  const kind = outcomeKind(outcomeToken);
  const outcomeBase = outcomeToken.split(":")[0];

  return (
    <div className="p-3 rounded-md border border-mid-gray/20 space-y-2 min-w-0">
      <div className="flex items-start justify-between gap-2">
        <div className="min-w-0">
          <p className="text-sm font-medium truncate" title={promptName}>
            {promptName}
          </p>
          <p className="text-xs text-secondary truncate" title={engineLabel}>
            {engineLabel}
          </p>
        </div>
        <button
          onClick={() =>
            onRerun({ promptId: card.promptId, providerId: card.providerId })
          }
          disabled={rerunDisabled || card.status === "running"}
          className="p-1.5 rounded-md flex items-center justify-center transition-colors cursor-pointer disabled:cursor-not-allowed disabled:text-text/20 text-text/50 hover:text-logo-primary shrink-0"
          title={t("settings.studio.rerun")}
        >
          <RotateCcw
            width={14}
            height={14}
            style={
              card.status === "running"
                ? { animation: "spin 1s linear infinite reverse" }
                : undefined
            }
          />
        </button>
      </div>

      {card.status === "running" ? (
        <p className="flex items-center gap-1.5 text-xs text-secondary">
          {/* In-flight feedback: the spinner keeps the running state visibly
           * alive instead of static gray text a stalled run would render
           * exactly like. */}
          <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden="true" />
          {t("settings.studio.running")}
        </p>
      ) : card.status === "cancelled" ? (
        /* KB-257: the cancel landed before this pair's turn; it never
           ran, so there is no outcome to badge or diff to show. */
        <p className="text-xs text-secondary">
          {t("settings.studio.cancelledCard")}
        </p>
      ) : card.result ? (
        <>
          <div className="flex flex-wrap items-center gap-2">
            <Badge
              variant={kind === "applied" ? "success" : "secondary"}
              className={kind === "failed" ? "text-error" : undefined}
            >
              {/* "Success", not "applied": the studio types and saves
                 nothing, so an "applied" pill contradicted the run
                 caption beside it. The other outcome words (skipped,
                 failed, unchanged) make no such claim and stay as-is. */}
              {kind === "applied"
                ? t("settings.studio.appliedLabel")
                : t(`settings.ppOutcome.${outcomeBase}`)}
              {kind === "other" ? ` (${outcomeToken})` : ""}
            </Badge>
            <span className="text-xs text-secondary">
              {t("settings.studio.latency", { ms: card.result.latency_ms })}
            </span>
          </div>
          {keptRaw(outcomeToken) ? (
            <p className="text-xs text-warning">
              {t("settings.studio.rawKept")}
            </p>
          ) : (
            <DiffView
              oldText={card.result.before}
              newText={card.result.after ?? card.result.before}
            />
          )}
        </>
      ) : null}
    </div>
  );
};

/**
 * The Prompt Studio (v1.6.0 W1-L6): pick a source transcript, pick one or
 * more templates AND engines, run them as one batch, and compare the
 * results side by side. Every run goes through the exact engine lifecycle
 * a dictation uses, but pastes nothing and writes no history row (the
 * backend seam guarantees both); the cards are labeled per pair and each
 * is re-runnable on its own.
 */
export const PromptStudio: React.FC = () => {
  const { t, i18n } = useTranslation();
  const { settings } = useSettings();

  const prompts = settings?.post_process_prompts ?? [];
  const providers = settings?.post_process_providers ?? [];
  const models = settings?.post_process_models ?? {};

  const [selectedPrompts, setSelectedPrompts] = useState<Set<string>>(
    () =>
      new Set(
        settings?.post_process_selected_prompt_id
          ? [settings.post_process_selected_prompt_id]
          : [],
      ),
  );
  const [selectedEngines, setSelectedEngines] = useState<Set<string>>(
    () =>
      new Set(
        settings?.post_process_provider_id
          ? [settings.post_process_provider_id]
          : [],
      ),
  );
  const [sourceValue, setSourceValue] = useState<string>(LAST_TRANSCRIPT_VALUE);
  const [entries, setEntries] = useState<HistoryEntry[]>([]);
  const [entriesLoading, setEntriesLoading] = useState(true);
  // KB-262: the engine row's note follows the tri-state probe so a build
  // lacking Apple Intelligence reads its own copy, not the OS-requirements
  // one.
  const [appleStatus, setAppleStatus] = useState<AppleIntelligenceStatus>(
    "available",
  );
  // KB-257: the batch state (cards, in-flight flag, error) lives in the
  // module-level controller, so navigating away and back reflects the
  // still-running batch instead of discarding it - and a remount can no
  // longer stack a second batch behind the orphaned first one.
  const batchState = useSyncExternalStore(
    subscribeStudioBatch,
    studioBatchState,
  );
  const { cards, inFlight: batchInFlight, error: batchError } = batchState;

  // The source picker's entry list: the newest page is plenty (the dropdown
  // shows date + snippet; older entries stay reachable from History).
  useEffect(() => {
    let cancelled = false;
    setEntriesLoading(true);
    commands
      .getHistoryEntries(null, STUDIO_HISTORY_PAGE_SIZE)
      .then((result) => {
        if (!cancelled && result.status === "ok") {
          setEntries(result.data.entries);
        }
      })
      .catch((error) => {
        console.error("Failed to load history entries for the studio:", error);
      })
      .finally(() => {
        if (!cancelled) setEntriesLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  // Apple Intelligence "as available": the provider row stays listed but
  // checks off with a note when the probe refuses it (KB-262: the note
  // words a build lacking AI differently from an OS lacking it).
  useEffect(() => {
    if (!providers.some((provider) => provider.id === APPLE_PROVIDER_ID)) {
      return;
    }
    let cancelled = false;
    commands
      .checkAppleIntelligenceAvailable()
      .then((status) => {
        if (!cancelled) setAppleStatus(status);
      })
      .catch((error) => {
        console.error("Apple Intelligence availability check failed:", error);
        // Probe transport failure: the conservative verdict is the
        // OS-unsupported copy the bool probe always produced.
        if (!cancelled) setAppleStatus("os_unsupported");
      });
    return () => {
      cancelled = true;
    };
  }, [providers]);

  // The per-template Test button lands here: arrive with that template
  // checked (App.tsx re-dispatches the preload event once mounted).
  useEffect(() => {
    const listener = (event: Event) => {
      const promptId = (event as CustomEvent<{ promptId: string }>).detail
        ?.promptId;
      if (!promptId) return;
      setSelectedPrompts((prev) => new Set(prev).add(promptId));
    };
    window.addEventListener(PRELOAD_PROMPT_STUDIO_EVENT, listener);
    return () =>
      window.removeEventListener(PRELOAD_PROMPT_STUDIO_EVENT, listener);
  }, []);

  const promptName = useCallback(
    (promptId: string): string => {
      const prompt = prompts.find((p) => p.id === promptId);
      if (!prompt) return promptId;
      return prompt.is_builtin
        ? t(`settings.postProcessing.prompts.templates.${prompt.id}.name`, {
            defaultValue: prompt.name,
          })
        : prompt.name;
    },
    [prompts, t],
  );

  const engineLabel = useCallback(
    (providerId: string): string => {
      if (providerId === LOCAL_PROVIDER_ID) {
        return t("settings.postProcessing.local.providerLabel");
      }
      return providers.find((p) => p.id === providerId)?.label ?? providerId;
    },
    [providers, t],
  );

  const toggle = (
    set: Set<string>,
    setter: (next: Set<string>) => void,
    id: string,
  ) => {
    const next = new Set(set);
    if (next.has(id)) {
      next.delete(id);
    } else {
      next.add(id);
    }
    setter(next);
  };

  const batch = useMemo(
    () => buildBatch([...selectedPrompts], [...selectedEngines]),
    [selectedPrompts, selectedEngines],
  );
  const canRun = batch.length > 0 && !batchInFlight && !entriesLoading;

  const sourceForRun = (): StudioSource =>
    sourceValue === LAST_TRANSCRIPT_VALUE
      ? { kind: "last_transcript" }
      : { kind: "history_entry", id: Number(sourceValue) };

  const errorText = (error: StudioBatchError): string => {
    if (error.kind === "raw") return error.message;
    switch (error.error.kind) {
      case "no_history":
        return t("settings.postProcessing.prompts.testNoHistory");
      case "prompt_not_found":
        return t("settings.postProcessing.prompts.testPromptNotFound");
      case "engine_not_found":
        return t("settings.studio.errors.engineNotFound", {
          id: error.error.id,
        });
      case "entry_not_found":
        return t("settings.studio.errors.entryNotFound");
      case "empty_batch":
        return t("settings.studio.errors.emptyBatch");
      default:
        return t("settings.postProcessing.prompts.testFailed", {
          detail: error.error.detail,
        });
    }
  };

  const runPairs = (pairs: StudioPair[], fresh: boolean) => {
    runStudioBatch(pairs, sourceForRun(), fresh);
  };

  const handleRun = () => {
    if (!canRun) return;
    runPairs(batch, true);
  };

  const handleRerun = (pair: StudioPair) => {
    runPairs([pair], false);
  };

  const handleCancel = () => {
    cancelStudioBatch();
  };

  const sourceOptions = useMemo(() => {
    const options = [
      {
        value: LAST_TRANSCRIPT_VALUE,
        label: t("settings.studio.source.last"),
      },
    ];
    for (const entry of entries) {
      const snippet = entry.transcription_text.trim().slice(0, 48);
      options.push({
        value: String(entry.id),
        label: `${formatDateTime(String(entry.timestamp), i18n.language)}${
          snippet ? ` - ${snippet}` : ""
        }`,
      });
    }
    return options;
  }, [entries, i18n.language, t]);

  const doneCards = cards.filter((card) => card.status === "done");
  const partialFailure = isPartialFailure(cards);

  return (
    <div className="max-w-3xl w-full mx-auto space-y-6">
      <div>
        <h1 className="text-xl font-medium mb-2">
          {t("settings.studio.title")}
        </h1>
        <p className="mt-1 text-xs text-secondary">
          {t("settings.studio.intro")}
        </p>
      </div>

      <SettingContainer
        title={t("settings.studio.source.label")}
        description={t("settings.studio.source.description")}
        descriptionMode="tooltip"
        layout="stacked"
        grouped={true}
      >
        <div className="space-y-2">
          <Dropdown
            options={sourceOptions}
            selectedValue={sourceValue}
            onSelect={setSourceValue}
            disabled={entriesLoading || batchInFlight}
            placeholder={
              entriesLoading
                ? t("settings.studio.source.loading")
                : t("settings.studio.source.label")
            }
            className="w-full"
          />
          {entriesLoading ? (
            <p className="text-xs text-secondary">
              {t("settings.studio.source.loading")}
            </p>
          ) : entries.length === 0 ? (
            <p className="text-xs text-secondary">
              {t("settings.postProcessing.prompts.testNoHistory")}
            </p>
          ) : null}
        </div>
      </SettingContainer>

      <SettingContainer
        title={t("settings.studio.prompts.label")}
        description={t("settings.studio.prompts.description")}
        descriptionMode="tooltip"
        layout="stacked"
        grouped={true}
      >
        <div className="grid gap-1.5 sm:grid-cols-2">
          {prompts.map((prompt) => (
            <CheckRow
              key={prompt.id}
              checked={selectedPrompts.has(prompt.id)}
              onToggle={() =>
                toggle(selectedPrompts, setSelectedPrompts, prompt.id)
              }
              label={promptName(prompt.id)}
              /* W1-L9: the note localizes the register word (the row above
                 showed the raw token, e.g. "auto - general") and separates
                 the two facts with a middot. */
              note={`${prompt.language ?? "auto"} \u00b7 ${t(
                `settings.postProcessing.prompts.registers.${
                  prompt.register ?? "general"
                }`,
                { defaultValue: prompt.register ?? "general" },
              )}`}
            />
          ))}
        </div>
      </SettingContainer>

      <SettingContainer
        title={t("settings.studio.engines.label")}
        description={t("settings.studio.engines.description")}
        descriptionMode="tooltip"
        layout="stacked"
        grouped={true}
      >
        <div className="grid gap-1.5 sm:grid-cols-2">
          {providers.map((provider) => {
            const apple = provider.id === APPLE_PROVIDER_ID;
            // KB-262: the unavailable note words a build lacking Apple
            // Intelligence differently from an OS lacking it (one new
            // locale string, shared with the settings surface).
            const appleNote =
              appleStatus === "build_lacks"
                ? t("settings.postProcessing.api.appleIntelligence.buildLacks")
                : t("settings.studio.engines.appleUnavailable");
            const model = models[provider.id] ?? "";
            const note =
              apple && appleStatus !== "available"
                ? appleNote
                : model
                  ? shortModelId(model)
                  : t("settings.studio.engines.noModelHint");
            return (
              <CheckRow
                key={provider.id}
                checked={selectedEngines.has(provider.id)}
                onToggle={() =>
                  toggle(selectedEngines, setSelectedEngines, provider.id)
                }
                label={engineLabel(provider.id)}
                note={note}
              />
            );
          })}
        </div>
      </SettingContainer>

      <div className="flex flex-wrap items-center gap-3">
        <Button
          onClick={handleRun}
          variant="primary"
          size="md"
          disabled={!canRun}
          className="flex items-center gap-2"
        >
          <Play width={14} height={14} />
          <span>
            {batchInFlight
              ? t("settings.studio.running")
              : t("settings.studio.run", { count: batch.length })}
          </span>
        </Button>
        {batchInFlight ? (
          /* KB-257: the cancel affordance. A batch can grind for as long
             as the provider timeout; before this, leaving the page was
             the only way to (invisibly) abandon it. */
          <Button
            onClick={handleCancel}
            variant="secondary"
            size="md"
            className="flex items-center gap-2"
          >
            <Square width={12} height={12} aria-hidden="true" />
            <span>{t("settings.studio.cancel")}</span>
          </Button>
        ) : null}
        <span className="text-xs text-secondary">
          {t("settings.studio.runHint", { count: batch.length })}
        </span>
      </div>

      {batchError ? (
        <Alert variant="error" contained>
          {errorText(batchError)}
        </Alert>
      ) : null}

      {partialFailure ? (
        <Alert variant="warning" contained>
          {t("settings.studio.partialFailure")}
        </Alert>
      ) : null}

      {cards.length > 0 ? (
        <div className="space-y-2">
          <h3 className="text-xs font-semibold text-text/70 uppercase tracking-wide">
            {t("settings.studio.results", { count: doneCards.length })}
          </h3>
          <div className="grid gap-3 md:grid-cols-2">
            {cards.map((card) => (
              <ResultCard
                key={card.key}
                card={card}
                promptName={promptName(card.promptId)}
                engineLabel={engineLabel(card.providerId)}
                onRerun={handleRerun}
                rerunDisabled={batchInFlight}
              />
            ))}
          </div>
        </div>
      ) : (
        /* W1-L9 visual pass: before the first run the page ended at the
           Run button, so a first-time user could not tell WHERE results
           would land or that the run is safe. This panel holds the space
           and answers both. */
        !batchError && (
          <div className="rounded-lg border border-dashed border-mid-gray/40 px-4 py-8 flex flex-col items-center gap-2 text-center">
            <FlaskConical className="h-5 w-5 text-text/40" aria-hidden="true" />
            <p className="text-sm text-secondary">
              {t("settings.studio.resultsEmptyTitle")}
            </p>
            <p className="text-xs text-secondary max-w-sm">
              {t("settings.studio.resultsEmptyHint")}
            </p>
          </div>
        )
      )}
    </div>
  );
};
