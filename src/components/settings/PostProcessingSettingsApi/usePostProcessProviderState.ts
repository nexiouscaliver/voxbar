import { useCallback, useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { useSettings } from "../../../hooks/useSettings";
import {
  commands,
  type PostProcessProvider,
  type TestConnectionResult,
} from "@/bindings";
import { shouldFetchOnOpen } from "@/stores/postProcessModelCache";
import type { ModelOption } from "./types";
import type { DropdownOption } from "../../ui/Dropdown";
import {
  APPLE_PROVIDER_ID,
  LOCAL_PROVIDER_ID,
  shouldFetchModels,
  showLocalRow,
} from "../post-processing/localLlmRouting";

type PostProcessProviderState = {
  providerOptions: DropdownOption[];
  selectedProviderId: string;
  selectedProvider: PostProcessProvider | undefined;
  isCustomProvider: boolean;
  isAppleProvider: boolean;
  isLocalProvider: boolean;
  appleIntelligenceUnavailable: boolean;
  baseUrl: string;
  handleBaseUrlChange: (value: string) => void;
  isBaseUrlUpdating: boolean;
  apiKey: string;
  handleApiKeyChange: (value: string) => void;
  isApiKeyUpdating: boolean;
  model: string;
  handleModelChange: (value: string) => void;
  modelOptions: ModelOption[];
  isModelUpdating: boolean;
  isFetchingModels: boolean;
  // The classified error of the last model-list fetch for the selected
  // provider (null when the last fetch succeeded or none ran yet).
  modelFetchError: { kind: string; detail: string } | null;
  // Unix seconds of the cached list's last successful fetch, for the
  // fetched-at hint. Null when nothing is cached for this provider.
  modelListFetchedAtUnix: number | null;
  handleProviderSelect: (providerId: string) => void;
  handleModelSelect: (value: string) => void;
  handleModelCreate: (value: string) => void;
  handleRefreshModels: () => void;
  // Test Connection (workstream 2): the probe call and its verdict.
  isTestingConnection: boolean;
  connectionResult: TestConnectionResult | null;
  handleTestConnection: () => void;
};

export const usePostProcessProviderState = (): PostProcessProviderState => {
  const { t } = useTranslation();
  const {
    settings,
    isUpdating,
    setPostProcessProvider,
    updatePostProcessBaseUrl,
    updatePostProcessApiKey,
    updatePostProcessModel,
    fetchPostProcessModels,
    postProcessModelOptions,
    postProcessModelFetchErrors,
  } = useSettings();

  // Settings are guaranteed to have providers after migration
  const providers = settings?.post_process_providers || [];

  const selectedProviderId = useMemo(() => {
    return settings?.post_process_provider_id || providers[0]?.id || "openai";
  }, [providers, settings?.post_process_provider_id]);

  const selectedProvider = useMemo(() => {
    return (
      providers.find((provider) => provider.id === selectedProviderId) ||
      providers[0]
    );
  }, [providers, selectedProviderId]);

  const isAppleProvider = selectedProvider?.id === APPLE_PROVIDER_ID;
  const isLocalProvider = showLocalRow(selectedProviderId);
  const [appleIntelligenceUnavailable, setAppleIntelligenceUnavailable] =
    useState(false);

  // Use settings directly as single source of truth
  const baseUrl = selectedProvider?.base_url ?? "";
  const apiKey = settings?.post_process_api_keys?.[selectedProviderId] ?? "";
  const model = settings?.post_process_models?.[selectedProviderId] ?? "";

  const providerOptions = useMemo<DropdownOption[]>(() => {
    return providers.map((provider) => ({
      value: provider.id,
      // The local engine's backend label ("Local (on-device)") is the
      // only descriptive (non-brand-name) label in the list; localize it
      // so it does not ship as a lone English string in every locale.
      // Brand names (OpenAI, Groq, ...) stay as the backend sends them.
      label:
        provider.id === LOCAL_PROVIDER_ID
          ? t("settings.postProcessing.local.providerLabel")
          : provider.label,
    }));
  }, [providers, t]);

  const handleProviderSelect = useCallback(
    async (providerId: string) => {
      // Clear error state on any selection attempt (allows dismissing the error)
      setAppleIntelligenceUnavailable(false);

      if (providerId === selectedProviderId) return;

      // Check Apple Intelligence availability before selecting
      if (providerId === APPLE_PROVIDER_ID) {
        const available = await commands.checkAppleIntelligenceAvailable();
        if (!available) {
          setAppleIntelligenceUnavailable(true);
          // Don't return - still set the provider so dropdown shows the selection
          // The backend gracefully handles unavailable Apple Intelligence
        }
      }

      await setPostProcessProvider(providerId);

      // Auto-fetch available models for the new provider so the model dropdown
      // reflects what's actually valid. Without this, a stale model value from
      // a previous provider/base_url can persist and silently 404 at runtime.
      // Skip when the provider isn't configured yet (no API key / empty base URL)
      // to avoid unnecessary backend errors. On-device providers (local, Apple
      // Intelligence) have no models endpoint and must never fetch (T30).
      if (shouldFetchModels(providerId)) {
        const provider = providers.find((p) => p.id === providerId);
        const apiKey = settings?.post_process_api_keys?.[providerId] ?? "";
        const hasBaseUrl = (provider?.base_url ?? "").trim() !== "";
        const hasApiKey = apiKey.trim() !== "";

        if (provider?.id === "custom" ? hasBaseUrl : hasApiKey) {
          void fetchPostProcessModels(providerId);
        }
      }
    },
    [
      selectedProviderId,
      setPostProcessProvider,
      fetchPostProcessModels,
      providers,
      settings,
    ],
  );

  const handleBaseUrlChange = useCallback(
    (value: string) => {
      if (!selectedProvider || selectedProvider.id !== "custom") {
        return;
      }
      const trimmed = value.trim();
      if (trimmed && trimmed !== baseUrl) {
        void updatePostProcessBaseUrl(selectedProvider.id, trimmed);
      }
    },
    [selectedProvider, baseUrl, updatePostProcessBaseUrl],
  );

  const handleApiKeyChange = useCallback(
    (value: string) => {
      const trimmed = value.trim();
      if (trimmed !== apiKey) {
        void updatePostProcessApiKey(selectedProviderId, trimmed);
      }
    },
    [apiKey, selectedProviderId, updatePostProcessApiKey],
  );

  const handleModelChange = useCallback(
    (value: string) => {
      const trimmed = value.trim();
      if (trimmed !== model) {
        void updatePostProcessModel(selectedProviderId, trimmed);
      }
    },
    [model, selectedProviderId, updatePostProcessModel],
  );

  const handleModelSelect = useCallback(
    (value: string) => {
      void updatePostProcessModel(selectedProviderId, value.trim());
    },
    [selectedProviderId, updatePostProcessModel],
  );

  const handleModelCreate = useCallback(
    (value: string) => {
      void updatePostProcessModel(selectedProviderId, value);
    },
    [selectedProviderId, updatePostProcessModel],
  );

  const handleRefreshModels = useCallback(() => {
    // On-device providers (local, Apple Intelligence) have nothing to
    // fetch; the local row manages its own model status.
    if (!shouldFetchModels(selectedProviderId)) return;
    void fetchPostProcessModels(selectedProviderId);
  }, [fetchPostProcessModels, selectedProviderId]);

  // On-open refresh: a provider whose list was never fetched (this session
  // or in the persisted cache) gets fetched once when its panel opens. A
  // cache hit is a no-op, so reopening the tab shows the cached list
  // instantly without refetching; the refresh button stays the explicit
  // way to update it.
  useEffect(() => {
    if (!settings || !selectedProviderId) return;
    if (!shouldFetchModels(selectedProviderId)) return;
    if (
      !shouldFetchOnOpen(
        settings.post_process_model_lists,
        postProcessModelOptions,
        selectedProviderId,
      )
    ) {
      return;
    }
    const provider = providers.find((p) => p.id === selectedProviderId);
    const apiKey = settings.post_process_api_keys?.[selectedProviderId] ?? "";
    const hasBaseUrl = (provider?.base_url ?? "").trim() !== "";
    const hasApiKey = apiKey.trim() !== "";
    // Skip when the provider isn't configured yet (no API key / empty base
    // URL) to avoid a guaranteed-failing request.
    if (provider?.id === "custom" ? hasBaseUrl : hasApiKey) {
      void fetchPostProcessModels(selectedProviderId);
    }
  }, [
    settings,
    providers,
    selectedProviderId,
    postProcessModelOptions,
    fetchPostProcessModels,
  ]);

  // Test Connection: run the backend probe (model list + tiny completion
  // for cloud providers, downloaded state for local, availability for
  // Apple Intelligence) and hold the verdict for the settings row.
  const [connectionResult, setConnectionResult] =
    useState<TestConnectionResult | null>(null);
  const [isTestingConnection, setIsTestingConnection] = useState(false);

  const handleTestConnection = useCallback(() => {
    if (isTestingConnection) return;
    setIsTestingConnection(true);
    setConnectionResult(null);
    commands
      .testPostProcessConnection(selectedProviderId)
      .then((result) => {
        if (result.status === "ok") {
          setConnectionResult(result.data);
        } else {
          setConnectionResult({
            model_list_ok: false,
            completion_ok: null,
            latency_ms: null,
            failure_class: null,
            detail: result.error,
          });
        }
      })
      .catch((error) => {
        console.error("Failed to test post-process connection:", error);
      })
      .finally(() => {
        setIsTestingConnection(false);
      });
  }, [isTestingConnection, selectedProviderId]);

  // A provider switch invalidates the previous verdict.
  useEffect(() => {
    setConnectionResult(null);
  }, [selectedProviderId]);

  const modelFetchError =
    postProcessModelFetchErrors[selectedProviderId] ?? null;
  const modelListFetchedAtUnix =
    settings?.post_process_model_lists?.[selectedProviderId]?.fetched_at_unix ??
    null;

  const availableModelsRaw = postProcessModelOptions[selectedProviderId] || [];

  const modelOptions = useMemo<ModelOption[]>(() => {
    const seen = new Set<string>();
    const options: ModelOption[] = [];

    const upsert = (value: string | null | undefined) => {
      const trimmed = value?.trim();
      if (!trimmed || seen.has(trimmed)) return;
      seen.add(trimmed);
      options.push({ value: trimmed, label: trimmed });
    };

    // Add available models from API
    for (const candidate of availableModelsRaw) {
      upsert(candidate);
    }

    // Ensure current model is in the list
    upsert(model);

    return options;
  }, [availableModelsRaw, model]);

  const isBaseUrlUpdating = isUpdating(
    `post_process_base_url:${selectedProviderId}`,
  );
  const isApiKeyUpdating = isUpdating(
    `post_process_api_key:${selectedProviderId}`,
  );
  const isModelUpdating = isUpdating(
    `post_process_model:${selectedProviderId}`,
  );
  const isFetchingModels = isUpdating(
    `post_process_models_fetch:${selectedProviderId}`,
  );

  const isCustomProvider = selectedProvider?.id === "custom";

  // No automatic fetching - user must click refresh button

  return {
    providerOptions,
    selectedProviderId,
    selectedProvider,
    isCustomProvider,
    isAppleProvider,
    isLocalProvider,
    appleIntelligenceUnavailable,
    baseUrl,
    handleBaseUrlChange,
    isBaseUrlUpdating,
    apiKey,
    handleApiKeyChange,
    isApiKeyUpdating,
    model,
    handleModelChange,
    modelOptions,
    isModelUpdating,
    isFetchingModels,
    modelFetchError,
    modelListFetchedAtUnix,
    handleProviderSelect,
    handleModelSelect,
    handleModelCreate,
    handleRefreshModels,
    isTestingConnection,
    connectionResult,
    handleTestConnection,
  };
};
