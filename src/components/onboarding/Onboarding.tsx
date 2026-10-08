import React, { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { listen } from "@tauri-apps/api/event";
import { ChevronDown } from "lucide-react";
import { commands, type ModelInfo } from "@/bindings";
import type { ModelCardStatus } from "./ModelCard";
import ModelCard, { isLegacySource } from "./ModelCard";
import HandyTextLogo from "../icons/HandyTextLogo";
import { useModelStore } from "../../stores/modelStore";
import { useSettings } from "../../hooks/useSettings";
import { useOsType } from "../../hooks/useOsType";
import { formatKeyCombination } from "../../lib/utils/keyboard";
import type { ModelStateEvent } from "../../lib/types/events";
import {
  formatMarginMb,
  formatMemoryAmount,
  parseRefusal,
  refusalActions,
  type MemoryGateRefusal,
  type RefusalAction,
} from "./modelRefusal";

interface OnboardingProps {
  onModelSelected: () => void;
  preview?: boolean;
}

/** A memory-gate refusal of the pending selection, with the data the
 * recovery card renders. */
interface RefusalCardState {
  modelId: string;
  modelName: string;
  refusal: MemoryGateRefusal;
}

const refusalActionLabelKey: Record<RefusalAction, string> = {
  retry: "onboarding.refusal.retry",
  "switch-model": "onboarding.refusal.switchModel",
  "disable-guard": "onboarding.refusal.disableGuard",
  "continue-deferred": "onboarding.refusal.continueDeferred",
};

const Onboarding: React.FC<OnboardingProps> = ({
  onModelSelected,
  preview = false,
}) => {
  const { t } = useTranslation();
  const {
    models,
    downloadModel,
    selectModel,
    downloadingModels,
    verifyingModels,
    extractingModels,
    downloadProgress,
    downloadStats,
    cancelDownload,
  } = useModelStore();
  const { getSetting, updateSetting } = useSettings();
  const osType = useOsType();
  const [selectedModelId, setSelectedModelId] = useState<string | null>(null);
  const [showAll, setShowAll] = useState(false);
  const [refusalCard, setRefusalCard] = useState<RefusalCardState | null>(null);
  const hasStartedSelection = useRef(false);
  // The pending selection at event time: the refusal card must only answer
  // the load this component itself started.
  const pendingSelectionRef = useRef<string | null>(null);

  useEffect(() => {
    pendingSelectionRef.current = selectedModelId;
  }, [selectedModelId]);

  const guardEnabled = getSetting("memory_pressure_guard") ?? true;

  const isBusy = selectedModelId !== null;

  // Curate the download list: legacy (.bin/ONNX) downloads are deprecated and
  // never shown here (they still appear in the compatible section if already on
  // disk). The catalog arrives rank-sorted, so the first two recommended models
  // are the featured picks - currently Parakeet Unified (English) and Nemotron
  // Streaming (multilingual). Everything else hides behind "Show all".
  const { downloadable, topPicks, otherRecommended, rest } = useMemo(() => {
    const downloadable = models.filter(
      (m: ModelInfo) => !m.is_downloaded && !isLegacySource(m),
    );
    const recommended = downloadable.filter((m: ModelInfo) => m.is_recommended);
    // `models` arrives in editorial rank order (the backend sorts by rank_of,
    // then accuracy), so keep that order here: ranked-but-not-recommended models
    // surface first, then the unranked tail by accuracy.
    const rest = downloadable.filter((m: ModelInfo) => !m.is_recommended);
    return {
      downloadable,
      topPicks: recommended.slice(0, 2),
      otherRecommended: recommended.slice(2),
      rest,
    };
  }, [models]);

  const hasRecommended = topPicks.length > 0 || otherRecommended.length > 0;
  // When nothing recommended remains to download (e.g. all already on disk),
  // there is no curated subset to collapse, so just show the full list.
  const showRest = showAll || !hasRecommended;

  // Watch for the selected model to finish downloading + verifying + extracting
  useEffect(() => {
    // Debug previews are inert: never switch the user's active model. Guarded
    // here as well as in the handlers because this is where the backend call
    // actually happens.
    if (preview) return;

    if (!selectedModelId) {
      hasStartedSelection.current = false;
      return;
    }

    const model = models.find((m) => m.id === selectedModelId);
    const stillDownloading = selectedModelId in downloadingModels;
    const stillVerifying = selectedModelId in verifyingModels;
    const stillExtracting = selectedModelId in extractingModels;

    if (
      model?.is_downloaded &&
      !stillDownloading &&
      !stillVerifying &&
      !stillExtracting &&
      !hasStartedSelection.current
    ) {
      hasStartedSelection.current = true;

      // Model is ready - select it and transition
      selectModel(selectedModelId).then((success) => {
        if (success) {
          onModelSelected();
        } else {
          // No duplicate generic toast here: the App-level loading_failed
          // toast already carries the reason, and the persistent inline
          // refusal card (fed by the listener below) is the real surface.
          hasStartedSelection.current = false;
          setSelectedModelId(null);
        }
      });
    }
  }, [
    selectedModelId,
    models,
    downloadingModels,
    verifyingModels,
    extractingModels,
    selectModel,
    onModelSelected,
    preview,
    t,
  ]);

  // A memory-gate refusal of the pending selection becomes a persistent
  // inline card with the structured numbers and four recovery actions; it
  // never lives only in a transient toast. Debug previews stay inert.
  useEffect(() => {
    if (preview) return;
    const unlisten = listen<ModelStateEvent>("model-state-changed", (event) => {
      const payload = event.payload;
      const refusal = parseRefusal(payload);
      if (!refusal) return;
      if (payload.model_id !== pendingSelectionRef.current) return;
      setRefusalCard({
        modelId: payload.model_id ?? "",
        modelName: payload.model_name ?? payload.model_id ?? "",
        refusal,
      });
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, [preview]);

  const handleRefusalAction = async (action: RefusalAction) => {
    if (preview || !refusalCard) return;
    const id = refusalCard.modelId;
    switch (action) {
      case "retry":
        setRefusalCard(null);
        setSelectedModelId(id);
        break;
      case "switch-model":
        setRefusalCard(null);
        setSelectedModelId(null);
        break;
      case "disable-guard":
        // The override the old refusal message pointed at but onboarding
        // could never reach: flip the guard off, then retry the load. The
        // toggle is reversible in Settings > Advanced.
        setRefusalCard(null);
        await updateSetting("memory_pressure_guard", false);
        setSelectedModelId(id);
        break;
      case "continue-deferred":
        try {
          const result = await commands.setActiveModelDeferred(id);
          if (result.status === "ok") {
            setRefusalCard(null);
            onModelSelected();
          }
        } catch (e) {
          console.error("Failed to defer model selection:", e);
        }
        break;
    }
  };

  const handleDownloadModel = async (modelId: string) => {
    if (preview) return;

    setSelectedModelId(modelId);

    // Error toast is handled centrally by the model-download-failed event listener
    // in modelStore - no toast here to avoid duplicates.
    const success = await downloadModel(modelId);
    if (!success) {
      setSelectedModelId(null);
    }
  };

  const handleCancelDownload = async (modelId: string) => {
    if (preview) return;

    const success = await cancelDownload(modelId);
    if (success) {
      setSelectedModelId(null);
    }
  };

  const handleSelectExistingModel = (modelId: string) => {
    if (preview) return;

    setSelectedModelId(modelId);
  };

  const getModelStatus = (modelId: string): ModelCardStatus => {
    if (modelId in extractingModels) return "extracting";
    if (modelId in verifyingModels) return "verifying";
    if (modelId in downloadingModels) return "downloading";
    return "downloadable";
  };

  const getExistingModelStatus = (modelId: string): ModelCardStatus => {
    if (selectedModelId === modelId) return "switching";
    return "available";
  };

  const getModelDownloadProgress = (modelId: string): number | undefined => {
    return downloadProgress[modelId]?.percentage;
  };

  const getModelDownloadSpeed = (modelId: string): number | undefined => {
    return downloadStats[modelId]?.speed;
  };

  // The start-dictation binding, shown during onboarding because it appears
  // nowhere else before the main app: default option+space per settings.rs.
  const bindings = getSetting("bindings");
  const hotkeyBinding =
    bindings?.["transcribe"]?.current_binding || "option+space";
  const hotkeyDisplay = formatKeyCombination(hotkeyBinding, osType);

  return (
    <div className="h-screen w-full flex flex-col p-6 gap-4">
      <div className="flex flex-col items-center gap-2 shrink-0">
        <HandyTextLogo width={200} />
        <p className="text-text/70 max-w-md font-medium mx-auto">
          {t("onboarding.subtitle")}
        </p>
        <p className="text-xs text-text/50">
          {t("onboarding.hotkeyHint", { hotkey: hotkeyDisplay })}
        </p>
      </div>

      <div className="max-w-[600px] w-full mx-auto text-center flex-1 flex flex-col min-h-0">
        <div className="space-y-6 pb-6">
          {refusalCard && (
            <div
              data-testid="memory-refusal-card"
              className="rounded-xl border border-red-500/30 bg-red-500/5 p-4 space-y-3 text-left"
            >
              <h2 className="text-sm font-semibold text-text">
                {t("onboarding.refusal.title")}
              </h2>
              <p className="text-sm text-text/80">
                {t("onboarding.refusal.needs", {
                  model: refusalCard.modelName,
                  needed: formatMemoryAmount(refusalCard.refusal.forecastBytes),
                  free: formatMemoryAmount(refusalCard.refusal.freeBytes),
                })}
              </p>
              {refusalCard.refusal.headroomBytes > 0 && (
                <p className="text-xs text-text/60">
                  {t("onboarding.refusal.marginNote", {
                    margin: formatMarginMb(refusalCard.refusal.headroomBytes),
                  })}
                </p>
              )}
              <div className="flex flex-wrap gap-2">
                {refusalActions(refusalCard.refusal, guardEnabled).map(
                  (action, index) => (
                    <button
                      key={action}
                      type="button"
                      onClick={() => handleRefusalAction(action)}
                      className={
                        index === 0
                          ? "rounded-lg bg-text text-background px-3 py-1.5 text-sm font-medium hover:opacity-90 transition-opacity"
                          : "rounded-lg border border-text/20 px-3 py-1.5 text-sm font-medium text-text hover:bg-text/10 transition-colors"
                      }
                    >
                      {t(refusalActionLabelKey[action])}
                    </button>
                  ),
                )}
              </div>
              {guardEnabled && (
                <p className="text-xs text-text/50">
                  {t("onboarding.refusal.guardOffCaveat")}
                </p>
              )}
            </div>
          )}

          {models.some((m: ModelInfo) => m.is_downloaded) && (
            <div className="space-y-3">
              <div className="text-left">
                <h2 className="text-sm font-medium text-text/60">
                  {t("onboarding.existingModelsTitle")}
                </h2>
              </div>
              {models
                .filter((m: ModelInfo) => m.is_downloaded)
                .map((model: ModelInfo) => (
                  <ModelCard
                    key={model.id}
                    model={model}
                    status={getExistingModelStatus(model.id)}
                    disabled={isBusy}
                    onSelect={handleSelectExistingModel}
                    showRecommended={false}
                  />
                ))}
            </div>
          )}

          {downloadable.length > 0 && (
            <div className="space-y-3">
              <div className="text-left">
                <h2 className="text-sm font-medium text-text/60">
                  {t("onboarding.downloadModelsTitle")}
                </h2>
              </div>

              {topPicks.map((model: ModelInfo) => (
                <ModelCard
                  key={model.id}
                  model={model}
                  variant="featured"
                  status={getModelStatus(model.id)}
                  disabled={isBusy}
                  onSelect={handleDownloadModel}
                  onDownload={handleDownloadModel}
                  onCancel={handleCancelDownload}
                  downloadProgress={getModelDownloadProgress(model.id)}
                  downloadSpeed={getModelDownloadSpeed(model.id)}
                  showRecommended={false}
                />
              ))}

              {otherRecommended.map((model: ModelInfo) => (
                <ModelCard
                  key={model.id}
                  model={model}
                  status={getModelStatus(model.id)}
                  disabled={isBusy}
                  onSelect={handleDownloadModel}
                  onDownload={handleDownloadModel}
                  onCancel={handleCancelDownload}
                  downloadProgress={getModelDownloadProgress(model.id)}
                  downloadSpeed={getModelDownloadSpeed(model.id)}
                  showRecommended={false}
                />
              ))}

              {hasRecommended && rest.length > 0 && (
                <button
                  type="button"
                  onClick={() => setShowAll((v) => !v)}
                  className="flex items-center justify-center gap-1.5 mx-auto py-1 text-sm font-medium text-text/60 hover:text-text transition-colors"
                >
                  {showAll
                    ? t("onboarding.showFewerModels")
                    : t("onboarding.showAllModels", {
                        total: downloadable.length,
                      })}
                  <ChevronDown
                    className={`w-4 h-4 transition-transform duration-200 ${
                      showAll ? "rotate-180" : ""
                    }`}
                  />
                </button>
              )}

              {showRest &&
                rest.map((model: ModelInfo) => (
                  <ModelCard
                    key={model.id}
                    model={model}
                    status={getModelStatus(model.id)}
                    disabled={isBusy}
                    onSelect={handleDownloadModel}
                    onDownload={handleDownloadModel}
                    onCancel={handleCancelDownload}
                    downloadProgress={getModelDownloadProgress(model.id)}
                    downloadSpeed={getModelDownloadSpeed(model.id)}
                    showRecommended={false}
                  />
                ))}
            </div>
          )}
        </div>
      </div>
    </div>
  );
};

export default Onboarding;
