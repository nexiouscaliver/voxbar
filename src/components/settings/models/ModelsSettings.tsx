import React, { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { ask } from "@tauri-apps/plugin-dialog";
import {
  AudioLines,
  ChevronDown,
  Globe,
  Languages,
  Search,
} from "lucide-react";
import type { ModelCardStatus } from "@/components/onboarding";
import { ModelCard } from "@/components/onboarding";
import { useModelStore } from "@/stores/modelStore";
import {
  getLanguageLabel,
  MODEL_CAPABILITY_LANGUAGES,
  supportsLanguageCode,
} from "@/lib/constants/languages.ts";
import type { ModelInfo } from "@/bindings";
import { AddModelFromHuggingFace } from "./AddModelFromHuggingFace";
import { isSessionRefusalError } from "./modelRefusals";

// check if model supports a language based on its supported_languages list
const modelSupportsLanguage = (model: ModelInfo, langCode: string): boolean => {
  return supportsLanguageCode(model.supported_languages, langCode);
};

// Legacy models are the blob (Url-sourced) .bin/ONNX downloads, superseded by
// the catalog GGUFs. They stay runnable when already on disk, but we no
// longer advertise the download.
const isLegacyModel = (model: ModelInfo): boolean =>
  typeof model.source === "object" && "Url" in model.source;

// Stable prefix of the selection-refusal error string
// (SELECTION_REFUSAL_PREFIX in src-tauri/src/commands/models.rs). Must stay
// in sync: the row matches it to word the "this is the post-process engine,
// not a transcription model" explanation instead of the raw refusal.
const SELECTION_REFUSAL_PREFIX = "not-an-asr-model";

const isSelectionRefusalError = (error: string): boolean =>
  error.startsWith(SELECTION_REFUSAL_PREFIX);

export const ModelsSettings: React.FC = () => {
  const { t } = useTranslation();
  const [switchingModelId, setSwitchingModelId] = useState<string | null>(null);
  // A row-level failure the backend refused or failed to perform (a
  // selection refusal, a failed delete, a failed download cancel), pinned
  // to the row that triggered it so the error reads in place ("this model
  // cannot transcribe", "could not delete: disk error") instead of
  // vanishing into the console (KB-155).
  const [selectionError, setSelectionError] = useState<{
    modelId: string;
    message: string;
  } | null>(null);
  const [searchQuery, setSearchQuery] = useState("");
  const [filterStreaming, setFilterStreaming] = useState(false);
  const [filterTranslation, setFilterTranslation] = useState(false);
  const [languageFilter, setLanguageFilter] = useState("all");
  const [languageDropdownOpen, setLanguageDropdownOpen] = useState(false);
  const [languageSearch, setLanguageSearch] = useState("");
  const languageDropdownRef = useRef<HTMLDivElement>(null);
  const languageSearchInputRef = useRef<HTMLInputElement>(null);
  const {
    models,
    currentModel,
    downloadingModels,
    downloadProgress,
    downloadStats,
    verifyingModels,
    extractingModels,
    loading,
    downloadModel,
    cancelDownload,
    selectModel,
    deleteModel,
  } = useModelStore();

  // click outside handler for language dropdown
  useEffect(() => {
    const handleClickOutside = (event: MouseEvent) => {
      if (
        languageDropdownRef.current &&
        !languageDropdownRef.current.contains(event.target as Node)
      ) {
        setLanguageDropdownOpen(false);
        setLanguageSearch("");
      }
    };
    document.addEventListener("mousedown", handleClickOutside);
    return () => document.removeEventListener("mousedown", handleClickOutside);
  }, []);

  // focus search input when dropdown opens
  useEffect(() => {
    if (languageDropdownOpen && languageSearchInputRef.current) {
      languageSearchInputRef.current.focus();
    }
  }, [languageDropdownOpen]);

  // filtered languages for dropdown (exclude "auto")
  const filteredLanguages = useMemo(() => {
    return MODEL_CAPABILITY_LANGUAGES.filter((lang) =>
      lang.label.toLowerCase().includes(languageSearch.toLowerCase()),
    );
  }, [languageSearch]);

  // Get selected language label
  const selectedLanguageLabel = useMemo(() => {
    if (languageFilter === "all") {
      return t("settings.models.filters.allLanguages");
    }
    return getLanguageLabel(languageFilter) || "";
  }, [languageFilter, t]);

  const getModelStatus = (modelId: string): ModelCardStatus => {
    if (modelId in extractingModels) {
      return "extracting";
    }
    if (modelId in verifyingModels) {
      return "verifying";
    }
    if (modelId in downloadingModels) {
      return "downloading";
    }
    if (switchingModelId === modelId) {
      return "switching";
    }
    const model = models.find((m: ModelInfo) => m.id === modelId);
    // A stale persisted selection must never make a missing model look Active.
    // Catalog models without files should offer their recovery action instead.
    if (!model?.is_downloaded) {
      return "downloadable";
    }
    if (modelId === currentModel) {
      return "active";
    }
    return "available";
  };

  const getDownloadProgress = (modelId: string): number | undefined => {
    const progress = downloadProgress[modelId];
    return progress?.percentage;
  };

  const getDownloadSpeed = (modelId: string): number | undefined => {
    const stats = downloadStats[modelId];
    return stats?.speed;
  };

  const handleModelSelect = async (modelId: string) => {
    setSwitchingModelId(modelId);
    setSelectionError(null);
    try {
      const switched = await selectModel(modelId);
      if (!switched) {
        const { error } = useModelStore.getState();
        const raw = typeof error === "string" ? error : "";
        const message = isSelectionRefusalError(raw)
          ? t("settings.models.selectionErrors.notAsrModel")
          : t("settings.models.selectionErrors.generic", {
              error: raw.replace(/^Failed to switch to model:\s*/, ""),
            });
        setSelectionError({ modelId, message });
      }
    } finally {
      setSwitchingModelId(null);
    }
  };

  const handleModelDownload = async (modelId: string) => {
    await downloadModel(modelId);
  };

  const handleModelDelete = async (modelId: string) => {
    const model = models.find((m: ModelInfo) => m.id === modelId);
    const modelName = model?.name || modelId;
    const isActive = modelId === currentModel;

    const confirmed = await ask(
      isActive
        ? t("settings.models.deleteActiveConfirm", { modelName })
        : t("settings.models.deleteConfirm", { modelName }),
      {
        title: t("settings.models.deleteTitle"),
        kind: "warning",
      },
    );

    if (confirmed) {
      try {
        const deleted = await deleteModel(modelId);
        if (!deleted) {
          // The store never throws the backend error; it lands in state
          // prefixed "Failed to delete model: " (mirrors the select path).
          const { error } = useModelStore.getState();
          const raw =
            typeof error === "string"
              ? error.replace(/^Failed to delete model:\s*/, "")
              : "";
          if (isSessionRefusalError(raw)) {
            // KB-117/KB-220: a live dictation is using this model; the
            // refusal is transient, so surface it in place as a retry hint
            // instead of letting it vanish into the console.
            setSelectionError({
              modelId,
              message: t("settings.models.deleteRefusedLive"),
            });
          } else {
            // KB-155: every other delete failure (fs error, swap-guard
            // refusal, missing file) must reach the row alert too - a
            // console-only failure is invisible after the confirm dialog.
            setSelectionError({
              modelId,
              message: t("settings.models.deleteFailed", { error: raw || String(error ?? "") }),
            });
          }
        }
      } catch (err) {
        // KB-155: transport failures ride the same row alert.
        console.error(`Failed to delete model ${modelId}:`, err);
        setSelectionError({
          modelId,
          message: t("settings.models.deleteFailed", { error: String(err) }),
        });
      }
    }
  };

  const handleModelCancel = async (modelId: string) => {
    try {
      // KB-155: the store records a cancel failure in state (prefixed
      // "Failed to cancel download: ") and returns false instead of
      // throwing; ignoring the result left the row looking fine while the
      // download kept running.
      const cancelled = await cancelDownload(modelId);
      if (!cancelled) {
        const { error } = useModelStore.getState();
        const raw =
          typeof error === "string"
            ? error.replace(/^Failed to cancel download:\s*/, "")
            : "";
        setSelectionError({
          modelId,
          message: t("settings.models.cancelFailed", {
            error: raw || String(error ?? ""),
          }),
        });
      }
    } catch (err) {
      console.error(`Failed to cancel download for ${modelId}:`, err);
      setSelectionError({
        modelId,
        message: t("settings.models.cancelFailed", { error: String(err) }),
      });
    }
  };

  // Filter models by search query (name + description), language filter, and toggles
  const filteredModels = useMemo(() => {
    const q = searchQuery.trim().toLowerCase();
    return models.filter((model: ModelInfo) => {
      // Hide deprecated legacy (.bin/ONNX) downloads unless already on disk.
      if (isLegacyModel(model) && !model.is_downloaded) return false;
      if (languageFilter !== "all") {
        if (!modelSupportsLanguage(model, languageFilter)) return false;
      }
      if (filterStreaming && !model.supports_streaming) return false;
      if (filterTranslation && !model.supports_translation) return false;

      if (q) {
        const haystack = `${model.name} ${model.description}`.toLowerCase();
        if (!haystack.includes(q)) return false;
      }
      return true;
    });
  }, [models, languageFilter, filterStreaming, filterTranslation, searchQuery]);

  // Split filtered models into downloaded (including custom) and available sections
  const { downloadedModels, availableModels } = useMemo(() => {
    const downloaded: ModelInfo[] = [];
    const available: ModelInfo[] = [];

    for (const model of filteredModels) {
      if (
        model.is_custom ||
        model.is_downloaded ||
        model.id in downloadingModels ||
        model.id in extractingModels
      ) {
        downloaded.push(model);
      } else {
        available.push(model);
      }
    }

    // Sort: active model first, then non-custom, then custom at the bottom
    downloaded.sort((a, b) => {
      if (a.id === currentModel) return -1;
      if (b.id === currentModel) return 1;
      if (a.is_custom !== b.is_custom) return a.is_custom ? 1 : -1;
      return 0;
    });

    return {
      downloadedModels: downloaded,
      availableModels: available,
    };
  }, [filteredModels, downloadingModels, extractingModels, currentModel]);

  if (loading) {
    return (
      <div className="max-w-3xl w-full mx-auto">
        <div className="flex items-center justify-center py-16">
          <div className="w-8 h-8 border-2 border-logo-primary border-t-transparent rounded-full animate-spin" />
        </div>
      </div>
    );
  }

  return (
    <div className="max-w-3xl w-full mx-auto space-y-4">
      <div className="mb-4">
        <h1 className="text-xl font-medium mb-2">
          {t("settings.models.title")}
        </h1>
        <p className="text-sm text-text/60">
          {t("settings.models.description")}
        </p>
      </div>

      {/* Add a model by pasting a Hugging Face repo, plus a prominent rescan */}
      <AddModelFromHuggingFace />

      <div className="space-y-6">
        {/* Downloaded Models Section - header always visible so filter stays accessible */}
        <div className="space-y-3">
          {/* The heading keeps its single line (whitespace-nowrap) and the
              filter cluster wraps to its own row beneath it when the
              labeled pills plus the language dropdown outgrow the ~450px
              settings column - the cluster gives way, never the heading. */}
          <div className="flex flex-wrap items-center justify-between gap-x-2 gap-y-1.5">
            <h2 className="text-sm font-medium text-text/60 whitespace-nowrap">
              {t("settings.models.yourModels")}
            </h2>
            <div className="flex flex-wrap items-center gap-1.5">
              {/* Capability filters carry their label inline (icon-only
                  pills read as mystery toggles, and the translate glyph
                  crowded the language dropdown beside it). Rescan lives
                  once, labeled, in the Add-model card above. */}
              <button
                type="button"
                onClick={() => setFilterStreaming((enabled) => !enabled)}
                title={t("settings.models.filters.streaming")}
                aria-label={t("settings.models.filters.streaming")}
                aria-pressed={filterStreaming}
                className={`flex items-center gap-1.5 h-8 px-2 text-xs font-medium rounded-lg transition-colors cursor-pointer ${
                  filterStreaming
                    ? "bg-logo-primary/20 text-logo-primary hover:bg-logo-primary/30"
                    : "bg-mid-gray/10 text-text/60 hover:bg-mid-gray/20"
                }`}
              >
                <AudioLines className="w-3.5 h-3.5" aria-hidden="true" />
                {/* The short capability word the model cards themselves use;
                    the full sentence rides the tooltip. */}
                <span>{t("modelSelector.streaming")}</span>
              </button>
              <button
                type="button"
                onClick={() => setFilterTranslation((enabled) => !enabled)}
                title={t("settings.models.filters.translation")}
                aria-label={t("settings.models.filters.translation")}
                aria-pressed={filterTranslation}
                className={`flex items-center gap-1.5 h-8 px-2 text-xs font-medium rounded-lg transition-colors cursor-pointer ${
                  filterTranslation
                    ? "bg-logo-primary/20 text-logo-primary hover:bg-logo-primary/30"
                    : "bg-mid-gray/10 text-text/60 hover:bg-mid-gray/20"
                }`}
              >
                <Languages className="w-3.5 h-3.5" aria-hidden="true" />
                <span>{t("modelSelector.capabilities.translate")}</span>
              </button>
              {/* Language filter dropdown */}
              <div className="relative" ref={languageDropdownRef}>
                <button
                  type="button"
                  onClick={() => setLanguageDropdownOpen(!languageDropdownOpen)}
                  className={`flex items-center gap-1.5 h-8 px-2 text-xs font-medium rounded-lg transition-colors ${
                    languageFilter !== "all"
                      ? "bg-logo-primary/20 text-logo-primary"
                      : "bg-mid-gray/10 text-text/60 hover:bg-mid-gray/20"
                  }`}
                >
                  <Globe className="w-3.5 h-3.5" />
                  <span className="max-w-[120px] truncate">
                    {selectedLanguageLabel}
                  </span>
                  <ChevronDown
                    className={`w-3.5 h-3.5 transition-transform ${
                      languageDropdownOpen ? "rotate-180" : ""
                    }`}
                  />
                </button>

                {languageDropdownOpen && (
                  <div className="absolute top-full right-0 mt-1 w-56 bg-background border border-mid-gray/20 rounded-lg shadow-lg z-50 overflow-hidden">
                    <div className="p-2 border-b border-mid-gray/40">
                      <input
                        ref={languageSearchInputRef}
                        type="text"
                        value={languageSearch}
                        onChange={(e) => setLanguageSearch(e.target.value)}
                        onKeyDown={(e) => {
                          if (
                            e.key === "Enter" &&
                            filteredLanguages.length > 0
                          ) {
                            setLanguageFilter(filteredLanguages[0].value);
                            setLanguageDropdownOpen(false);
                            setLanguageSearch("");
                          } else if (e.key === "Escape") {
                            setLanguageDropdownOpen(false);
                            setLanguageSearch("");
                          }
                        }}
                        placeholder={t(
                          "settings.general.language.searchPlaceholder",
                        )}
                        className="w-full px-2 py-1 text-sm bg-mid-gray/10 border border-mid-gray/40 rounded-md focus:outline-none focus:ring-1 focus:ring-logo-primary"
                      />
                    </div>
                    <div className="max-h-48 overflow-y-auto">
                      <button
                        type="button"
                        onClick={() => {
                          setLanguageFilter("all");
                          setLanguageDropdownOpen(false);
                          setLanguageSearch("");
                        }}
                        className={`w-full px-3 py-1.5 text-sm text-left transition-colors ${
                          languageFilter === "all"
                            ? "bg-logo-primary/20 text-logo-primary font-medium"
                            : "hover:bg-mid-gray/10"
                        }`}
                      >
                        {t("settings.models.filters.allLanguages")}
                      </button>
                      {filteredLanguages.map((lang) => (
                        <button
                          key={lang.value}
                          type="button"
                          onClick={() => {
                            setLanguageFilter(lang.value);
                            setLanguageDropdownOpen(false);
                            setLanguageSearch("");
                          }}
                          className={`w-full px-3 py-1.5 text-sm text-left transition-colors ${
                            languageFilter === lang.value
                              ? "bg-logo-primary/20 text-logo-primary font-medium"
                              : "hover:bg-mid-gray/10"
                          }`}
                        >
                          {lang.label}
                        </button>
                      ))}
                      {filteredLanguages.length === 0 && (
                        <div className="px-3 py-2 text-sm text-text/50 text-center">
                          {t("settings.general.language.noResults")}
                        </div>
                      )}
                    </div>
                  </div>
                )}
              </div>
            </div>
          </div>
          {/* Search bar - filters the downloaded/available model lists by
              name or description. It lives INSIDE the models section (above
              the first card) so it reads as filtering the list, not the
              Hugging Face card that used to sit directly beneath it. */}
          <div className="relative">
            <Search className="absolute left-3 top-1/2 -translate-y-1/2 w-4 h-4 text-text/40 pointer-events-none" />
            <input
              type="text"
              value={searchQuery}
              onChange={(e) => setSearchQuery(e.target.value)}
              placeholder={t("settings.models.searchPlaceholder")}
              aria-label={t("settings.models.searchPlaceholder")}
              className="w-full pl-9 pr-3 py-2 text-sm bg-mid-gray/10 border border-mid-gray/40 rounded-lg focus:outline-none focus:ring-1 focus:ring-logo-primary placeholder:text-text/40"
            />
          </div>
          {downloadedModels.map((model: ModelInfo) => (
            <React.Fragment key={model.id}>
              <ModelCard
                model={model}
                status={getModelStatus(model.id)}
                onSelect={handleModelSelect}
                onDownload={handleModelDownload}
                onDelete={handleModelDelete}
                onCancel={handleModelCancel}
                downloadProgress={getDownloadProgress(model.id)}
                downloadSpeed={getDownloadSpeed(model.id)}
                showRecommended={false}
              />
              {selectionError?.modelId === model.id && (
                <div
                  data-testid="model-selection-error"
                  role="alert"
                  className="rounded-lg border border-error/30 bg-error/5 px-4 py-2.5 text-sm text-text/80"
                >
                  {selectionError.message}
                </div>
              )}
            </React.Fragment>
          ))}
        </div>

        {/* Available Models Section */}
        {availableModels.length > 0 && (
          <div className="space-y-3">
            <h2 className="text-sm font-medium text-text/60">
              {t("settings.models.availableModels")}
            </h2>
            {availableModels.map((model: ModelInfo) => (
              <ModelCard
                key={model.id}
                model={model}
                status={getModelStatus(model.id)}
                onSelect={handleModelSelect}
                onDownload={handleModelDownload}
                onDelete={handleModelDelete}
                onCancel={handleModelCancel}
                downloadProgress={getDownloadProgress(model.id)}
                downloadSpeed={getDownloadSpeed(model.id)}
                showRecommended={true}
              />
            ))}
          </div>
        )}
        {filteredModels.length === 0 && (
          <div className="text-center py-8 text-text/50">
            {t("settings.models.noModelsMatch")}
          </div>
        )}
      </div>
    </div>
  );
};
