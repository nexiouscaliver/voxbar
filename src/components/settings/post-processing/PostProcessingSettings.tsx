import React, { useEffect, useRef, useState } from "react";
import { Trans, useTranslation } from "react-i18next";
import { Check, RefreshCcw, Search } from "lucide-react";
import {
  commands,
  type LLMPrompt,
  type PostProcessProvider,
  type TestConnectionResult,
} from "@/bindings";

import { Alert } from "../../ui/Alert";
import {
  Dropdown,
  SettingContainer,
  SettingsGroup,
  Slider,
  Textarea,
  ToggleSwitch,
} from "@/components/ui";
import Badge from "../../ui/Badge";
import { Button } from "../../ui/Button";
import { Input } from "../../ui/Input";

import { ProviderSelect } from "../PostProcessingSettingsApi/ProviderSelect";
import { BaseUrlField } from "../PostProcessingSettingsApi/BaseUrlField";
import { ApiKeyField } from "../PostProcessingSettingsApi/ApiKeyField";
import { usePostProcessProviderState } from "../PostProcessingSettingsApi/usePostProcessProviderState";
import { PostProcessModelsSection } from "./PostProcessModelsSection";
import { ShortcutInput } from "../ShortcutInput";
import { shortcutRowDisabled } from "../shortcutGating";
import { openPromptStudio } from "../studio/studioEvents";
import { useSettings } from "../../../hooks/useSettings";
import { showLocalRow } from "./localLlmRouting";

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
/// (the default). The boolean reads as the app's standard pill switch (off =
/// gray track + left knob; on = accent track), and the duration is an
/// indented slider sub-row that only exists while warm-keeping is on - the
/// old always-on slider rendered the Off state as a pink knob on a dead
/// track plus a separate "Off" text, contradicting the accent-means-on
/// language every other boolean on the page uses.
const WARM_DEFAULT_SECS = 60;

const PostProcessKeepWarmRow: React.FC = () => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, isUpdating } = useSettings();

  const keepWarmSecs = getSetting("post_process_local_keep_warm_secs") ?? 0;
  const warmOn = keepWarmSecs > 0;
  const formatWarm = (v: number): string =>
    v >= 60 && v % 60 === 0 ? `${v / 60}m` : `${v}s`;

  return (
    <>
      <ToggleSwitch
        checked={warmOn}
        onChange={(enabled) =>
          updateSetting(
            "post_process_local_keep_warm_secs",
            enabled ? WARM_DEFAULT_SECS : 0,
          )
        }
        isUpdating={isUpdating("post_process_local_keep_warm_secs")}
        label={t("settings.postProcessing.local.keepWarm.title")}
        description={t("settings.postProcessing.local.keepWarm.description")}
        descriptionMode="tooltip"
        grouped={true}
      />
      {warmOn && (
        <div className="flex items-center gap-2 pl-8 pr-2 h-6">
          <input
            type="range"
            min={5}
            max={600}
            step={5}
            value={keepWarmSecs}
            onChange={(event) =>
              updateSetting(
                "post_process_local_keep_warm_secs",
                parseFloat(event.target.value),
              )
            }
            aria-label={t("settings.postProcessing.local.keepWarm.title")}
            className="flex-grow h-1.5 rounded-full appearance-none cursor-pointer focus:outline-none focus:ring-2 focus:ring-logo-primary/50 [&::-webkit-slider-thumb]:appearance-none [&::-webkit-slider-thumb]:h-3.5 [&::-webkit-slider-thumb]:w-3.5 [&::-webkit-slider-thumb]:rounded-full [&::-webkit-slider-thumb]:bg-background-ui [&::-webkit-slider-thumb]:border-2 [&::-webkit-slider-thumb]:border-background [&::-moz-range-thumb]:h-3.5 [&::-moz-range-thumb]:w-3.5 [&::-moz-range-thumb]:rounded-full [&::-moz-range-thumb]:bg-background-ui [&::-moz-range-thumb]:border-2 [&::-moz-range-thumb]:border-background"
            style={{
              background: `linear-gradient(to right, var(--color-background-ui) ${
                ((keepWarmSecs - 5) / (600 - 5)) * 100
              }%, rgba(128, 128, 128, 0.2) ${
                ((keepWarmSecs - 5) / (600 - 5)) * 100
              }%)`,
            }}
          />
          <span
            className="min-w-12 max-w-32 shrink-0 truncate text-end text-sm font-medium text-text/90"
            title={formatWarm(keepWarmSecs)}
          >
            {formatWarm(keepWarmSecs)}
          </span>
        </div>
      )}
    </>
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
        // The local on-device engine: no API fields, no model dropdown.
        // The models catalog lives in its own section below this card
        // (rendered by PostProcessingSettings at page level, so its header
        // and padding match the sibling sections instead of nesting a card
        // inside this card). The keep-warm row stays here: it governs the
        // local worker's lifetime, which is engine config.
        <PostProcessKeepWarmRow />
      ) : state.isAppleProvider ? (
        // KB-262: the tri-state probe words the two unavailable verdicts
        // differently - a build compiled without Apple Intelligence is not
        // an OS-requirements problem, so it gets its own copy instead of
        // the "requires macOS Tahoe" message. A probe that failed to
        // answer (fleet, cycle 14) gets the availability-unknown copy: a
        // transport failure established nothing about the OS.
        state.appleIntelligenceStatus === "build_lacks" ? (
          <Alert variant="error" contained>
            {t("settings.postProcessing.api.appleIntelligence.buildLacks")}
          </Alert>
        ) : state.appleIntelligenceStatus === "os_unsupported" ? (
          <Alert variant="error" contained>
            {t("settings.postProcessing.api.appleIntelligence.unavailable")}
          </Alert>
        ) : state.appleIntelligenceStatus === "probe_failed" ? (
          <Alert variant="error" contained>
            {t("settings.postProcessing.api.appleIntelligence.probeFailed")}
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
        <CloudModelPicker
          model={state.model}
          modelOptions={state.modelOptions}
          isModelUpdating={state.isModelUpdating}
          isFetchingModels={state.isFetchingModels}
          modelFetchError={state.modelFetchError}
          modelListFetchedAtUnix={state.modelListFetchedAtUnix}
          apiKeyPresent={Boolean(state.apiKey.trim())}
          description={
            state.isCustomProvider
              ? t("settings.postProcessing.api.model.descriptionCustom")
              : t("settings.postProcessing.api.model.visibleDescription")
          }
          onSelect={state.handleModelSelect}
          onCreate={state.handleModelCreate}
          onRefresh={state.handleRefreshModels}
        />
      )}

      {!state.isAppleProvider &&
        !state.isLocalProvider &&
        state.selectedProvider && (
          <PostProcessTimeoutRow provider={state.selectedProvider} />
        )}
    </>
  );
};

/// One selectable model id in the cloud model list (W1-L9 visual pass):
/// the same check-row pattern the Prompt Studio uses, so picking a model
/// reads as an explicit, visible choice instead of a value hidden inside
/// a closed combobox.
const ModelOptionRow: React.FC<{
  modelId: string;
  selected: boolean;
  disabled?: boolean;
  onSelect: () => void;
  activeLabel: string;
}> = ({ modelId, selected, disabled, onSelect, activeLabel }) => (
  <button
    type="button"
    role="radio"
    aria-checked={selected}
    onClick={onSelect}
    disabled={disabled}
    className={`flex items-center gap-2.5 px-2.5 py-1.5 rounded-md border text-left transition-colors ${
      selected
        ? "border-logo-primary bg-logo-primary/5"
        : "border-mid-gray/20 hover:bg-mid-gray/10"
    } ${disabled ? "opacity-50 cursor-not-allowed" : "cursor-pointer"}`}
  >
    <span
      className={`h-3.5 w-3.5 shrink-0 rounded-full border ${
        selected
          ? "border-logo-primary bg-logo-primary/80"
          : "border-mid-gray/50"
      }`}
      aria-hidden="true"
    />
    <span className="min-w-0 flex-1 font-mono text-xs truncate" title={modelId}>
      {modelId}
    </span>
    {selected ? (
      <Badge variant="primary" className="shrink-0 px-1.5 py-0.5">
        {activeLabel}
      </Badge>
    ) : null}
  </button>
);

/// The cloud model selection surface (W1-L9 visual pass): a visible list
/// of the models the provider's key can see (radio rows), an explicit
/// custom-id input for anything not listed, a labeled refresh action with
/// the fetched-at hint, and honest empty/loading states. Replaces the
/// bare creatable combobox whose only affordance was a closed box
/// with an unlabeled icon button next to it. Review round: provider
/// catalogs can run to dozens of ids (OpenAI /v1/models also returns
/// embedding/tts/whisper models), so the grid carries a type-to-filter
/// input (the affordance the deleted combobox had) and renders a capped
/// first page with a Show-all toggle; the currently selected model is
/// always kept visible, filter or no filter.
const MAX_MODEL_ROWS = 8;

const CloudModelPicker: React.FC<{
  model: string;
  modelOptions: { value: string; label: string }[];
  isModelUpdating: boolean;
  isFetchingModels: boolean;
  modelFetchError: { kind: string; detail: string } | null;
  modelListFetchedAtUnix: number | null;
  /** Whether the selected provider has an API key stored: without one a
   * model-list fetch cannot succeed, so the refresh action rests disabled
   * and the empty state names the key as the blocker instead of a dead-end
   * "no list yet" next to a button that can never work. */
  apiKeyPresent: boolean;
  /** The row description: provider-class aware (custom endpoints need the
   * "identifier your endpoint expects" wording). */
  description: string;
  onSelect: (value: string) => void;
  onCreate: (value: string) => void;
  onRefresh: () => void;
}> = ({
  model,
  modelOptions,
  isModelUpdating,
  isFetchingModels,
  modelFetchError,
  modelListFetchedAtUnix,
  apiKeyPresent,
  description,
  onSelect,
  onCreate,
  onRefresh,
}) => {
  const { t } = useTranslation();
  const [customId, setCustomId] = useState("");
  const [filter, setFilter] = useState("");
  const [showAll, setShowAll] = useState(false);
  const base = "settings.postProcessing.api.model";

  const applyCustomId = () => {
    const trimmed = customId.trim();
    if (!trimmed) return;
    onCreate(trimmed);
    setCustomId("");
  };

  const query = filter.trim().toLowerCase();
  // The selected model stays pinned at the top of the filtered view so the
  // Active row never disappears under the user's own filter.
  const matching = modelOptions.filter((option) =>
    option.value.toLowerCase().includes(query),
  );
  const rows =
    model && !matching.some((option) => option.value === model)
      ? [
          modelOptions.find((option) => option.value === model),
          ...matching,
        ].filter((option): option is { value: string; label: string } =>
          Boolean(option),
        )
      : matching;
  const visibleRows = showAll ? rows : rows.slice(0, MAX_MODEL_ROWS);
  const hiddenCount = rows.length - visibleRows.length;
  const showFilterInput = modelOptions.length > MAX_MODEL_ROWS;

  return (
    <SettingContainer
      title={t(`${base}.title`)}
      description={isFetchingModels ? t(`${base}.fetchingList`) : description}
      descriptionMode="inline"
      layout="stacked"
      grouped={true}
    >
      <div className="space-y-2">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <p className="text-xs text-secondary/70">
            {/* Honesty rule (review round): the fetched-at summary may only
                appear when a list fetch actually succeeded. When options
                hold nothing but the upserted current model (e.g. a custom
                id was Set with no list fetched yet) the line must say no
                list exists, never "fetched " with a dangling empty time. */}
            {isFetchingModels
              ? t(`${base}.fetchingList`)
              : modelListFetchedAtUnix !== null && modelOptions.length > 0
                ? t(`${base}.listSummary`, {
                    count: modelOptions.length,
                    time: new Date(
                      modelListFetchedAtUnix * 1000,
                    ).toLocaleTimeString(),
                  })
                : apiKeyPresent
                  ? t(`${base}.notFetched`)
                  : t(`${base}.emptyNoKey`)}
          </p>
          <Button
            onClick={onRefresh}
            variant="secondary"
            size="sm"
            disabled={isFetchingModels || !apiKeyPresent}
            className="shrink-0 flex items-center gap-1.5"
            title={
              apiKeyPresent
                ? undefined
                : t("settings.postProcessing.api.model.emptyNoKey")
            }
          >
            <RefreshCcw
              className={`h-3.5 w-3.5 ${isFetchingModels ? "animate-spin" : ""}`}
            />
            {t(`${base}.refreshModels`)}
          </Button>
        </div>

        {modelFetchError ? (
          <Alert variant="error" contained>
            <div className="flex flex-1 flex-wrap items-center justify-between gap-3">
              <span className="text-sm">
                {t(`${base}.fetchFailed`, {
                  failureClass: modelFetchError.kind,
                  detail: modelFetchError.detail,
                })}
              </span>
              <Button
                onClick={onRefresh}
                variant="secondary"
                size="sm"
                disabled={isFetchingModels}
                className="shrink-0"
              >
                {t(`${base}.retry`)}
              </Button>
            </div>
          </Alert>
        ) : modelOptions.length > 0 ? (
          <>
            {showFilterInput && (
              <div className="relative">
                <Search className="absolute left-3 top-1/2 -translate-y-1/2 w-3.5 h-3.5 text-text/40 pointer-events-none" />
                <input
                  type="text"
                  value={filter}
                  onChange={(event) => {
                    setFilter(event.target.value);
                    setShowAll(false);
                  }}
                  placeholder={t(`${base}.filterPlaceholder`)}
                  aria-label={t(`${base}.filterPlaceholder`)}
                  className="w-full pl-8 pr-3 py-1.5 text-sm bg-mid-gray/10 border border-mid-gray/40 rounded-lg focus:outline-none focus:ring-1 focus:ring-logo-primary placeholder:text-text/40"
                />
              </div>
            )}
            <div
              role="radiogroup"
              aria-label={t(`${base}.title`)}
              className="grid gap-1.5 sm:grid-cols-2"
            >
              {visibleRows.map((option) => (
                <ModelOptionRow
                  key={option.value}
                  modelId={option.value}
                  selected={model === option.value}
                  disabled={isModelUpdating}
                  onSelect={() => onSelect(option.value)}
                  activeLabel={t("settings.postProcessing.models.active")}
                />
              ))}
            </div>
            {(showAll || hiddenCount > 0) && (
              <button
                type="button"
                onClick={() => setShowAll((open) => !open)}
                className="text-xs text-text/60 hover:text-text underline underline-offset-2 cursor-pointer"
              >
                {showAll
                  ? t(`${base}.showFewer`)
                  : t(`${base}.showAll`, { count: rows.length })}
              </button>
            )}
          </>
        ) : (
          !isFetchingModels && (
            <div className="rounded-md border border-dashed border-mid-gray/40 px-3 py-4 text-center">
              <p className="text-sm text-secondary">
                {t(`${base}.emptyTitle`)}
              </p>
              <p className="mt-1 text-xs text-secondary/70">
                {/* The no-key state names the actual blocker (the key field
                 * above) instead of a dead end; the plain hint only applies
                 * once a refresh could succeed. */}
                {apiKeyPresent
                  ? t(`${base}.emptyHint`)
                  : t(`${base}.emptyNoKey`)}
              </p>
            </div>
          )
        )}

        <div className="space-y-1.5 pt-1">
          <div className="flex items-center gap-2">
            <Input
              type="text"
              value={customId}
              onChange={(event) => setCustomId(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter") applyCustomId();
              }}
              placeholder={t(`${base}.placeholderNoOptions`)}
              variant="compact"
              className="flex-1 min-w-0 font-mono"
            />
            <Button
              onClick={applyCustomId}
              variant="secondary"
              size="sm"
              disabled={!customId.trim() || isModelUpdating}
              className="shrink-0"
            >
              {t(`${base}.customSet`)}
            </Button>
          </div>
          <p className="text-xs text-secondary/70">{t(`${base}.customHint`)}</p>
        </div>
      </div>
    </SettingContainer>
  );
};

/// One row of the template library list: name, language/register badges,
/// the catalog description, and the row actions (select happens through the
/// dropdown above; Edit focuses this template in the editor below; Test
/// opens the Prompt Studio preloaded with this template, v1.6.0 W1-L6).
const TemplateRow: React.FC<{
  prompt: LLMPrompt;
  isSelected: boolean;
  onSelect: () => void;
  onEdit: () => void;
  onDuplicate: () => void;
  onTest: () => void;
  onDelete: () => void;
  canDelete: boolean;
}> = ({
  prompt,
  isSelected,
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
  // The language pill only renders when the template carries one: an
  // undefined language used to render an empty gray pill that read as a
  // broken badge. "auto" localizes like every other language label.
  const languageLabel = prompt.language
    ? prompt.language === "auto"
      ? t("settings.general.language.auto")
      : prompt.language
    : null;

  return (
    <div
      className={`p-3 rounded-md border ${
        isSelected
          ? "border-logo-primary bg-logo-primary/5"
          : "border-mid-gray/20"
      }`}
    >
      {/* W1-L9 visual pass: the actions moved to their own wrapping row
          below the identity block. The old single line (name + 4 badges +
          5 buttons, one of them labeled "Test on my last transcript")
          overflowed the card horizontally at the default window width. */}
      <div className="flex flex-col gap-2">
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-1.5">
            <span className="text-sm font-medium truncate">{displayName}</span>
            {languageLabel && (
              <Badge variant="secondary">{languageLabel}</Badge>
            )}
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
        <div className="flex flex-wrap items-center gap-1.5">
          {isSelected ? (
            /* A positive selected state (the models catalog's "Active"
             * vocabulary) instead of a grayed-out Select button that read
             * as a broken control on the very card that is selected. */
            <Badge variant="primary" className="gap-1">
              <Check className="h-3 w-3" aria-hidden="true" />
              {t("settings.postProcessing.models.active")}
            </Badge>
          ) : (
            <Button onClick={onSelect} variant="secondary" size="sm">
              {t(`${base}.selectPrompt`)}
            </Button>
          )}
          <Button onClick={onEdit} variant="secondary" size="sm">
            {t(`${base}.editPrompt`)}
          </Button>
          <Button onClick={onDuplicate} variant="secondary" size="sm">
            {t(`${base}.duplicate`)}
          </Button>
          <Button
            onClick={onTest}
            variant="secondary"
            size="sm"
            title={t(`${base}.testOnLastTranscript`)}
          >
            {t(`${base}.testShort`)}
          </Button>
          <Button
            onClick={onDelete}
            variant="secondary"
            size="sm"
            disabled={!canDelete}
          >
            {t(`${base}.deleteShort`)}
          </Button>
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
  // The create form's Save/Cancel row: entering create mode scrolls it into
  // view so the actions that commit the draft render fully above the fold
  // instead of being clipped at the window bottom.
  const createActionRef = useRef<HTMLDivElement>(null);

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

  const handleTestPrompt = (promptId: string) => {
    // v1.6.0 W1-L6: Test opens the Prompt Studio preloaded with this
    // template instead of rendering the buried inline panel. The studio
    // runs the same engine lifecycle with visible diffs, engine choice,
    // and re-runs; nothing about the template library itself changes.
    openPromptStudio(promptId);
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
    // The form renders below the fold on short windows; bring its commit
    // row into view once it exists.
    requestAnimationFrame(() => {
      createActionRef.current?.scrollIntoView({ block: "end" });
    });
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
          {/* While the create form is open this row swaps from the (now
              pointless, disabled) Create New Prompt to an enabled Cancel, so
              an actionable affordance always sits inside the visible area
              even when the form's own Save/Cancel row is below the fold. */}
          <Button
            onClick={isCreating ? handleCancelCreate : handleStartCreate}
            variant={isCreating ? "secondary" : "primary"}
            size="md"
            className="shrink-0"
          >
            {isCreating
              ? t("settings.postProcessing.prompts.cancel")
              : t("settings.postProcessing.prompts.createNew")}
          </Button>
        </div>

        {!isCreating && hasPrompts && (
          <div className="space-y-2">
            {prompts.map((prompt) => (
              <TemplateRow
                key={prompt.id}
                prompt={prompt}
                isSelected={prompt.id === selectedPromptId}
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

            <div ref={createActionRef} className="flex gap-2 pt-2">
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

      <SettingsGroup
        // W1-L9 visual pass: "API (OpenAI Compatible)" lied when the local
        // on-device engine was the selected provider; the group header now
        // names the surface it actually shows.
        title={
          showLocalRow(settings?.post_process_provider_id ?? "")
            ? t("settings.postProcessing.api.titleLocal")
            : t("settings.postProcessing.api.title")
        }
      >
        <PostProcessingSettingsApi />
      </SettingsGroup>

      {/* The local models catalog: a page-level sibling of the ENGINE card,
          not a nested card inside it. Rendering it here gives its header the
          same clearance and its content the same inner padding as the
          HOTKEY/ENGINE/PROMPTS sections (the old nesting glued the header to
          the ENGINE card's border). */}
      {showLocalRow(settings?.post_process_provider_id ?? "") && (
        <PostProcessModelsSection />
      )}

      <SettingsGroup title={t("settings.postProcessing.prompts.title")}>
        <PostProcessingSettingsPrompts />
      </SettingsGroup>
    </div>
  );
};
