import React, {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import { readFile } from "@tauri-apps/plugin-fs";
import {
  Check,
  ChevronDown,
  ChevronRight,
  Copy,
  FolderOpen,
  History,
  Mic,
  RotateCcw,
  Star,
  Trash2,
} from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import {
  commands,
  events,
  type HistoryEntry,
  type HistoryUpdatePayload,
} from "@/bindings";
import { useOsType } from "@/hooks/useOsType";
import { useSettings } from "@/hooks/useSettings";
import { diffText } from "@/lib/diff";
import { formatDate, formatDateTime, formatTimeOnly } from "@/utils/dateFormat";
import { AudioPlayer, AudioPlayerGroup } from "../../ui/AudioPlayer";
import { ToggleSwitch } from "../../ui/ToggleSwitch";
import { Button } from "../../ui/Button";
import { SettingsGroup } from "../../ui/SettingsGroup";
import { HistoryLimit } from "../HistoryLimit";
import { RecordingRetentionPeriodSelector } from "../RecordingRetentionPeriod";
import { DiffSummaryBadge, DiffView } from "../studio/DiffView";
import { copyToClipboard } from "./clipboard";

const IconButton: React.FC<{
  onClick: () => void;
  title: string;
  disabled?: boolean;
  active?: boolean;
  children: React.ReactNode;
}> = ({ onClick, title, disabled, active, children }) => (
  <button
    onClick={onClick}
    disabled={disabled}
    className={`p-1.5 rounded-md flex items-center justify-center transition-colors cursor-pointer disabled:cursor-not-allowed disabled:text-text/20 ${
      active
        ? "text-logo-primary hover:text-logo-primary/80"
        : "text-text/50 hover:text-logo-primary"
    }`}
    title={title}
  >
    {children}
  </button>
);

const PAGE_SIZE = 30;

/* The calendar-day key an entry groups under (YYYY-M-D in local time):
 * same-day rows share one section header instead of each repeating the
 * full long date. */
const dayKeyOf = (timestampSecs: number): string => {
  const date = new Date(timestampSecs * 1000);
  return `${date.getFullYear()}-${date.getMonth()}-${date.getDate()}`;
};

/* Section header label for a day: Today / Yesterday (localized) for the
 * two days people scan for, the localized long date for everything older. */
const dayHeaderLabel = (
  timestampSecs: number,
  locale: string,
  t: (key: string) => string,
): string => {
  const startOfDay = (date: Date) =>
    new Date(date.getFullYear(), date.getMonth(), date.getDate()).getTime();
  const now = new Date();
  const yesterday = new Date(now);
  yesterday.setDate(now.getDate() - 1);
  const day = startOfDay(new Date(timestampSecs * 1000));
  if (day === startOfDay(now)) return t("settings.history.today");
  if (day === startOfDay(yesterday)) return t("settings.history.yesterday");
  return formatDate(String(timestampSecs), locale);
};

/* Loading skeleton (2-3 rows mirroring the entry layout: date line, body
 * line, player row) so the pending pane reads as working, not stalled. */
const HistorySkeleton: React.FC = () => (
  <div className="divide-y divide-mid-gray/20" aria-hidden="true">
    {[0, 1, 2].map((row) => (
      <div key={row} className="px-4 py-2 pb-5 flex flex-col gap-3">
        <div className="h-3.5 w-24 rounded bg-mid-gray/20 animate-pulse" />
        <div className="flex flex-col gap-1.5">
          <div className="h-3.5 w-11/12 rounded bg-mid-gray/20 animate-pulse" />
          <div className="h-3.5 w-2/3 rounded bg-mid-gray/20 animate-pulse" />
        </div>
        <div className="h-5 w-40 rounded bg-mid-gray/20 animate-pulse" />
      </div>
    ))}
  </div>
);

/* The display form of a model id for history chips: the segment after the
 * last '/' (e.g. "parakeet-unified-en-0.6b-gguf"), falling back to the full
 * id when there is no slash. */
const shortModelId = (id: string): string => {
  const tail = id.split("/").pop()?.trim();
  return tail || id;
};

interface OpenRecordingsButtonProps {
  onClick: () => void;
  label: string;
}

const OpenRecordingsButton: React.FC<OpenRecordingsButtonProps> = ({
  onClick,
  label,
}) => (
  <Button
    onClick={onClick}
    variant="secondary"
    size="sm"
    className="flex items-center gap-2"
    title={label}
  >
    <FolderOpen className="w-4 h-4" />
    <span>{label}</span>
  </Button>
);

export const HistorySettings: React.FC = () => {
  const { t, i18n } = useTranslation();
  const osType = useOsType();
  const { settings, getSetting, updateSetting, isUpdating } = useSettings();

  const showModelBadge = getSetting("show_history_model") ?? true;
  const [entries, setEntries] = useState<HistoryEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [hasMore, setHasMore] = useState(true);
  const sentinelRef = useRef<HTMLDivElement>(null);
  const entriesRef = useRef<HistoryEntry[]>([]);
  const loadingRef = useRef(false);

  // Keep ref in sync for use in IntersectionObserver callback
  useEffect(() => {
    entriesRef.current = entries;
  }, [entries]);

  const loadPage = useCallback(async (cursor?: number) => {
    const isFirstPage = cursor === undefined;
    if (!isFirstPage && loadingRef.current) return;
    loadingRef.current = true;

    if (isFirstPage) setLoading(true);

    try {
      const result = await commands.getHistoryEntries(
        cursor ?? null,
        PAGE_SIZE,
      );
      if (result.status === "ok") {
        const { entries: newEntries, has_more } = result.data;
        setEntries((prev) =>
          isFirstPage ? newEntries : [...prev, ...newEntries],
        );
        setHasMore(has_more);
      }
    } catch (error) {
      console.error("Failed to load history entries:", error);
    } finally {
      setLoading(false);
      loadingRef.current = false;
    }
  }, []);

  // Initial load
  useEffect(() => {
    loadPage();
  }, [loadPage]);

  // Infinite scroll via IntersectionObserver
  useEffect(() => {
    if (loading) return;

    const sentinel = sentinelRef.current;
    if (!sentinel || !hasMore) return;

    const observer = new IntersectionObserver(
      (observerEntries) => {
        const first = observerEntries[0];
        if (first.isIntersecting) {
          const lastEntry = entriesRef.current[entriesRef.current.length - 1];
          if (lastEntry) {
            loadPage(lastEntry.id);
          }
        }
      },
      { threshold: 0 },
    );

    observer.observe(sentinel);
    return () => observer.disconnect();
  }, [loading, hasMore, loadPage]);

  // Listen for new entries added from the transcription pipeline
  useEffect(() => {
    const unlisten = events.historyUpdatePayload.listen((event) => {
      const payload: HistoryUpdatePayload = event.payload;
      if (payload.action === "added") {
        setEntries((prev) => [payload.entry, ...prev]);
      } else if (payload.action === "updated") {
        setEntries((prev) =>
          prev.map((e) => (e.id === payload.entry.id ? payload.entry : e)),
        );
      }
      // "deleted" and "toggled" are handled by optimistic updates only,
      // so we intentionally ignore them here to avoid double-mutation.
    });

    return () => {
      unlisten.then((fn) => fn());
    };
  }, []);

  const toggleSaved = async (id: number) => {
    // Optimistic update
    setEntries((prev) =>
      prev.map((e) => (e.id === id ? { ...e, saved: !e.saved } : e)),
    );
    try {
      const result = await commands.toggleHistoryEntrySaved(id);
      if (result.status !== "ok") {
        // Revert on failure
        setEntries((prev) =>
          prev.map((e) => (e.id === id ? { ...e, saved: !e.saved } : e)),
        );
      }
    } catch (error) {
      console.error("Failed to toggle saved status:", error);
      // Revert on failure
      setEntries((prev) =>
        prev.map((e) => (e.id === id ? { ...e, saved: !e.saved } : e)),
      );
    }
  };

  const getAudioUrl = useCallback(
    async (fileName: string) => {
      try {
        const result = await commands.getAudioFilePath(fileName);
        if (result.status === "ok") {
          if (osType === "linux") {
            const fileData = await readFile(result.data);
            const blob = new Blob([fileData], { type: "audio/wav" });
            return URL.createObjectURL(blob);
          }
          return convertFileSrc(result.data, "asset");
        }
        return null;
      } catch (error) {
        console.error("Failed to get audio file path:", error);
        return null;
      }
    },
    [osType],
  );

  const deleteAudioEntry = async (id: number) => {
    // Optimistically remove
    setEntries((prev) => prev.filter((e) => e.id !== id));
    try {
      const result = await commands.deleteHistoryEntry(id);
      if (result.status !== "ok") {
        // Reload on failure
        loadPage();
      }
    } catch (error) {
      console.error("Failed to delete entry:", error);
      loadPage();
    }
  };

  const retryHistoryEntry = async (id: number) => {
    const result = await commands.retryHistoryEntryTranscription(id);
    if (result.status !== "ok") {
      throw new Error(String(result.error));
    }
  };

  const openRecordingsFolder = async () => {
    try {
      const result = await commands.openRecordingsFolder();
      if (result.status !== "ok") {
        throw new Error(String(result.error));
      }
    } catch (error) {
      console.error("Failed to open recordings folder:", error);
      toast.error(t("errors.openDirectory", { error: String(error) }));
    }
  };

  let content: React.ReactNode;

  if (loading) {
    content = (
      <>
        <span className="sr-only">{t("settings.history.loading")}</span>
        <HistorySkeleton />
      </>
    );
  } else if (entries.length === 0) {
    // Empty state in the app's dashed-panel language (matching the Prompt
    // Studio's empty results): icon, a calm title without the exclamation
    // mark, and the next step that gets the first entry on screen.
    content = (
      <div className="px-4 py-10 flex flex-col items-center gap-2 text-center">
        <History className="h-5 w-5 text-text/40" aria-hidden="true" />
        <p className="text-sm text-secondary">
          {t("settings.history.emptyTitle")}
        </p>
        <p className="text-xs text-secondary max-w-sm">
          {t("settings.history.emptyHint")}
        </p>
        <Mic className="mt-2 h-4 w-4 text-text/30" aria-hidden="true" />
      </div>
    );
  } else {
    content = (
      <>
        <AudioPlayerGroup>
          <div className="divide-y divide-mid-gray/20">
            {entries.map((entry, index) => {
              /* A day change renders the section header; every row under it
               * carries only its time, so same-day entries stop repeating
               * the full long date. */
              const previous = entries[index - 1];
              const showDayHeader =
                !previous ||
                dayKeyOf(entry.timestamp) !== dayKeyOf(previous.timestamp);
              return (
                <React.Fragment key={entry.id}>
                  {showDayHeader && (
                    <div className="px-4 pt-3 pb-1 text-xs font-semibold text-text/70 uppercase tracking-wide">
                      {dayHeaderLabel(entry.timestamp, i18n.language, t)}
                    </div>
                  )}
                  <HistoryEntryComponent
                    entry={entry}
                    showModelBadge={showModelBadge}
                    promptName={
                      entry.post_process_prompt_id
                        ? settings?.post_process_prompts?.find(
                            (prompt) =>
                              prompt.id === entry.post_process_prompt_id,
                          )?.name
                        : undefined
                    }
                    onToggleSaved={() => toggleSaved(entry.id)}
                    onCopyText={() =>
                      // The FINAL text: what a polished dictation actually
                      // pasted (v1.6.0 W1-L6). Entries without post-processing
                      // keep copying the raw transcript, byte for byte.
                      copyToClipboard(
                        entry.post_processed_text ?? entry.transcription_text,
                      )
                    }
                    getAudioUrl={getAudioUrl}
                    deleteAudio={deleteAudioEntry}
                    retryTranscription={retryHistoryEntry}
                  />
                </React.Fragment>
              );
            })}
          </div>
        </AudioPlayerGroup>
        {/* Sentinel for infinite scroll */}
        <div ref={sentinelRef} className="h-1" />
      </>
    );
  }

  return (
    <div className="max-w-3xl w-full mx-auto space-y-6">
      {/* Settings precede data, exactly as on every other tab: the retention
          group sits above the entries list, with the model badge toggle that
          governs how history reads absorbed into it. */}
      <SettingsGroup title={t("settings.history.groups.retention")}>
        <HistoryLimit descriptionMode="tooltip" grouped={true} />
        <RecordingRetentionPeriodSelector
          descriptionMode="tooltip"
          grouped={true}
        />
        <ToggleSwitch
          checked={showModelBadge}
          onChange={(enabled) => updateSetting("show_history_model", enabled)}
          isUpdating={isUpdating("show_history_model")}
          label={t("settings.history.showModel.label")}
          description={t("settings.history.showModel.description")}
          descriptionMode="tooltip"
          grouped={true}
          tooltipPosition="bottom"
        />
      </SettingsGroup>
      <div className="space-y-2">
        <div className="px-4 flex items-center justify-between">
          <div>
            <h2 className="text-xs font-semibold text-text/70 uppercase tracking-wide">
              {t("settings.history.title")}
            </h2>
          </div>
          <OpenRecordingsButton
            onClick={openRecordingsFolder}
            label={t("settings.history.openFolder")}
          />
        </div>
        <div className="bg-background border border-mid-gray/20 rounded-lg overflow-visible">
          {content}
        </div>
      </div>
    </div>
  );
};

interface HistoryEntryProps {
  entry: HistoryEntry;
  showModelBadge: boolean;
  /** The selected prompt's display name for the entry's prompt id, when
   * the entry carries a post-process summary and the prompt still exists. */
  promptName?: string;
  onToggleSaved: () => void;
  onCopyText: () => Promise<boolean>;
  getAudioUrl: (fileName: string) => Promise<string | null>;
  deleteAudio: (id: number) => Promise<void>;
  retryTranscription: (id: number) => Promise<void>;
}

/* The localized word for a pp outcome token's base ("applied" |
 * "skipped:<reason>" | "failed:<class>"); the full token rides the row's
 * tooltip. */
const outcomeWord = (outcome: string, t: (key: string) => string): string => {
  const base = outcome.split(":")[0];
  return t(`settings.ppOutcome.${base}`);
};

const HistoryEntryComponent: React.FC<HistoryEntryProps> = ({
  entry,
  showModelBadge,
  promptName,
  onToggleSaved,
  onCopyText,
  getAudioUrl,
  deleteAudio,
  retryTranscription,
}) => {
  const { t, i18n } = useTranslation();
  const osType = useOsType();
  const [showCopied, setShowCopied] = useState(false);
  const [retrying, setRetrying] = useState(false);
  const [showChanges, setShowChanges] = useState(false);

  const hasTranscription = entry.transcription_text.trim().length > 0;

  // The FINAL text (v1.6.0 W1-L6): a polished dictation's body is what was
  // actually pasted, the persisted post-processed output, not the raw
  // transcript the polish consumed. Entries whose run failed, skipped, or
  // echoed the transcript byte-identically keep the pre-cycle body, so no
  // post-processing means an unchanged UI.
  const processedText = entry.post_processed_text;
  const hasChanges =
    processedText !== null && processedText !== entry.transcription_text;
  const finalText = processedText ?? entry.transcription_text;

  const changesSummary = useMemo(
    () =>
      hasChanges
        ? diffText(entry.transcription_text, processedText ?? "")
        : null,
    [hasChanges, entry.transcription_text, processedText],
  );

  const handleLoadAudio = useCallback(
    () => getAudioUrl(entry.file_name),
    [getAudioUrl, entry.file_name],
  );

  const handleCopyText = async () => {
    if (!hasTranscription) {
      return;
    }

    const copied = await onCopyText();
    if (!copied) {
      toast.error(t("settings.history.copyError"));
      return;
    }

    setShowCopied(true);
    setTimeout(() => setShowCopied(false), 2000);
  };

  const handleDeleteEntry = async () => {
    try {
      await deleteAudio(entry.id);
    } catch (error) {
      console.error("Failed to delete entry:", error);
      toast.error(t("settings.history.deleteError"));
    }
  };

  const handleRetranscribe = async () => {
    try {
      setRetrying(true);
      await retryTranscription(entry.id);
    } catch (error) {
      console.error("Failed to re-transcribe:", error);
      toast.error(t("settings.history.retranscribeError"));
    } finally {
      setRetrying(false);
    }
  };

  // Rows sit under a localized day-section header (Today / Yesterday / long
  // date), so the per-row label is the time alone; the full date rides the
  // tooltip for absolute reference.
  const formattedTime = formatTimeOnly(String(entry.timestamp), i18n.language);

  return (
    <div className="px-4 py-2 pb-5 flex flex-col gap-3">
      <div className="flex justify-between items-center">
        <div className="flex items-center gap-2 min-w-0">
          <p
            className="text-sm font-medium whitespace-nowrap"
            title={formatDateTime(String(entry.timestamp), i18n.language)}
          >
            {formattedTime}
          </p>
          {showModelBadge && entry.model_id ? (
            <span
              className="px-1.5 py-0.5 text-xs leading-none font-medium rounded bg-mid-gray/20 text-secondary truncate min-w-0 max-w-[16rem]"
              title={`${t("settings.history.model")}: ${entry.model_id}`}
            >
              {/* The short tail of the id (segment after the last '/') so the
                  chip names the model instead of truncating mid-path; the
                  full id rides along as the tooltip. */}
              {shortModelId(entry.model_id)}
            </span>
          ) : null}
          {/* The pp: run summary (WS3): which engine and prompt polished
           * this entry, how it ended, how long it took. Gated by the same
           * show_history_model toggle as the ASR chip; pre-cycle entries
           * carry no summary and render exactly as before. */}
          {showModelBadge && entry.post_process_model ? (
            <span
              className="px-1.5 py-0.5 text-xs leading-none font-medium rounded bg-mid-gray/10 text-text/60 truncate min-w-0 max-w-[16rem]"
              title={`${t("settings.history.viaPostProcess", {
                model: entry.post_process_model,
                prompt: promptName ?? entry.post_process_prompt_id ?? "-",
                outcome: entry.post_process_outcome ?? "-",
                latency: entry.post_process_latency_ms ?? 0,
              })}`}
            >
              {t("settings.history.viaPostProcess", {
                model: shortModelId(entry.post_process_model),
                prompt: promptName ?? entry.post_process_prompt_id ?? "-",
                outcome: outcomeWord(
                  entry.post_process_outcome ?? "applied",
                  t,
                ),
                latency: entry.post_process_latency_ms ?? 0,
              })}
            </span>
          ) : null}
        </div>
        <div className="flex items-center">
          <IconButton
            onClick={handleCopyText}
            disabled={!hasTranscription || retrying}
            title={t("settings.history.copyToClipboard")}
          >
            {showCopied ? (
              <Check width={16} height={16} />
            ) : (
              <Copy width={16} height={16} />
            )}
          </IconButton>
          <IconButton
            onClick={onToggleSaved}
            disabled={retrying}
            active={entry.saved}
            title={
              entry.saved
                ? t("settings.history.unsave")
                : t("settings.history.save")
            }
          >
            <Star
              width={16}
              height={16}
              fill={entry.saved ? "currentColor" : "none"}
            />
          </IconButton>
          <IconButton
            onClick={handleRetranscribe}
            disabled={retrying}
            title={t("settings.history.retranscribe")}
          >
            <RotateCcw
              width={16}
              height={16}
              style={
                retrying
                  ? { animation: "spin 1s linear infinite reverse" }
                  : undefined
              }
            />
          </IconButton>
          <IconButton
            onClick={handleDeleteEntry}
            disabled={retrying}
            title={t("settings.history.delete")}
          >
            <Trash2 width={16} height={16} />
          </IconButton>
        </div>
      </div>

      <p
        className={`italic text-sm pb-2 ${
          retrying
            ? ""
            : hasTranscription
              ? // W1-L9 visual pass: the transcript is the payload of this
                // tab, so it carries full-strength text; metadata around it
                // stays secondary.
                "text-text select-text cursor-text whitespace-pre-wrap break-words"
              : "text-text/40"
        }`}
        style={
          retrying
            ? { animation: "transcribe-pulse 3s ease-in-out infinite" }
            : undefined
        }
      >
        {retrying && (
          <style>{`
            @keyframes transcribe-pulse {
              0%, 100% { color: color-mix(in srgb, var(--color-text) 40%, transparent); }
              50% { color: color-mix(in srgb, var(--color-text) 90%, transparent); }
            }
          `}</style>
        )}
        {retrying
          ? t("settings.history.transcribing")
          : hasTranscription
            ? finalText
            : t("settings.history.transcriptionFailed")}
      </p>

      {/* The raw-to-processed diff (v1.6.0 W1-L6): only entries whose polish
          actually changed the text offer it; the expander summary chip names
          the volume of the change before anything opens. */}
      {hasChanges && !retrying && changesSummary ? (
        <div className="space-y-1.5">
          <button
            onClick={() => setShowChanges((open) => !open)}
            className="flex items-center gap-1.5 text-xs text-secondary hover:text-logo-primary cursor-pointer"
          >
            {showChanges ? (
              <ChevronDown width={14} height={14} />
            ) : (
              <ChevronRight width={14} height={14} />
            )}
            <span>
              {showChanges
                ? t("settings.history.hideChanges")
                : t("settings.history.viewChanges")}
            </span>
            <DiffSummaryBadge
              addedLines={changesSummary.summary.addedLines}
              removedLines={changesSummary.summary.removedLines}
              addedChars={changesSummary.summary.addedChars}
              removedChars={changesSummary.summary.removedChars}
              className="ml-1"
            />
          </button>
          {showChanges ? (
            <DiffView
              oldText={entry.transcription_text}
              newText={processedText ?? ""}
            />
          ) : null}
        </div>
      ) : null}

      {/* Eager metadata is enabled where resolving the audio URL is a cheap
       * path lookup (macOS/Windows asset URLs); on Linux getAudioUrl reads
       * the whole recording into a blob, which a full list must not do. */}
      <AudioPlayer
        onLoadRequest={handleLoadAudio}
        eagerMetadata={osType !== "linux"}
        className="w-full"
      />
    </div>
  );
};
