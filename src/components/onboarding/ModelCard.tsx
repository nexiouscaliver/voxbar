import React from "react";
import { useTranslation } from "react-i18next";
import {
  AudioLines,
  Check,
  Download,
  Globe,
  HardDrive,
  Languages,
  Loader2,
  Trash2,
} from "lucide-react";
import type { ModelInfo } from "@/bindings";
import { formatModelSize } from "../../lib/utils/format";
import {
  getTranslatedModelDescription,
  getTranslatedModelName,
} from "../../lib/utils/modelTranslation";
import {
  getLanguageLabel,
  getUniqueCapabilityLanguages,
} from "../../lib/constants/languages";
import Badge from "../ui/Badge";
import { Button } from "../ui/Button";
import { useSettingsStore } from "@/stores/settingsStore";

// Get display text for model's language support
const getLanguageDisplayText = (
  supportedLanguages: string[],
  t: (key: string, options?: Record<string, unknown>) => string,
): string => {
  const capabilityLanguages = getUniqueCapabilityLanguages(supportedLanguages);
  if (capabilityLanguages.length === 1) {
    const langCode = capabilityLanguages[0];
    const langName = getLanguageLabel(langCode) || langCode;
    return t("modelSelector.capabilities.languageOnly", { language: langName });
  }
  return t("modelSelector.capabilities.languageCount", {
    total: capabilityLanguages.length,
  });
};

// Legacy = a blob (Url-sourced) .bin/ONNX model, kept runnable but no longer the
// advertised download (catalog GGUFs supersede it).
export const isLegacySource = (model: ModelInfo): boolean =>
  typeof model.source === "object" && "Url" in model.source;

// Extract a GGUF quantization label from a filename, if present (e.g. "Q8_0").
const getQuantLabel = (filename: string): string | null => {
  const match = filename.match(
    /[._-](IQ\d+_\w+|Q\d+(?:_\w+)?|F16|BF16|F32)\.gguf$/i,
  );
  return match ? match[1].toUpperCase() : null;
};

export type ModelCardStatus =
  | "downloadable"
  | "downloading"
  | "verifying"
  | "extracting"
  | "switching"
  | "active"
  | "available";

interface ModelCardProps {
  model: ModelInfo;
  variant?: "default" | "featured";
  status?: ModelCardStatus;
  disabled?: boolean;
  className?: string;
  onSelect: (modelId: string) => void;
  onDownload?: (modelId: string) => void;
  onDelete?: (modelId: string) => void;
  onCancel?: (modelId: string) => void;
  downloadProgress?: number;
  downloadSpeed?: number; // MB/s
  showRecommended?: boolean;
  /** Accent ring + Selected badge for the card the wizard is acting on. */
  selected?: boolean;
  /** False renders an informational card (no click, no action): used for
   * the post-process engine card, which is not a voice-model choice. */
  selectable?: boolean;
  /** Explicit per-card action pills (Download / Use). Opt-in: the shared
   * card also serves Settings > Models, which keeps its current look. */
  showActions?: boolean;
}

const ModelCard: React.FC<ModelCardProps> = ({
  model,
  variant = "default",
  status = "downloadable",
  disabled = false,
  className = "",
  onSelect,
  onDownload,
  onDelete,
  onCancel,
  downloadProgress,
  downloadSpeed,
  showRecommended = true,
  selected = false,
  selectable = true,
  showActions = false,
}) => {
  const { t } = useTranslation();
  const debugMode = useSettingsStore(
    (state) => state.settings?.debug_mode ?? false,
  );
  const isFeatured = variant === "featured";
  // The active model is already loaded - re-selecting it just reloads it for no
  // gain, so it is deliberately not clickable.
  const isClickable =
    selectable && (status === "available" || status === "downloadable");

  // Get translated model name and description
  const displayName = getTranslatedModelName(model, t);
  const displayDescription = getTranslatedModelDescription(model, t);
  const showModelSize =
    status === "downloadable" || status === "available" || status === "active";
  const formattedModelSize = formatModelSize(Number(model.size_mb));
  const quantLabel = getQuantLabel(model.filename);
  const capabilityLanguages = getUniqueCapabilityLanguages(
    model.supported_languages,
  );

  const baseClasses =
    "flex flex-col rounded-xl px-4 py-3 gap-2 text-left transition-all duration-200";

  const getVariantClasses = () => {
    if (selected) {
      return "border-2 border-logo-primary ring-2 ring-logo-primary/30 bg-logo-primary/5";
    }
    if (status === "active") {
      return "border-2 border-logo-primary/50 bg-logo-primary/10";
    }
    if (isFeatured) {
      return "border-2 border-logo-primary/25 bg-logo-primary/5";
    }
    return "border-2 border-mid-gray/20";
  };

  const getInteractiveClasses = () => {
    if (!isClickable) return "";
    // While the wizard acts on another card, the rest dim but stay legible
    // (0.7, not the old near-disabled 0.5); the card being acted on keeps
    // full opacity so the progress on it reads clearly.
    if (disabled) return selected ? "" : "opacity-70 cursor-not-allowed";
    return "cursor-pointer hover:border-logo-primary/50 hover:bg-logo-primary/5 hover:shadow-lg hover:scale-[1.01] active:scale-[0.99] group";
  };

  const handleClick = () => {
    if (!isClickable || disabled) return;
    if (status === "downloadable" && onDownload) {
      onDownload(model.id);
    } else {
      onSelect(model.id);
    }
  };

  const handleActionClick = (e: React.MouseEvent) => {
    e.stopPropagation();
    handleClick();
  };

  const handleDelete = (e: React.MouseEvent) => {
    e.stopPropagation();
    onDelete?.(model.id);
  };

  return (
    <div
      onClick={handleClick}
      onKeyDown={(e) => {
        if (e.key === "Enter" && isClickable) handleClick();
      }}
      role={isClickable ? "button" : undefined}
      tabIndex={isClickable ? 0 : undefined}
      className={[
        baseClasses,
        getVariantClasses(),
        getInteractiveClasses(),
        className,
      ]
        .filter(Boolean)
        .join(" ")}
    >
      {/* Top section: name/description + score bars */}
      <div className="flex justify-between items-center w-full">
        <div className="flex flex-col items-start flex-1 min-w-0">
          <div className="flex items-center gap-3 flex-wrap">
            <h3
              className={`text-base font-medium text-text ${isClickable ? "group-hover:text-logo-primary" : ""} transition-colors`}
            >
              {displayName}
            </h3>
            {showRecommended && model.is_recommended && (
              <Badge variant="primary">{t("onboarding.recommended")}</Badge>
            )}
            {selected && (
              <Badge variant="primary">
                <Check className="w-3 h-3 mr-1" />
                {t("onboarding.modelCard.selected")}
              </Badge>
            )}
            {!selected && showActions && status === "available" && (
              <Badge variant="secondary">
                <HardDrive className="w-3 h-3 mr-1" />
                {t("onboarding.modelCard.installed")}
              </Badge>
            )}
            {status === "active" && (
              <Badge variant="primary">
                <Check className="w-3 h-3 mr-1" />
                {t("modelSelector.active")}
              </Badge>
            )}
            {model.is_custom && (
              <Badge variant="secondary">{t("modelSelector.custom")}</Badge>
            )}
            {isLegacySource(model) && (
              <Badge variant="secondary">{t("modelSelector.legacy")}</Badge>
            )}
            {status === "switching" && (
              <Badge variant="secondary">
                <Loader2 className="w-3 h-3 mr-1 animate-spin" />
                {t("modelSelector.switching")}
              </Badge>
            )}
          </div>
          <p className="text-text/60 text-sm leading-relaxed">
            {displayDescription}
          </p>
        </div>
        {(model.accuracy_score > 0 || model.speed_score > 0) && (
          <div className="hidden sm:flex items-center ms-4">
            <div className="space-y-1">
              {(
                [
                  ["accuracy", model.accuracy_score],
                  ["speed", model.speed_score],
                ] as const
              ).map(([kind, score]) => (
                <div
                  key={kind}
                  className="flex items-center gap-2"
                  /* The scale tooltip: the bar is a 0-100 relative score,
                     not a percentage of anything measured. */
                  title={`${t(`onboarding.modelCard.${kind}`)}: ${Math.round(score * 100)} / 100`}
                >
                  <p className="text-xs text-text/60 w-24 text-end">
                    {t(`onboarding.modelCard.${kind}`)}
                  </p>
                  <div className="w-16 h-1.5 bg-mid-gray/20 rounded-full overflow-hidden">
                    <div
                      className="h-full bg-logo-primary rounded-full"
                      style={{ width: `${score * 100}%` }}
                    />
                  </div>
                  {/* The concrete value beside the bar so the meter carries
                      its number, not just a length. */}
                  <span className="w-7 text-end text-xs tabular-nums text-text/60">
                    {Math.round(score * 100)}
                  </span>
                </div>
              ))}
            </div>
          </div>
        )}
      </div>

      <hr className="w-full border-mid-gray/20" />

      {/* Bottom row: tags + action buttons (full width) */}
      <div className="flex items-center gap-3 w-full -mb-0.5 mt-0.5 min-h-5">
        {capabilityLanguages.length > 0 && (
          <div
            className="flex items-center gap-1 text-xs text-text/50"
            title={
              capabilityLanguages.length === 1
                ? t("modelSelector.capabilities.singleLanguage")
                : t("modelSelector.capabilities.languageSelection")
            }
          >
            <Globe className="w-3.5 h-3.5" />
            <span>{getLanguageDisplayText(model.supported_languages, t)}</span>
          </div>
        )}
        {model.supports_translation && (
          <div
            className="flex items-center gap-1 text-xs text-text/50"
            title={t("modelSelector.capabilities.translation")}
          >
            <Languages className="w-3.5 h-3.5" />
            <span>{t("modelSelector.capabilities.translate")}</span>
          </div>
        )}
        {model.supports_streaming && (
          <div
            className="flex items-center gap-1 text-xs text-text/50"
            title={t("modelSelector.capabilities.streaming")}
          >
            <AudioLines className="w-3.5 h-3.5" />
            <span>{t("modelSelector.streaming")}</span>
          </div>
        )}
        {/* Trailing zone: size info plus the explicit per-card action. With
            actions on (onboarding), the Download pill carries the size on
            downloadable cards; without them (Settings > Models) the plain
            size span keeps its old rendering on every card. */}
        <div className="flex items-center gap-2 ms-auto">
          {showModelSize && (status !== "downloadable" || !showActions) && (
            <span className="flex items-center gap-1.5 text-xs text-text/50">
              {status === "downloadable" ? (
                <Download className="w-3.5 h-3.5" />
              ) : (
                <HardDrive className="w-3.5 h-3.5" />
              )}
              <span>{formattedModelSize}</span>
              {debugMode && quantLabel && (
                <span className="text-text/40">{quantLabel}</span>
              )}
            </span>
          )}
          {showActions && isClickable && status === "downloadable" && (
            <Button
              variant="primary-soft"
              size="sm"
              onClick={handleActionClick}
              disabled={disabled}
              aria-label={`${t("settings.postProcessing.local.download")} ${displayName}`}
              className="flex items-center gap-1.5"
            >
              <Download className="w-3.5 h-3.5" />
              <span>
                {t("settings.postProcessing.local.download")} ·{" "}
                {formattedModelSize}
              </span>
            </Button>
          )}
          {showActions && isClickable && status === "available" && (
            <Button
              variant="secondary"
              size="sm"
              onClick={handleActionClick}
              disabled={disabled}
              aria-label={`${t("settings.postProcessing.models.select")} ${displayName}`}
            >
              {t("settings.postProcessing.models.select")}
            </Button>
          )}
          {onDelete && (status === "available" || status === "active") && (
            <>
              {/* A hairline keeps the destructive action visually separate
                  from the size/actions it trails. */}
              <div
                className="h-4 w-px bg-mid-gray/30 mx-0.5"
                aria-hidden="true"
              />
              <Button
                variant="danger-ghost"
                size="sm"
                onClick={handleDelete}
                /* The active model cannot be deleted in place: deleting the
                   model that is loaded for transcription would stop every
                   dictation. Select another model first; the confirm dialog
                   (ModelsSettings) guards every enabled delete. */
                disabled={status === "active"}
                title={t("modelSelector.deleteModel", {
                  modelName: displayName,
                })}
                className="flex items-center gap-1.5"
              >
                <Trash2 className="w-3.5 h-3.5" />
                <span>{t("common.delete")}</span>
              </Button>
            </>
          )}
        </div>
      </div>

      {/* Download/extract progress */}
      {status === "downloading" && downloadProgress !== undefined && (
        <div className="w-full mt-3">
          <div className="w-full h-1.5 bg-mid-gray/20 rounded-full overflow-hidden">
            <div
              className="h-full bg-logo-primary rounded-full transition-all duration-300"
              style={{ width: `${downloadProgress}%` }}
            />
          </div>
          <div className="flex items-center justify-between text-xs mt-1">
            <span className="text-text/50">
              {t("modelSelector.downloading", {
                percentage: Math.round(downloadProgress),
              })}
            </span>
            <div className="flex items-center gap-2">
              {downloadSpeed !== undefined && downloadSpeed > 0 && (
                <span className="tabular-nums text-text/50">
                  {t("modelSelector.downloadSpeed", {
                    speed: downloadSpeed.toFixed(1),
                  })}
                </span>
              )}
              {onCancel && (
                <Button
                  variant="danger-ghost"
                  size="sm"
                  onClick={(e) => {
                    e.preventDefault();
                    e.stopPropagation();
                    onCancel(model.id);
                  }}
                  aria-label={t("modelSelector.cancelDownload")}
                >
                  {t("modelSelector.cancel")}
                </Button>
              )}
            </div>
          </div>
        </div>
      )}
      {status === "verifying" && (
        <div className="w-full mt-3">
          <div className="w-full h-1.5 bg-mid-gray/20 rounded-full overflow-hidden">
            <div className="h-full bg-logo-primary rounded-full animate-pulse w-full" />
          </div>
          <p className="text-xs text-text/50 mt-1">
            {t("modelSelector.verifyingGeneric")}
          </p>
        </div>
      )}
      {status === "extracting" && (
        <div className="w-full mt-3">
          <div className="w-full h-1.5 bg-mid-gray/20 rounded-full overflow-hidden">
            <div className="h-full bg-logo-primary rounded-full animate-pulse w-full" />
          </div>
          <p className="text-xs text-text/50 mt-1">
            {t("modelSelector.extractingGeneric")}
          </p>
        </div>
      )}
    </div>
  );
};

export default ModelCard;
