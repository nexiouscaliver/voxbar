import React, { useEffect, useState } from "react";
import { Trans, useTranslation } from "react-i18next";
import { RefreshCcw } from "lucide-react";
import {
  commands,
  type LLMPrompt,
  type PostProcessProvider,
  type PromptTestOutcome,
  type TestPromptError,
  type TestConnectionResult,
} from "@/bindings";

import { Alert } from "../../ui/Alert";
import {
  Dropdown,
  SettingContainer,
  SettingsGroup,
  Slider,
  Textarea,
} from "@/components/ui";
import Badge from "../../ui/Badge";
import { Button } from "../../ui/Button";
import { ResetButton } from "../../ui/ResetButton";
import { Input } from "../../ui/Input";

import { ProviderSelect } from "../PostProcessingSettingsApi/ProviderSelect";
import { BaseUrlField } from "../PostProcessingSettingsApi/BaseUrlField";
import { ApiKeyField } from "../PostProcessingSettingsApi/ApiKeyField";
import { ModelSelect } from "../PostProcessingSettingsApi/ModelSelect";
import { usePostProcessProviderState } from "../PostProcessingSettingsApi/usePostProcessProviderState";
import { PostProcessModelsSection } from "./PostProcessModelsSection";
import { ShortcutInput } from "../ShortcutInput";
import { shortcutRowDisabled } from "../shortcutGating";
import { useSettings } from "../../../hooks/useSettings";

/// One line under the provider selector that renders the Test Connection
/// verdict: the ok path names auth, latency and model reachability; the
/// failure path prints the failure class token verbatim (auth, network,
/// timeout, context_length, output_invalid, oom, cancelled) followed by
/// the backend's diagnostic detail.
const ConnectionVerdictLine: React.FC<{
  result: TestConnectionResult;
  isLocalProvider: boolean;
  isAppleProvider: boolean;
}> = ({ result, isLocalProvider, isAppleProvider }) => {
  const { t } = useTranslation();
  const key = "settings.postProcessing.api.verdict";

  if (isLocalProvider) {
    return result.completion_ok === true ? (
      <p className="px-4 pb-2 text-xs text-secondary/70">
        {t(`${key}.localReady`)}
      </p>
    ) : (
      <p className="px-4 pb-2 text-xs text-error">
        {t(`${key}.localNotReady`, { detail: result.detail })}
      </p>
    );
  }

  if (isAppleProvider) {
    return result.model_list_ok ? (
      <p className="px-4 pb-2 text-xs text-secondary/70">
        {t(`${key}.appleReady`)}
      </p>
    ) : (
      <p className="px-4 pb-2 text-xs text-error">
        {t(`${key}.appleUnavailable`)}
      </p>
    );
  }

  if (result.model_list_ok && result.completion_ok === true) {
    return (
      <p className="px-4 pb-2 text-xs text-secondary/70">
        {t(`${key}.ok`, { latencyMs: result.latency_ms ?? 0 })}
      </p>
    );
  }

  if (result.model_list_ok && result.completion_ok === null) {
    return (
      <p className="px-4 pb-2 text-xs text-secondary/70">
        {t(`${key}.okNoModel`, { latencyMs: result.latency_ms ?? 0 })}
      </p>
    );
  }

  return (
    <p className="px-4 pb-2 text-xs text-error">
      {t(`${key}.failed`, {
        failureClass: result.failure_class ?? "other",
        detail: result.detail,
      })}
    </p>
  );
};

/// The per-provider request timeout row for cloud post-process calls:
/// bounds how long a wedged endpoint (one that accepts the connection but
/// never answers) can hold THIS provider's requests before the dictation
/// finishes with the raw transcript. The row edits the selected provider's
/// own value; the reset target is the provider's class default (Groq and
/// Cerebras 30s, the others 60s), shown in the description.
const PostProcessTimeoutRow: React.FC<{
  provider: PostProcessProvider;
}> = ({ provider }) => {
  const { t } = useTranslation();
  const { settings, refreshSettings } = useSettings();
  const [isPending, setIsPending] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const override = settings?.post_process_timeouts?.[provider.id];
  const classDefault = provider.default_timeout_secs ?? 60;
  const value = override ?? classDefault;

  const apply = async (seconds: number) => {
    setIsPending(true);
    setError(null);
    const result = await commands.setPostProcessTimeoutForProvider(
      provider.id,
      seconds,
    );
    if (result.status === "error") setError(result.error);
    await refreshSettings();
    setIsPending(false);
  };

  const reset = async () => {
    setIsPending(true);
    setError(null);
    await commands.resetPostProcessTimeoutForProvider(provider.id);
    await refreshSettings();
    setIsPending(false);
  };

  return (
    <>
      <Slider
        value={value}
        onChange={(value) => apply(value)}
        onReset={override === undefined ? undefined : reset}
        isResetting={isPending}
        min={5}
        max={600}
        step={5}
        label={t("settings.postProcessing.api.timeout.title")}
        description={t("settings.postProcessing.api.timeout.description", {
          classDefault,
        })}
        descriptionMode="tooltip"
        grouped={true}
        formatValue={(v) => `${v}s`}
      />
      {error && (
        <Alert variant="error" contained>
          {error}
        </Alert>
      )}
    </>
  );
};

/// The keep-warm row for the LOCAL post-process model: how long the model
/// worker stays in memory after a polish instead of unloading immediately
/// (the default). 0 is exactly the v1.3.0 exclusive swap; anything longer
/// holds the model resident for back-to-back polishing, and a dictation
/// always evicts it first.
const PostProcessKeepWarmRow: React.FC = () => {
  const { t } = useTranslation();
  const { settings, updateSetting, isUpdating } = useSettings();

  return (
    <Slider
      value={settings?.post_process_local_keep_warm_secs ?? 0}
      onChange={(value) =>
        updateSetting("post_process_local_keep_warm_secs", value)
      }
      onReset={() => updateSetting("post_process_local_keep_warm_secs", 0)}
      isResetting={isUpdating("post_process_local_keep_warm_secs")}
      min={0}
      max={600}
      step={5}
      label={t("settings.postProcessing.local.keepWarm.title")}
      description={t("settings.postProcessing.local.keepWarm.description")}
      descriptionMode="tooltip"
      grouped={true}
      formatValue={(v) =>
        v === 0 ? t("settings.postProcessing.local.keepWarm.off") : `${v}s`
      }
    />
  );
};

const PostProcessingSettingsApiComponent: React.FC = () => {
  const { t } = useTranslation();
  const state = usePostProcessProviderState();

  return (
    <>
      <SettingContainer
        title={t("settings.postProcessing.api.provider.title")}
        description={t("settings.postProcessing.api.provider.description")}
        descriptionMode="tooltip"
        layout="horizontal"
        grouped={true}
      >
        <div className="flex items-center gap-2">
          <ProviderSelect
            options={state.providerOptions}
            value={state.selectedProviderId}
            onChange={state.handleProviderSelect}
          />
          <Button
            onClick={state.handleTestConnection}
            variant="secondary"
            size="md"
            disabled={state.isTestingConnection}
            className="shrink-0"
          >
            {state.isTestingConnection
              ? t("settings.postProcessing.api.testConnectionRunning")
              : t("settings.postProcessing.api.testConnection")}
          </Button>
        </div>
      </SettingContainer>

      {state.connectionResult && (
        <ConnectionVerdictLine
          result={state.connectionResult}
          isLocalProvider={state.isLocalProvider}
          isAppleProvider={state.isAppleProvider}
        />
      )}

      {state.isLocalProvider ? (
        // The local on-device engine: no API fields, no model dropdown,
        // just the models section (spec 7.2, generalized). The section
        // itself renders the original pinned row unchanged until the user
        // downloads or selects anything beyond the pinned model. The
        // keep-warm row follows: it governs the local worker's lifetime.
        <>
          <PostProcessModelsSection />
          <PostProcessKeepWarmRow />
        </>
      ) : state.isAppleProvider ? (
        state.appleIntelligenceUnavailable ? (
          <Alert variant="error" contained>
            {t("settings.postProcessing.api.appleIntelligence.unavailable")}
          </Alert>
        ) : null
      ) : (
        <>
          {state.selectedProvider?.id === "custom" && (
            <SettingContainer
              title={t("settings.postProcessing.api.baseUrl.title")}
              description={t("settings.postProcessing.api.baseUrl.description")}
              descriptionMode="tooltip"
              layout="horizontal"
              grouped={true}
            >
              <div className="flex items-center gap-2">
                <BaseUrlField
                  value={state.baseUrl}
                  onBlur={state.handleBaseUrlChange}
                  placeholder={t(
                    "settings.postProcessing.api.baseUrl.placeholder",
                  )}
                  disabled={state.isBaseUrlUpdating}
                  className="min-w-[380px]"
                />
              </div>
            </SettingContainer>
          )}

          <SettingContainer
            title={t("settings.postProcessing.api.apiKey.title")}
            description={t("settings.postProcessing.api.apiKey.description")}
            descriptionMode="tooltip"
            layout="horizontal"
            grouped={true}
          >
            <div className="flex items-center gap-2">
              <ApiKeyField
                value={state.apiKey}
                onBlur={state.handleApiKeyChange}
                placeholder={t(
                  "settings.postProcessing.api.apiKey.placeholder",
                )}
                disabled={state.isApiKeyUpdating}
                className="min-w-[320px]"
              />
            </div>
          </SettingContainer>
        </>
      )}

      {!state.isAppleProvider && !state.isLocalProvider && (
        <SettingContainer
          title={t("settings.postProcessing.api.model.title")}
          description={
            state.isCustomProvider
              ? t("settings.postProcessing.api.model.descriptionCustom")
              : t("settings.postProcessing.api.model.descriptionDefault")
          }
          descriptionMode="tooltip"
          layout="stacked"
          grouped={true}
        >
          <div className="space-y-2">
            <div className="flex items-center gap-2">
              <ModelSelect
                value={state.model}
                options={state.modelOptions}
                disabled={state.isModelUpdating}
                isLoading={state.isFetchingModels}
                placeholder={
                  state.modelOptions.length > 0
                    ? t(
                        "settings.postProcessing.api.model.placeholderWithOptions",
                      )
                    : t(
                        "settings.postProcessing.api.model.placeholderNoOptions",
                      )
                }
                onSelect={state.handleModelSelect}
                onCreate={state.handleModelCreate}
                onBlur={() => {}}
                className="flex-1 min-w-[380px]"
              />
              <ResetButton
                onClick={state.handleRefreshModels}
                disabled={state.isFetchingModels}
                ariaLabel={t("settings.postProcessing.api.model.refreshModels")}
                className="flex h-10 w-10 items-center justify-center"
              >
                <RefreshCcw
                  className={`h-4 w-4 ${state.isFetchingModels ? "animate-spin" : ""}`}
                />
              </ResetButton>
            </div>

            {state.modelFetchError && (
              <Alert variant="error" contained>
                <div className="flex flex-1 flex-wrap items-center justify-between gap-3">
                  <span className="text-sm">
                    {t("settings.postProcessing.api.model.fetchFailed", {
                      failureClass: state.modelFetchError.kind,
                      detail: state.modelFetchError.detail,
                    })}
                  </span>
                  <Button
                    onClick={state.handleRefreshModels}
                    variant="secondary"
                    size="sm"
                    disabled={state.isFetchingModels}
                    className="shrink-0"
                  >
                    {t("settings.postProcessing.api.model.retry")}
                  </Button>
                </div>
              </Alert>
            )}

            {!state.modelFetchError &&
              state.modelListFetchedAtUnix !== null &&
              state.modelOptions.length > 0 && (
                <p className="text-xs text-secondary/70">
                  {t("settings.postProcessing.api.model.fetchedAtHint", {
                    time: new Date(
                      state.modelListFetchedAtUnix * 1000,
                    ).toLocaleString(),
                  })}
                </p>
              )}
          </div>
        </SettingContainer>
      )}

      {!state.isAppleProvider &&
        !state.isLocalProvider &&
        state.selectedProvider && (
          <PostProcessTimeoutRow provider={state.selectedProvider} />
        )}
    </>
  );
};

/// One row of the template library list: name, language/register badges,
/// the catalog description, and the row actions (select happens through the
/// dropdown above; Edit focuses this template in the editor below).
const TemplateRow: React.FC<{
  prompt: LLMPrompt;
  isSelected: boolean;
  isBusy: boolean;
  onSelect: () => void;
  onEdit: () => void;
  onDuplicate: () => void;
  onTest: () => void;
  onDelete: () => void;
  canDelete: boolean;
}> = ({
  prompt,
  isSelected,
  isBusy,
  onSelect,
  onEdit,
  onDuplicate,
  onTest,
  onDelete,
  canDelete,
}) => {
  const { t } = useTranslation();
  const base = "settings.postProcessing.prompts";
  const displayName = prompt.is_builtin
    ? t(`${base}.templates.${prompt.id}.name`, { defaultValue: prompt.name })
    : prompt.name;
  const description = prompt.is_builtin
    ? t(`${base}.templates.${prompt.id}.description`, {
        defaultValue: prompt.description,
      })
    : prompt.description;

  return (
    <div
      className={`p-3 rounded-md border ${
        isSelected
          ? "border-logo-primary bg-logo-primary/5"
          : "border-mid-gray/20"
      }`}
    >
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-center gap-1.5">
            <span className="text-sm font-medium truncate">{displayName}</span>
            <Badge variant="secondary">{prompt.language}</Badge>
            <Badge variant="secondary">
              {t(`${base}.registers.${prompt.register ?? "general"}`, {
                defaultValue: prompt.register ?? "general",
              })}
            </Badge>
            <Badge variant={prompt.is_builtin ? "primary" : "success"}>
              {prompt.is_builtin
                ? t(`${base}.builtinBadge`)
                : t(`${base}.customBadge`)}
            </Badge>
            {(prompt.version ?? 0) > 1 && (
              <Badge variant="secondary">{t(`${base}.editedBadge`)}</Badge>
            )}
          </div>
          {description && (
            <p className="mt-1 text-xs text-secondary/80 leading-snug">
              {description}
            </p>
          )}
        </div>
        <div className="flex shrink-0 items-center gap-1.5">
          <Button
            onClick={onSelect}
            variant="secondary"
            size="sm"
            disabled={isSelected || isBusy}
          >
            {t(`${base}.selectPrompt`)}
          </Button>
          <Button onClick={onEdit} variant="secondary" size="sm">
            {t(`${base}.editPrompt`)}
          </Button>
          <Button
            onClick={onDuplicate}
            variant="secondary"
            size="sm"
            disabled={isBusy}
          >
            {t(`${base}.duplicate`)}
          </Button>
          <Button
            onClick={onTest}
            variant="secondary"
            size="sm"
            disabled={isBusy}
          >
            {t(`${base}.testOnLastTranscript`)}
          </Button>
          <Button
            onClick={onDelete}
            variant="secondary"
            size="sm"
            disabled={!canDelete || isBusy}
          >
            {t(`${base}.deletePrompt`)}
          </Button>
        </div>
      </div>
    </div>
  );
};

/// The before/after panel the Test button renders: the run's outcome token
/// and latency, the transcript as it was, what it would become, and the
/// raw-kept note when the engine failed or skipped.
const PromptTestResultPanel: React.FC<{ result: PromptTestOutcome }> = ({
  result,
}) => {
  const { t } = useTranslation();
  const base = "settings.postProcessing.prompts";
  return (
    <div className="p-3 rounded-md border border-mid-gray/20 space-y-2">
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-sm font-medium">
          {t(`${base}.testResultTitle`)}
        </span>
        <Badge variant="secondary">
          {t(`${base}.testOutcome`, { outcome: result.outcome })}
        </Badge>
        <Badge variant="secondary">
          {t(`${base}.testLatency`, { ms: result.latency_ms })}
        </Badge>
      </div>
      <div className="grid gap-2 md:grid-cols-2">
        <div>
          <p className="text-xs font-medium text-secondary">
            {t(`${base}.testBefore`)}
          </p>
          <p className="mt-1 text-sm whitespace-pre-wrap break-words">
            {result.before}
          </p>
        </div>
        <div>
          <p className="text-xs font-medium text-secondary">
            {t(`${base}.testAfter`)}
          </p>
          <p className="mt-1 text-sm whitespace-pre-wrap break-words">
            {result.after ?? t(`${base}.testRawKept`)}
          </p>
        </div>
      </div>
    </div>
  );
};

const PostProcessingSettingsPromptsComponent: React.FC = () => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, isUpdating, refreshSettings } =
    useSettings();
  const [isCreating, setIsCreating] = useState(false);
  const [draftName, setDraftName] = useState("");
  const [draftText, setDraftText] = useState("");
  const [testingId, setTestingId] = useState<string | null>(null);
  const [testResult, setTestResult] = useState<PromptTestOutcome | null>(null);
  const [testError, setTestError] = useState<string | null>(null);

  const prompts = getSetting("post_process_prompts") || [];
  const selectedPromptId = getSetting("post_process_selected_prompt_id") || "";
  const selectedPrompt =
    prompts.find((prompt) => prompt.id === selectedPromptId) || null;

  useEffect(() => {
    if (isCreating) return;

    if (selectedPrompt) {
      setDraftName(selectedPrompt.name);
      setDraftText(selectedPrompt.prompt);
    } else {
      setDraftName("");
      setDraftText("");
    }
  }, [
    isCreating,
    selectedPromptId,
    selectedPrompt?.name,
    selectedPrompt?.prompt,
  ]);

  const handlePromptSelect = (promptId: string | null) => {
    if (!promptId) return;
    updateSetting("post_process_selected_prompt_id", promptId);
    setIsCreating(false);
  };

  const handleCreatePrompt = async () => {
    if (!draftName.trim() || !draftText.trim()) return;

    try {
      const result = await commands.addPostProcessPrompt(
        draftName.trim(),
        draftText.trim(),
      );
      if (result.status === "ok") {
        await refreshSettings();
        updateSetting("post_process_selected_prompt_id", result.data.id);
        setIsCreating(false);
      }
    } catch (error) {
      console.error("Failed to create prompt:", error);
    }
  };

  const handleUpdatePrompt = async () => {
    if (!selectedPromptId || !draftName.trim() || !draftText.trim()) return;

    try {
      await commands.updatePostProcessPrompt(
        selectedPromptId,
        draftName.trim(),
        draftText.trim(),
      );
      await refreshSettings();
    } catch (error) {
      console.error("Failed to update prompt:", error);
    }
  };

  const handleDeletePrompt = async (promptId: string) => {
    if (!promptId) return;

    try {
      await commands.deletePostProcessPrompt(promptId);
      await refreshSettings();
      setIsCreating(false);
    } catch (error) {
      console.error("Failed to delete prompt:", error);
    }
  };

  const handleDuplicatePrompt = async (promptId: string) => {
    try {
      const result = await commands.duplicatePostProcessPrompt(promptId);
      if (result.status === "ok") {
        await refreshSettings();
        // The copy becomes the selection so the editor opens on it.
        updateSetting("post_process_selected_prompt_id", result.data.id);
      }
    } catch (error) {
      console.error("Failed to duplicate prompt:", error);
    }
  };

  const handleTestPrompt = async (promptId: string) => {
    setTestingId(promptId);
    setTestResult(null);
    setTestError(null);
    const base = "settings.postProcessing.prompts";
    try {
      const result = await commands.testPostProcessPrompt(promptId);
      if (result.status === "ok") {
        setTestResult(result.data);
      } else {
        const error: TestPromptError = result.error;
        setTestError(
          error.kind === "no_history"
            ? t(`${base}.testNoHistory`)
            : error.kind === "prompt_not_found"
              ? t(`${base}.testPromptNotFound`)
              : t(`${base}.testFailed`, { detail: error.detail }),
        );
      }
    } catch (error) {
      console.error("Failed to test prompt:", error);
      setTestError(String(error));
    } finally {
      setTestingId(null);
    }
  };

  const handleCancelCreate = () => {
    setIsCreating(false);
    if (selectedPrompt) {
      setDraftName(selectedPrompt.name);
      setDraftText(selectedPrompt.prompt);
    } else {
      setDraftName("");
      setDraftText("");
    }
  };

  const handleStartCreate = () => {
    setIsCreating(true);
    setDraftName("");
    setDraftText("");
  };

  const hasPrompts = prompts.length > 0;
  const isDirty =
    !!selectedPrompt &&
    (draftName.trim() !== selectedPrompt.name ||
      draftText.trim() !== selectedPrompt.prompt.trim());
  const base = "settings.postProcessing.prompts";

  return (
    <SettingContainer
      title={t("settings.postProcessing.prompts.selectedPrompt.title")}
      description={t(
        "settings.postProcessing.prompts.selectedPrompt.description",
      )}
      descriptionMode="tooltip"
      layout="stacked"
      grouped={true}
    >
      <div className="space-y-3">
        <div className="flex gap-2 min-w-0">
          <Dropdown
            selectedValue={selectedPromptId || null}
            options={prompts.map((p) => ({
              value: p.id,
              label: p.is_builtin
                ? t(`${base}.templates.${p.id}.name`, { defaultValue: p.name })
                : p.name,
              // The dropdown's per-entry badge line: language + register.
              description: `${p.language ?? "auto"} · ${t(
                `${base}.registers.${p.register ?? "general"}`,
                { defaultValue: p.register ?? "general" },
              )}`,
            }))}
            onSelect={(value) => handlePromptSelect(value)}
            placeholder={
              prompts.length === 0
                ? t("settings.postProcessing.prompts.noPrompts")
                : t("settings.postProcessing.prompts.selectPrompt")
            }
            disabled={
              isUpdating("post_process_selected_prompt_id") || isCreating
            }
            className="flex-1 min-w-0"
          />
          <Button
            onClick={handleStartCreate}
            variant="primary"
            size="md"
            disabled={isCreating}
            className="shrink-0"
          >
            {t("settings.postProcessing.prompts.createNew")}
          </Button>
        </div>

        {!isCreating && hasPrompts && (
          <div className="space-y-2">
            {prompts.map((prompt) => (
              <TemplateRow
                key={prompt.id}
                prompt={prompt}
                isSelected={prompt.id === selectedPromptId}
                isBusy={testingId === prompt.id}
                onSelect={() =>
                  handlePromptSelect(
                    prompt.id === selectedPromptId ? null : prompt.id,
                  )
                }
                onEdit={() => handlePromptSelect(prompt.id)}
                onDuplicate={() => handleDuplicatePrompt(prompt.id)}
                onTest={() => handleTestPrompt(prompt.id)}
                onDelete={() => handleDeletePrompt(prompt.id)}
                canDelete={prompts.length > 1}
              />
            ))}
          </div>
        )}

        {testingId && (
          <p className="text-xs text-secondary/70">{t(`${base}.testing`)}</p>
        )}
        {testError && (
          <Alert variant="error" contained>
            {testError}
          </Alert>
        )}
        {testResult && <PromptTestResultPanel result={testResult} />}

        {!isCreating && hasPrompts && selectedPrompt && (
          <div className="space-y-3">
            <div className="space-y-2 flex flex-col">
              <label className="text-sm font-medium">
                {t("settings.postProcessing.prompts.promptLabel")}
              </label>
              <Input
                type="text"
                value={draftName}
                onChange={(e) => setDraftName(e.target.value)}
                placeholder={t(
                  "settings.postProcessing.prompts.promptLabelPlaceholder",
                )}
                variant="compact"
              />
            </div>

            <div className="space-y-2 flex flex-col">
              <label className="text-sm font-medium">
                {t("settings.postProcessing.prompts.promptInstructions")}
              </label>
              <Textarea
                value={draftText}
                onChange={(e) => setDraftText(e.target.value)}
                placeholder={t(
                  "settings.postProcessing.prompts.promptInstructionsPlaceholder",
                )}
              />
              <p className="text-xs text-secondary/70">
                <Trans
                  i18nKey="settings.postProcessing.prompts.promptTip"
                  components={{ code: <code /> }}
                />
              </p>
            </div>

            <div className="flex gap-2 pt-2">
              <Button
                onClick={handleUpdatePrompt}
                variant="primary"
                size="md"
                disabled={!draftName.trim() || !draftText.trim() || !isDirty}
              >
                {t("settings.postProcessing.prompts.updatePrompt")}
              </Button>
              <Button
                onClick={() => handleDeletePrompt(selectedPromptId)}
                variant="secondary"
                size="md"
                disabled={!selectedPromptId || prompts.length <= 1}
              >
                {t("settings.postProcessing.prompts.deletePrompt")}
              </Button>
            </div>
          </div>
        )}

        {!isCreating && !selectedPrompt && (
          <div className="p-3 bg-mid-gray/5 rounded-md border border-mid-gray/20">
            <p className="text-sm text-secondary">
              {hasPrompts
                ? t("settings.postProcessing.prompts.selectToEdit")
                : t("settings.postProcessing.prompts.createFirst")}
            </p>
          </div>
        )}

        {isCreating && (
          <div className="space-y-3">
            <div className="space-y-2 block flex flex-col">
              <label className="text-sm font-medium text-text">
                {t("settings.postProcessing.prompts.promptLabel")}
              </label>
              <Input
                type="text"
                value={draftName}
                onChange={(e) => setDraftName(e.target.value)}
                placeholder={t(
                  "settings.postProcessing.prompts.promptLabelPlaceholder",
                )}
                variant="compact"
              />
            </div>

            <div className="space-y-2 flex flex-col">
              <label className="text-sm font-medium">
                {t("settings.postProcessing.prompts.promptInstructions")}
              </label>
              <Textarea
                value={draftText}
                onChange={(e) => setDraftText(e.target.value)}
                placeholder={t(
                  "settings.postProcessing.prompts.promptInstructionsPlaceholder",
                )}
              />
              <p className="text-xs text-secondary/70">
                <Trans
                  i18nKey="settings.postProcessing.prompts.promptTip"
                  components={{ code: <code /> }}
                />
              </p>
            </div>

            <div className="flex gap-2 pt-2">
              <Button
                onClick={handleCreatePrompt}
                variant="primary"
                size="md"
                disabled={!draftName.trim() || !draftText.trim()}
              >
                {t("settings.postProcessing.prompts.createPrompt")}
              </Button>
              <Button
                onClick={handleCancelCreate}
                variant="secondary"
                size="md"
              >
                {t("settings.postProcessing.prompts.cancel")}
              </Button>
            </div>
          </div>
        )}
      </div>
    </SettingContainer>
  );
};

export const PostProcessingSettingsApi = React.memo(
  PostProcessingSettingsApiComponent,
);
PostProcessingSettingsApi.displayName = "PostProcessingSettingsApi";

export const PostProcessingSettingsPrompts = React.memo(
  PostProcessingSettingsPromptsComponent,
);
PostProcessingSettingsPrompts.displayName = "PostProcessingSettingsPrompts";

export const PostProcessingSettings: React.FC = () => {
  const { t } = useTranslation();
  const { settings } = useSettings();

  return (
    <div className="max-w-3xl w-full mx-auto space-y-6">
      <SettingsGroup title={t("settings.postProcessing.hotkey.title")}>
        {/* Both rows are inert while the master toggle is off (the backend
            gate refuses to register either key), so they grey out to match
            and their recorders cannot arm (KB-009). Unbound by default. */}
        <ShortcutInput
          shortcutId="transcribe_with_post_process"
          descriptionMode="tooltip"
          grouped={true}
          disabled={shortcutRowDisabled(
            "transcribe_with_post_process",
            settings,
          )}
        />
        <ShortcutInput
          shortcutId="cycle_post_process_prompt"
          descriptionMode="tooltip"
          grouped={true}
          disabled={shortcutRowDisabled("cycle_post_process_prompt", settings)}
        />
      </SettingsGroup>

      <SettingsGroup title={t("settings.postProcessing.api.title")}>
        <PostProcessingSettingsApi />
      </SettingsGroup>

      <SettingsGroup title={t("settings.postProcessing.prompts.title")}>
        <PostProcessingSettingsPrompts />
      </SettingsGroup>
    </div>
  );
};
