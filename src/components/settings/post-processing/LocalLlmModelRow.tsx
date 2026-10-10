import React, { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { listen } from "@tauri-apps/api/event";
import {
  commands,
  type LlmModelEntry,
  type LocalLlmModelStatus,
} from "@/bindings";
import { SettingContainer } from "@/components/ui";
import { Button } from "../../ui/Button";
import {
  LOCAL_LLM_MODEL_ID,
  LOCAL_LLM_MODEL_NAME,
  LOCAL_LLM_MODEL_SIZE_MB,
  isSwapRefusalError,
} from "./localLlmRouting";

// Payload of the shared "model-download-progress" event
// (DownloadProgress in src-tauri/src/managers/model.rs).
interface DownloadProgressEvent {
  model_id: string;
  percentage: number;
}

// Shape fallback for optimistic status updates that land before the first
// authoritative snapshot arrives. The row itself never renders this as a
// claim: while the snapshot is still unknown it shows the neutral
// checking state instead of asserting "Not downloaded".
const PENDING_STATUS = {
  downloaded: false,
  downloading: false,
  size_mb: LOCAL_LLM_MODEL_SIZE_MB,
  progress: 0,
} as const;

// The local on-device post-process model row (spec 6.2): name, size,
// status (Not downloaded / Downloading with percentage / Ready), and a
// Download or Delete button. Managed by the same ModelManager pipeline
// as voice models; the only UI for this model (it is filtered out of
// the ASR model lists). Stability over looks: no polling loop, status
// refreshes on mount, on command completion, and on the shared model
// manager events.
export const LocalLlmModelRow: React.FC = () => {
  const { t } = useTranslation();
  const [status, setStatus] = useState<LocalLlmModelStatus | null>(null);
  // The transient delete refusal (a post-process swap holds the model
  // files) gets its own "try again in a moment" copy; every other delete
  // failure is real and shows the backend error inline.
  const [deleteRefused, setDeleteRefused] = useState(false);
  const [deleteFailure, setDeleteFailure] = useState<string | null>(null);
  // The raw backend error of the last failed download; shown inline so a
  // failed 610 MB download is never silent (the toast via the shared
  // model-download-failed event is transient). Cleared on the next attempt.
  const [downloadError, setDownloadError] = useState<string | null>(null);

  const refreshStatus = useCallback(async () => {
    const result = await commands.getLocalLlmModelStatus();
    if (result.status === "ok") {
      setStatus(result.data);
    }
    // On error the row keeps its last known state; the settings panel is
    // not the place to surface backend errors beyond the delete refusal.
  }, []);

  useEffect(() => {
    void refreshStatus();

    // Live percentage while the ModelManager downloads the pinned model.
    const unlistenProgress = listen<DownloadProgressEvent>(
      "model-download-progress",
      (event) => {
        if (event.payload.model_id !== LOCAL_LLM_MODEL_ID) return;
        const percentage = event.payload.percentage;
        setStatus((prev) => ({
          ...(prev ?? PENDING_STATUS),
          downloading: true,
          progress: percentage,
        }));
      },
    );

    // Registry-wide refresh signals (download finished, cancelled,
    // deleted elsewhere): re-read the authoritative snapshot.
    const unlistenUpdated = listen("models-updated", () => {
      void refreshStatus();
    });
    const unlistenCancelled = listen<string>(
      "model-download-cancelled",
      (event) => {
        if (event.payload !== LOCAL_LLM_MODEL_ID) return;
        void refreshStatus();
      },
    );

    return () => {
      unlistenProgress.then((fn) => fn());
      unlistenUpdated.then((fn) => fn());
      unlistenCancelled.then((fn) => fn());
    };
  }, [refreshStatus]);

  const isDownloaded = status?.downloaded ?? false;
  const isDownloading = status?.downloading ?? false;
  // The first authoritative snapshot is still in flight: the row must not
  // claim "Not downloaded" with an enabled Download button in that window
  // (the model may actually be Ready or Downloading). A neutral checking
  // state and a disabled button bridge the gap.
  const isStatusUnknown = status === null;

  const handleDownload = useCallback(async () => {
    setDeleteRefused(false);
    setDeleteFailure(null);
    setDownloadError(null);
    // Optimistic: the command runs until the download completes; live
    // progress arrives via model-download-progress events.
    setStatus((prev) => ({
      ...(prev ?? PENDING_STATUS),
      downloading: true,
      downloaded: false,
    }));
    const result = await commands.downloadLocalLlmModel();
    if (result.status === "error") {
      // Keep the failure visible: the row would otherwise silently revert
      // to "Not downloaded" once the status refresh lands.
      console.error("local model download failed:", result.error);
      setDownloadError(result.error);
    }
    await refreshStatus();
  }, [refreshStatus]);

  const handleDelete = useCallback(async () => {
    setDeleteRefused(false);
    setDeleteFailure(null);
    setDownloadError(null);
    const result = await commands.deleteLocalLlmModel();
    if (result.status === "error") {
      console.error("local model delete failed:", result.error);
      if (isSwapRefusalError(result.error)) {
        // The refusal while a swap is running is transient (L3); surface
        // it inline so the user knows to try again in a moment.
        setDeleteRefused(true);
      } else {
        // A real failure (disk error, missing file): show what actually
        // went wrong instead of a false "a run is in progress" claim.
        setDeleteFailure(result.error);
      }
    }
    await refreshStatus();
  }, [refreshStatus]);

  const statusText = isStatusUnknown
    ? t("settings.postProcessing.local.modelStatus.checking")
    : isDownloading
      ? t("settings.postProcessing.local.modelStatus.downloading", {
          progress: Math.round(status?.progress ?? 0),
        })
      : isDownloaded
        ? t("settings.postProcessing.local.modelStatus.ready")
        : t("settings.postProcessing.local.modelStatus.notDownloaded");

  return (
    <SettingContainer
      title={t("settings.postProcessing.local.label")}
      description={
        <span className="text-xs">
          {LOCAL_LLM_MODEL_NAME}
          {" - "}
          {t("settings.postProcessing.local.modelSizeMb", {
            size: status?.size_mb ?? LOCAL_LLM_MODEL_SIZE_MB,
          })}
          {" - "}
          {statusText}
        </span>
      }
      descriptionMode="inline"
      layout="stacked"
      grouped={true}
    >
      <div className="flex items-center gap-2">
        {isDownloaded ? (
          <Button
            onClick={() => void handleDelete()}
            variant="secondary"
            size="md"
            disabled={isDownloading}
          >
            {t("settings.postProcessing.local.delete")}
          </Button>
        ) : (
          <Button
            onClick={() => void handleDownload()}
            variant="primary"
            size="md"
            disabled={isDownloading || isStatusUnknown}
          >
            {t("settings.postProcessing.local.download")}
          </Button>
        )}
        {deleteRefused && (
          <span className="text-xs text-red-400">
            {t("settings.postProcessing.local.deleteInUse")}
          </span>
        )}
        {deleteFailure && (
          <span className="text-xs text-red-400">
            {t("settings.postProcessing.local.deleteFailed", {
              error: deleteFailure,
            })}
          </span>
        )}
        {downloadError && (
          <span className="text-xs text-red-400">
            {t("settings.postProcessing.local.downloadFailed", {
              error: downloadError,
            })}
          </span>
        )}
      </div>
    </SettingContainer>
  );
};

// Props for one card in the Post-process Models section. The section owns
// the list and refresh; the card owns its own command calls and error
// surfaces, mirroring the pinned row's behavior row for row.
interface LlmModelCardProps {
  entry: LlmModelEntry;
  /// Live download percentage (0-100) while a transfer is in flight.
  progress?: number;
  /// Called after any command finishes so the section re-reads the list.
  onChanged: () => void;
}

// One post-process model card (the LocalLlmModelRow generalized to any
// catalog, user-added, or pinned entry): name, size, quant, context,
// publisher, status, Download / Use / Delete actions, an Active badge on
// the selection, and the same delete/download error wording as the pinned
// row (including the transient swap refusal).
export const LlmModelCard: React.FC<LlmModelCardProps> = ({
  entry,
  progress,
  onChanged,
}) => {
  const { t } = useTranslation();
  const [deleteRefused, setDeleteRefused] = useState(false);
  const [deleteFailure, setDeleteFailure] = useState<string | null>(null);
  const [downloadError, setDownloadError] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);

  const isDownloaded = entry.info.is_downloaded;
  const isDownloading = entry.info.is_downloading;

  const handleDownload = useCallback(async () => {
    setDeleteRefused(false);
    setDeleteFailure(null);
    setDownloadError(null);
    const result = await commands.downloadLocalLlmModel(entry.info.id);
    if (result.status === "error") {
      console.error("post-process model download failed:", result.error);
      setDownloadError(result.error);
    }
    onChanged();
  }, [entry.info.id, onChanged]);

  const handleDelete = useCallback(async () => {
    setDeleteRefused(false);
    setDeleteFailure(null);
    setDownloadError(null);
    const result = await commands.deleteLocalLlmModel(entry.info.id);
    if (result.status === "error") {
      console.error("post-process model delete failed:", result.error);
      if (isSwapRefusalError(result.error)) {
        setDeleteRefused(true);
      } else {
        setDeleteFailure(result.error);
      }
    }
    onChanged();
  }, [entry.info.id, onChanged]);

  const handleSelect = useCallback(async () => {
    setActionError(null);
    const result = await commands.setPostProcessLocalModel(entry.info.id);
    if (result.status === "error") {
      console.error("post-process model selection failed:", result.error);
      setActionError(result.error);
    }
    onChanged();
  }, [entry.info.id, onChanged]);

  const statusText = isDownloading
    ? t("settings.postProcessing.local.modelStatus.downloading", {
        progress: Math.round(progress ?? 0),
      })
    : isDownloaded
      ? t("settings.postProcessing.local.modelStatus.ready")
      : t("settings.postProcessing.local.modelStatus.notDownloaded");

  const metaParts = [
    t("settings.postProcessing.local.modelSizeMb", {
      size: entry.info.size_mb,
    }),
  ];
  if (entry.quant) {
    metaParts.push(entry.quant);
  }
  metaParts.push(
    t("settings.postProcessing.models.contextTokens", {
      count: entry.context_tokens,
    }),
  );

  // The Active badge rides the description (a styled node) because the
  // container's title is a plain string.
  const activeBadge = entry.selected ? (
    <span className="px-1.5 py-0.5 text-xs font-medium rounded bg-logo-primary/20 text-logo-primary">
      {t("settings.postProcessing.models.active")}
    </span>
  ) : null;

  return (
    <SettingContainer
      title={entry.info.name}
      description={
        <span className="text-xs inline-flex items-center gap-2 flex-wrap">
          <span>
            {metaParts.join(" - ")} -{" "}
            {t("settings.postProcessing.models.publisher", {
              publisher: entry.publisher,
            })}{" "}
            - {statusText}
          </span>
          {activeBadge}
        </span>
      }
      descriptionMode="inline"
      layout="stacked"
      grouped={true}
    >
      <div className="flex items-center gap-2">
        {isDownloaded ? (
          <>
            {!entry.selected && (
              <Button
                onClick={() => void handleSelect()}
                variant="primary"
                size="md"
                disabled={isDownloading}
              >
                {t("settings.postProcessing.models.select")}
              </Button>
            )}
            <Button
              onClick={() => void handleDelete()}
              variant="secondary"
              size="md"
              disabled={isDownloading}
            >
              {t("settings.postProcessing.local.delete")}
            </Button>
          </>
        ) : (
          <Button
            onClick={() => void handleDownload()}
            variant="primary"
            size="md"
            disabled={isDownloading}
          >
            {t("settings.postProcessing.local.download")}
          </Button>
        )}
        {deleteRefused && (
          <span className="text-xs text-red-400">
            {t("settings.postProcessing.local.deleteInUse")}
          </span>
        )}
        {deleteFailure && (
          <span className="text-xs text-red-400">
            {t("settings.postProcessing.local.deleteFailed", {
              error: deleteFailure,
            })}
          </span>
        )}
        {downloadError && (
          <span className="text-xs text-red-400">
            {t("settings.postProcessing.local.downloadFailed", {
              error: downloadError,
            })}
          </span>
        )}
        {actionError && (
          <span className="text-xs text-red-400">{actionError}</span>
        )}
      </div>
    </SettingContainer>
  );
};
