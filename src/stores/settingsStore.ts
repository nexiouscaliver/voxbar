import { create } from "zustand";
import { subscribeWithSelector } from "zustand/middleware";
import { listen } from "@tauri-apps/api/event";
import type {
  AppSettings as Settings,
  AudioDevice,
  ChineseScript,
  CommandMatrixEntry,
  NumberFormat,
  TranscribeAcceleratorSetting,
  OrtAcceleratorSetting,
  PostProcessModelError,
  ShortcutActivation,
  UpdatePolicy,
  VadBackend,
} from "@/bindings";
import { commands } from "@/bindings";
import { toast } from "sonner";
import i18n from "../i18n";
import { mapCommandResult } from "./settingsWriteOutcome";
import {
  hydrateModelOptions,
  withModelFetchError,
  type ModelFetchErrors,
} from "./postProcessModelCache";
import {
  PRE_HYDRATION_STASH_LIMIT,
  PreHydrationUpdateQueue,
} from "./settingsHydrationQueue";
import type { UnlistenFn } from "@tauri-apps/api/event";

interface SettingsStore {
  settings: Settings | null;
  defaultSettings: Settings | null;
  isLoading: boolean;
  isUpdating: Record<string, boolean>;
  audioDevices: AudioDevice[];
  outputDevices: AudioDevice[];
  customSounds: { start: boolean; stop: boolean };
  postProcessModelOptions: Record<string, string[]>;
  // Classified model-list fetch errors per provider (null = no error). A
  // failure surfaces here so the panel can show it inline; it never
  // blanks the options the user already has.
  postProcessModelFetchErrors: ModelFetchErrors;
  // null until loadUpdateChecksLocked() resolves
  updateChecksLocked: boolean | null;

  // Actions
  initialize: () => Promise<void>;
  loadDefaultSettings: () => Promise<void>;
  loadUpdateChecksLocked: () => Promise<void>;
  updateSetting: <K extends keyof Settings>(
    key: K,
    value: Settings[K],
  ) => Promise<void>;
  resetSetting: (key: keyof Settings) => Promise<void>;
  refreshSettings: () => Promise<void>;
  refreshAudioDevices: () => Promise<void>;
  refreshOutputDevices: () => Promise<void>;
  updateBinding: (id: string, binding: string) => Promise<void>;
  resetBinding: (id: string) => Promise<void>;
  getSetting: <K extends keyof Settings>(key: K) => Settings[K] | undefined;
  isUpdatingKey: (key: string) => boolean;
  playTestSound: (soundType: "start" | "stop") => Promise<void>;
  checkCustomSounds: () => Promise<void>;
  setPostProcessProvider: (providerId: string) => Promise<void>;
  updatePostProcessSetting: (
    settingType: "base_url" | "api_key" | "model",
    providerId: string,
    value: string,
  ) => Promise<void>;
  updatePostProcessBaseUrl: (
    providerId: string,
    baseUrl: string,
  ) => Promise<void>;
  updatePostProcessApiKey: (
    providerId: string,
    apiKey: string,
  ) => Promise<void>;
  updatePostProcessModel: (providerId: string, model: string) => Promise<void>;
  fetchPostProcessModels: (providerId: string) => Promise<string[]>;
  setPostProcessModelOptions: (providerId: string, models: string[]) => void;

  // Internal state setters
  setSettings: (settings: Settings | null) => void;
  setDefaultSettings: (defaultSettings: Settings | null) => void;
  setLoading: (loading: boolean) => void;
  setUpdating: (key: string, updating: boolean) => void;
  setAudioDevices: (devices: AudioDevice[]) => void;
  setOutputDevices: (devices: AudioDevice[]) => void;
  setCustomSounds: (sounds: { start: boolean; stop: boolean }) => void;
}

// Note: Default settings are now fetched from Rust via commands.getDefaultSettings()
// This ensures platform-specific defaults (like overlay_position, shortcuts, paste_method) work correctly

const DEFAULT_AUDIO_DEVICE: AudioDevice = {
  index: "default",
  name: "Default",
  is_default: true,
};

// Module-level singleton for initialize(): see the comment on the action.
let initializePromise: Promise<void> | null = null;
const initializeUnlisteners: UnlistenFn[] = [];

// AUD-10: the in-flight (or settled) lock probe from loadUpdateChecksLocked,
// shared exactly like initializePromise above so every caller - the startup
// initialize() pass and the update gate (updateCheckGate) - awaits the SAME
// promise instead of racing a second probe against the first.
let updateChecksLockedPromise: Promise<void> | null = null;

// W1-L4 pre-hydration intents: updateSetting calls that land while
// settings === null (the boot window before the first getAppSettings
// response) are stashed here instead of running. See
// settingsHydrationQueue.ts for the why and the bound. The queue is
// strictly boot-scoped: nothing ever sets settings back to null, so it is
// filled before the first hydration and drained exactly once, right after
// the first successful refreshSettings.
const preHydrationUpdates = new PreHydrationUpdateQueue();

// W1-L4: a FAILED hydration must not be silent about queued intents. The
// intents are RETAINED, not dropped: the lane contract prefers never
// losing user intent, the queue is bounded so lingering is safe, and the
// next successful refreshSettings (the settings-changed / model-state-
// changed listeners initialize() registers fire one) still replays them.
// The warn exists so the retain-and-replay-later is visible in the
// console instead of applying at an arbitrary later moment with no trace.
function warnPreHydrationIntentsPending(): void {
  if (preHydrationUpdates.size > 0) {
    console.warn(
      `Settings hydration failed; ${preHydrationUpdates.size} pre-hydration update intent(s) stay queued and will replay on the next successful settings load`,
    );
  }
}

// W1-L4 numeric clamps (cycle-3 wave residual): the backend writes several
// numeric settings raw and unclamped (no load-time clamp in settings.rs,
// no range check in the change_* commands), so the store setter is the
// chokepoint. Each bound below is transcribed from the per-setting UI,
// which already declares it: touch a bound here and the component beside
// the citation must move with it.
//
//   hold_threshold_ms           100..1000   HoldThreshold.tsx:28-29
//   audio_feedback_volume       0..1        VolumeSlider.tsx:19-20
//   word_correction_threshold   0..1        WordCorrectionThreshold.tsx:27-28
//   paste_delay_ms              10..5000    PasteDelay.tsx:36-37
//   paste_delay_after_ms        10..5000    PasteDelay.tsx:36-37 (shared row)
//   history_limit               0..1000     HistoryLimit.tsx:117-118
//   extra_recording_buffer_ms   0..1500     RecordingBuffer.tsx:28-29
//   streaming_release_tail_ms   0..1000     StreamingReleaseTail.tsx:35-36
//
// memory_gate_headroom_mb is not a min/max: the rule is 0 (guard off) or
// >= 5 MB, and the UI REJECTS 1-4 rather than clamping (see the rule
// stated in memoryMarginInput.ts lines 4-8: "a deliberate 3 is never
// rewritten behind the user's back"), with the backend normalizing stale
// stored 1-4 down to 0 on load. The setter mirrors that backend
// normalization (1-4 land as 0), never an invented clamp to 5.
//
// post_process_timeout_secs and post_process_local_keep_warm_secs are
// deliberately absent: the backend already rejects out-of-range values for
// them (commands/transcription.rs, shortcut/mod.rs:2244), so no frontend
// clamp is needed.
type SettingNumericClamp =
  | { kind: "minMax"; min: number; max: number }
  | { kind: "zeroOrAtLeast"; least: number };

export const SETTING_NUMERIC_CLAMPS: Partial<
  Record<keyof Settings, SettingNumericClamp>
> = {
  hold_threshold_ms: { kind: "minMax", min: 100, max: 1000 },
  audio_feedback_volume: { kind: "minMax", min: 0, max: 1 },
  word_correction_threshold: { kind: "minMax", min: 0, max: 1 },
  paste_delay_ms: { kind: "minMax", min: 10, max: 5000 },
  paste_delay_after_ms: { kind: "minMax", min: 10, max: 5000 },
  history_limit: { kind: "minMax", min: 0, max: 1000 },
  extra_recording_buffer_ms: { kind: "minMax", min: 0, max: 1500 },
  streaming_release_tail_ms: { kind: "minMax", min: 0, max: 1000 },
  memory_gate_headroom_mb: { kind: "zeroOrAtLeast", least: 5 },
};

/** Clamp a numeric setting to its declared bound at the setter, so an
 * out-of-range value (typed into a number field, sent by a stray caller)
 * is corrected before the optimistic write AND the backend write. Values
 * for keys without a declared bound, and non-numeric values, pass through
 * untouched; a non-finite number lands on the bound's floor.
 */
export function clampSettingValue<K extends keyof Settings>(
  key: K,
  value: Settings[K],
): Settings[K] {
  const clamp = SETTING_NUMERIC_CLAMPS[key];
  if (!clamp || typeof value !== "number") {
    return value;
  }
  if (!Number.isFinite(value)) {
    return (clamp.kind === "minMax" ? clamp.min : 0) as Settings[K];
  }
  if (clamp.kind === "minMax") {
    return Math.min(clamp.max, Math.max(clamp.min, value)) as Settings[K];
  }
  // zeroOrAtLeast: 0 is valid (the off value); anything positive below the
  // threshold normalizes to 0, mirroring the backend's load-time guard;
  // negatives are garbage and also land on 0.
  return (value >= clamp.least ? value : 0) as Settings[K];
}

const settingUpdaters: {
  [K in keyof Settings]?: (value: Settings[K]) => Promise<unknown>;
} = {
  always_on_microphone: async (value) => {
    const result = await commands.updateMicrophoneMode(value as boolean);
    if (result.status === "error") {
      // A failed enable (no input device, revoked permission) must roll
      // the toggle back: the backend keeps the prior mode and does not
      // persist the preference when the runtime flip fails.
      throw new Error(result.error);
    }
  },
  audio_feedback: (value) =>
    commands.changeAudioFeedbackSetting(value as boolean),
  memory_pressure_guard: (value) =>
    commands.changeMemoryPressureGuardSetting(value as boolean),
  memory_gate_headroom_mb: (value) =>
    commands.changeMemoryGateHeadroomSetting(value as number),
  companion_devices_enabled: (value) =>
    commands.changeCompanionDevicesSetting(value as boolean),
  stall_watchdog_enabled: (value) =>
    commands.changeStallWatchdogSetting(value as boolean),
  auto_fallback: (value) =>
    commands.changeAutoFallbackSetting(value as boolean),
  menu_bar_model_title: (value) =>
    commands.changeMenuBarModelTitleSetting(value as boolean),
  show_history_model: (value) =>
    commands.changeShowHistoryModelSetting(value as boolean),
  audio_feedback_volume: (value) =>
    commands.changeAudioFeedbackVolumeSetting(value as number),
  sound_theme: (value) => commands.changeSoundThemeSetting(value as string),
  start_hidden: (value) => commands.changeStartHiddenSetting(value as boolean),
  autostart_enabled: (value) =>
    commands.changeAutostartSetting(value as boolean),
  update_checks_enabled: (value) =>
    commands.changeUpdateChecksSetting(value as boolean),
  update_policy: (value) =>
    commands.changeUpdatePolicySetting(value as UpdatePolicy),
  show_whats_new_on_update: (value) =>
    commands.changeShowWhatsNewOnUpdateSetting(value as boolean),
  whats_new_last_seen_version: (value) =>
    commands.changeWhatsNewLastSeenVersionSetting(value as string),
  shortcut_activation: (value) =>
    commands.changeShortcutActivationSetting(value as ShortcutActivation),
  hold_threshold_ms: (value) =>
    commands.changeHoldThresholdMsSetting(value as number),
  selected_microphone: (value) =>
    commands.setSelectedMicrophone(
      (value as string) === "Default" || value === null
        ? "default"
        : (value as string),
    ),
  selected_channel: (value) =>
    commands.setSelectedChannel((value as number | null | undefined) ?? null),
  clamshell_microphone: (value) =>
    commands.setClamshellMicrophone(
      (value as string) === "Default" ? "default" : (value as string),
    ),
  selected_output_device: (value) =>
    commands.setSelectedOutputDevice(
      (value as string) === "Default" || value === null
        ? "default"
        : (value as string),
    ),
  recording_retention_period: (value) =>
    commands.updateRecordingRetentionPeriod(value as string),
  translate_to_english: (value) =>
    commands.changeTranslateToEnglishSetting(value as boolean),
  selected_language: (value) =>
    commands.changeSelectedLanguageSetting(value as string),
  overlay_position: (value) =>
    commands.changeOverlayPositionSetting(value as string),
  debug_mode: (value) => commands.changeDebugModeSetting(value as boolean),
  custom_words: (value) => commands.updateCustomWords(value as string[]),
  word_correction_threshold: (value) =>
    commands.changeWordCorrectionThresholdSetting(value as number),
  paste_delay_ms: (value) =>
    commands.changePasteDelayMsSetting(value as number),
  paste_delay_after_ms: (value) =>
    commands.changePasteDelayAfterMsSetting(value as number),
  reliable_paste: (value) =>
    commands.changeReliablePasteSetting(value as boolean),
  paste_method: (value) => commands.changePasteMethodSetting(value as string),
  typing_tool: (value) => commands.changeTypingToolSetting(value as string),
  external_script_path: (value) =>
    commands.changeExternalScriptPathSetting(value as string | null),
  clipboard_handling: (value) =>
    commands.changeClipboardHandlingSetting(value as string),
  auto_submit: (value) => commands.changeAutoSubmitSetting(value as boolean),
  auto_submit_key: (value) =>
    commands.changeAutoSubmitKeySetting(value as string),
  history_limit: (value) => commands.updateHistoryLimit(value as number),
  post_process_enabled: (value) =>
    commands.changePostProcessEnabledSetting(value as boolean),
  post_process_timeout_secs: (value) =>
    commands.setPostProcessTimeout(value as number),
  post_process_local_keep_warm_secs: (value) =>
    commands.changePostProcessLocalKeepWarmSecsSetting(value as number),
  post_process_selected_prompt_id: (value) =>
    commands.setPostProcessSelectedPrompt(value as string),
  mute_while_recording: (value) =>
    commands.changeMuteWhileRecordingSetting(value as boolean),
  append_trailing_space: (value) =>
    commands.changeAppendTrailingSpaceSetting(value as boolean),
  log_level: (value) => commands.setLogLevel(value as any),
  app_language: (value) => commands.changeAppLanguageSetting(value as string),
  theme: (value) => commands.changeThemeSetting(value as string),
  accent_color: (value) => commands.changeAccentColorSetting(value as string),
  experimental_enabled: (value) =>
    commands.changeExperimentalEnabledSetting(value as boolean),
  lazy_stream_close: (value) =>
    commands.changeLazyStreamCloseSetting(value as boolean),
  overlay_style: (value) => commands.changeOverlayStyleSetting(value as string),
  vad_enabled: (value) => commands.changeVadEnabledSetting(value as boolean),
  vad_backend: (value) => commands.changeVadBackendSetting(value as VadBackend),
  filler_word_removal_enabled: (value) =>
    commands.changeFillerWordRemovalEnabledSetting(value as boolean),
  spoken_punctuation: (value) =>
    commands.changeSpokenPunctuationSetting(value as boolean),
  auto_interpret_commands: (value) =>
    commands.changeAutoInterpretCommandsSetting(value as boolean),
  terminal_punctuation: (value) =>
    commands.changeTerminalPunctuationSetting(value as boolean),
  voice_deletion_commands: (value) =>
    commands.changeVoiceDeletionCommandsSetting(value as boolean),
  command_phrases: (value) =>
    commands.updateCommandMatrix(value as CommandMatrixEntry[] | null),
  preview_before_paste: (value) =>
    commands.changePreviewBeforePasteSetting(value as boolean),
  delete_last_word_enabled: (value) =>
    commands.changeDeleteLastWordEnabledSetting(value as boolean),
  undo_enabled: (value) => commands.changeUndoEnabledSetting(value as boolean),
  command_mode_enabled: (value) =>
    commands.changeCommandModeEnabledSetting(value as boolean),
  chinese_script: (value) =>
    commands.changeChineseScriptSetting(value as ChineseScript),
  number_format: (value) =>
    commands.changeNumberFormatSetting(value as NumberFormat),
  show_tray_icon: (value) =>
    commands.changeShowTrayIconSetting(value as boolean),
  transcribe_accelerator: (value) =>
    commands.changeTranscribeAcceleratorSetting(
      value as TranscribeAcceleratorSetting,
    ),
  ort_accelerator: (value) =>
    commands.changeOrtAcceleratorSetting(value as OrtAcceleratorSetting),
  transcribe_gpu_device: (value) =>
    commands.changeTranscribeGpuDevice(value as string | null),
  extra_recording_buffer_ms: (value) =>
    commands.changeExtraRecordingBufferSetting(value as number),
  streaming_release_tail_ms: (value) =>
    commands.changeStreamingReleaseTailSetting(value as number),
};

export const useSettingsStore = create<SettingsStore>()(
  subscribeWithSelector((set, get) => ({
    settings: null,
    defaultSettings: null,
    isLoading: true,
    isUpdating: {},
    audioDevices: [],
    outputDevices: [],
    customSounds: { start: false, stop: false },
    postProcessModelOptions: {},
    postProcessModelFetchErrors: {},
    updateChecksLocked: null,

    // Internal setters
    setSettings: (settings) => set({ settings }),
    setDefaultSettings: (defaultSettings) => set({ defaultSettings }),
    setLoading: (isLoading) => set({ isLoading }),
    setUpdating: (key, updating) =>
      set((state) => ({
        isUpdating: { ...state.isUpdating, [key]: updating },
      })),
    setAudioDevices: (audioDevices) => set({ audioDevices }),
    setOutputDevices: (outputDevices) => set({ outputDevices }),
    setCustomSounds: (customSounds) => set({ customSounds }),

    // Getters
    getSetting: (key) => get().settings?.[key],
    isUpdatingKey: (key) => get().isUpdating[key] || false,

    // Load settings from store
    refreshSettings: async () => {
      try {
        const result = await commands.getAppSettings();
        if (result.status === "ok") {
          const settings = result.data;
          const normalizedSettings: Settings = {
            ...settings,
            always_on_microphone: settings.always_on_microphone ?? false,
            selected_microphone: settings.selected_microphone ?? "Default",
            clamshell_microphone: settings.clamshell_microphone ?? "Default",
            selected_output_device:
              settings.selected_output_device ?? "Default",
          };
          set((state) => ({
            settings: normalizedSettings,
            isLoading: false,
            // Hydrate the model dropdowns from the persisted cache so the
            // panel opens instantly (and offline). Lists fetched this
            // session already in state win over the cache.
            postProcessModelOptions: hydrateModelOptions(
              normalizedSettings.post_process_model_lists,
              state.postProcessModelOptions,
            ),
          }));

          // W1-L4: hydration just completed. Replay any updateSetting
          // intents stashed while settings was null, serially and through
          // the normal updateSetting path, so each one gets the full
          // optimistic-write / backend-write / rollback treatment a direct
          // call would. drain() empties the queue synchronously, so a
          // concurrent refreshSettings (a settings-changed event landing
          // mid-replay) finds it empty and cannot double-apply.
          const replayable = preHydrationUpdates.drain();
          for (const [replayKey, replayValue] of replayable) {
            await get().updateSetting(
              replayKey,
              replayValue as Settings[typeof replayKey],
            );
          }
        } else {
          console.error("Failed to load settings:", result.error);
          warnPreHydrationIntentsPending();
          set({ isLoading: false });
        }
      } catch (error) {
        console.error("Failed to load settings:", error);
        warnPreHydrationIntentsPending();
        set({ isLoading: false });
      }
    },

    // Load audio devices
    refreshAudioDevices: async () => {
      try {
        const result = await commands.getAvailableMicrophones();
        if (result.status === "ok") {
          const devicesWithDefault = [
            DEFAULT_AUDIO_DEVICE,
            ...result.data.filter(
              (d) => d.name !== "Default" && d.name !== "default",
            ),
          ];
          set({ audioDevices: devicesWithDefault });
        } else {
          set({ audioDevices: [DEFAULT_AUDIO_DEVICE] });
        }
      } catch (error) {
        console.error("Failed to load audio devices:", error);
        set({ audioDevices: [DEFAULT_AUDIO_DEVICE] });
      }
    },

    // Load output devices
    refreshOutputDevices: async () => {
      try {
        const result = await commands.getAvailableOutputDevices();
        if (result.status === "ok") {
          const devicesWithDefault = [
            DEFAULT_AUDIO_DEVICE,
            ...result.data.filter(
              (d) => d.name !== "Default" && d.name !== "default",
            ),
          ];
          set({ outputDevices: devicesWithDefault });
        } else {
          set({ outputDevices: [DEFAULT_AUDIO_DEVICE] });
        }
      } catch (error) {
        console.error("Failed to load output devices:", error);
        set({ outputDevices: [DEFAULT_AUDIO_DEVICE] });
      }
    },

    // Play a test sound
    playTestSound: async (soundType: "start" | "stop") => {
      try {
        await commands.playTestSound(soundType);
      } catch (error) {
        console.error(`Failed to play test sound (${soundType}):`, error);
      }
    },

    checkCustomSounds: async () => {
      try {
        const sounds = await commands.checkCustomSounds();
        get().setCustomSounds(sounds);
      } catch (error) {
        console.error("Failed to check custom sounds:", error);
      }
    },

    // Update a specific setting
    updateSetting: async <K extends keyof Settings>(
      key: K,
      value: Settings[K],
    ) => {
      // W1-L4 numeric clamp: bounds declared by the per-setting UI are
      // enforced here (see SETTING_NUMERIC_CLAMPS above) so an
      // out-of-range value is corrected before ANY write, optimistic or
      // backend, direct or replayed.
      const clampedValue = clampSettingValue(key, value);

      // W1-L4 hydration guard: while settings === null (the boot window
      // before the first getAppSettings response) do NOTHING observable:
      // no optimistic write, no backend write, no isUpdating spinner. The
      // old code skipped only the optimistic write but still ran the
      // backend updater, and the in-flight hydration response then
      // overwrote the store with the pre-write value, silently losing the
      // user's change. Instead, stash the intent and replay it on
      // hydration (refreshSettings, above). The stash is bounded; a
      // refused intent (a runaway caller touching more distinct keys than
      // the queue holds) is dropped with a warn rather than growing
      // without limit.
      if (!get().settings) {
        const stash = preHydrationUpdates.stash(key, clampedValue);
        if (!stash.stashed) {
          console.warn(
            `Pre-hydration updateSetting(${String(key)}) dropped: stash already holds ${preHydrationUpdates.size} distinct keys (limit ${PRE_HYDRATION_STASH_LIMIT}); the setting was not written`,
          );
        }
        return;
      }

      const { settings, setUpdating } = get();
      const updateKey = String(key);
      const originalValue = settings?.[key];

      setUpdating(updateKey, true);

      try {
        set((state) => ({
          settings: state.settings
            ? { ...state.settings, [key]: clampedValue }
            : null,
        }));

        const updater = settingUpdaters[key];
        let result: unknown = null;
        if (updater) {
          result = await updater(clampedValue);
        } else if (key !== "bindings" && key !== "selected_model") {
          // Handled elsewhere (bindings via updateBinding, selected_model
          // via the model store); anything else is a programming slip.
          console.warn(`No handler for setting: ${String(key)}`);
        }

        // A command that resolves with an error status is as much a failure
        // as one that throws: roll the optimistic value back and say so,
        // or the UI keeps showing a setting the backend never accepted.
        const outcome = mapCommandResult(result, updateKey);
        if (!outcome.ok) {
          console.error(
            `Failed to update setting ${updateKey}:`,
            outcome.error,
          );
          // KB-203: restore ONLY this key, from the state as it is NOW.
          // The pre-patch snapshot would silently revert unrelated keys
          // that changed while the command was in flight (a concurrent
          // successful updateSetting, or a settings-changed refresh).
          if (settings) {
            set((state) => ({
              settings: state.settings
                ? { ...state.settings, [key]: originalValue }
                : null,
            }));
          }
          toast.error(
            i18n.t("toast.settingNotSaved", { error: outcome.error ?? "" }),
          );
        }
      } catch (error) {
        console.error(`Failed to update setting ${String(key)}:`, error);
        // KB-203: same per-key rollback against the current state - never
        // the wholesale pre-patch snapshot.
        if (settings) {
          set((state) => ({
            settings: state.settings
              ? { ...state.settings, [key]: originalValue }
              : null,
          }));
        }
        toast.error(
          i18n.t("toast.settingNotSaved", {
            error: error instanceof Error ? error.message : String(error),
          }),
        );
      } finally {
        setUpdating(updateKey, false);
      }
    },

    // Reset a setting to its default value
    resetSetting: async (key) => {
      const { defaultSettings } = get();
      if (defaultSettings) {
        const defaultValue = defaultSettings[key];
        if (defaultValue !== undefined) {
          await get().updateSetting(key, defaultValue as any);
        }
      }
    },

    // Update a specific binding
    updateBinding: async (id, binding) => {
      const { settings, setUpdating } = get();
      const updateKey = `binding_${id}`;
      const originalBinding = settings?.bindings?.[id]?.current_binding;

      setUpdating(updateKey, true);

      try {
        // Optimistic update
        set((state) => ({
          settings: state.settings
            ? {
                ...state.settings,
                bindings: {
                  ...state.settings.bindings,
                  [id]: {
                    ...state.settings.bindings?.[id]!,
                    current_binding: binding,
                  },
                },
              }
            : null,
        }));

        const result = await commands.changeBinding(id, binding);

        // Check if the command executed successfully
        if (result.status === "error") {
          throw new Error(result.error);
        }

        // Check if the binding change was successful
        if (!result.data.success) {
          throw new Error(result.data.error || "Failed to update binding");
        }
      } catch (error) {
        console.error(`Failed to update binding ${id}:`, error);

        // Rollback on error
        if (originalBinding && get().settings) {
          set((state) => ({
            settings: state.settings
              ? {
                  ...state.settings,
                  bindings: {
                    ...state.settings.bindings,
                    [id]: {
                      ...state.settings.bindings?.[id]!,
                      current_binding: originalBinding,
                    },
                  },
                }
              : null,
          }));
        }

        // Re-throw to let the caller know it failed
        throw error;
      } finally {
        setUpdating(updateKey, false);
      }
    },

    // Reset a specific binding
    resetBinding: async (id) => {
      const { setUpdating, refreshSettings } = get();
      const updateKey = `binding_${id}`;

      setUpdating(updateKey, true);

      try {
        await commands.resetBinding(id);
        await refreshSettings();
      } catch (error) {
        console.error(`Failed to reset binding ${id}:`, error);
      } finally {
        setUpdating(updateKey, false);
      }
    },

    setPostProcessProvider: async (providerId) => {
      const {
        settings,
        setUpdating,
        refreshSettings,
        setPostProcessModelOptions,
      } = get();
      const updateKey = "post_process_provider_id";
      const previousId = settings?.post_process_provider_id ?? null;

      setUpdating(updateKey, true);

      if (settings) {
        set((state) => ({
          settings: state.settings
            ? { ...state.settings, post_process_provider_id: providerId }
            : null,
        }));
      }

      // Clear cached model options for the new provider so the dropdown
      // doesn't show stale models from a previous fetch or base_url.
      setPostProcessModelOptions(providerId, []);

      try {
        await commands.setPostProcessProvider(providerId);
        await refreshSettings();
      } catch (error) {
        console.error("Failed to set post-process provider:", error);
        if (previousId !== null) {
          set((state) => ({
            settings: state.settings
              ? { ...state.settings, post_process_provider_id: previousId }
              : null,
          }));
        }
      } finally {
        setUpdating(updateKey, false);
      }
    },

    // Generic updater for post-processing provider settings
    updatePostProcessSetting: async (
      settingType: "base_url" | "api_key" | "model",
      providerId: string,
      value: string,
    ) => {
      const { setUpdating, refreshSettings } = get();
      const updateKey = `post_process_${settingType}:${providerId}`;

      setUpdating(updateKey, true);

      try {
        if (settingType === "base_url") {
          await commands.changePostProcessBaseUrlSetting(providerId, value);
        } else if (settingType === "api_key") {
          await commands.changePostProcessApiKeySetting(providerId, value);
        } else if (settingType === "model") {
          await commands.changePostProcessModelSetting(providerId, value);
        }
        await refreshSettings();
      } catch (error) {
        console.error(
          `Failed to update post-process ${settingType.replace("_", " ")}:`,
          error,
        );
      } finally {
        setUpdating(updateKey, false);
      }
    },

    updatePostProcessBaseUrl: async (providerId, baseUrl) => {
      const { setUpdating, refreshSettings } = get();
      const updateKey = `post_process_base_url:${providerId}`;

      setUpdating(updateKey, true);

      try {
        // Persist the new base URL first.
        const urlResult = await commands.changePostProcessBaseUrlSetting(
          providerId,
          baseUrl,
        );
        if (urlResult.status === "error") {
          console.error("Failed to persist base URL:", urlResult.error);
          return;
        }

        // Reset the stored model since the previous value is almost certainly
        // invalid for the new endpoint (e.g. switching Custom from Groq to
        // Cerebras). Only proceed if the reset succeeds.
        const modelResult = await commands.changePostProcessModelSetting(
          providerId,
          "",
        );
        if (modelResult.status === "error") {
          console.error("Failed to reset model setting:", modelResult.error);
          return;
        }

        // Clear cached model options only after both backend writes succeed.
        set((state) => ({
          postProcessModelOptions: {
            ...state.postProcessModelOptions,
            [providerId]: [],
          },
        }));

        // Single refresh after both backend writes.
        await refreshSettings();
      } catch (error) {
        console.error("Failed to update post-process base URL:", error);
      } finally {
        setUpdating(updateKey, false);
      }
    },

    updatePostProcessApiKey: async (providerId, apiKey) => {
      // Clear cached models when API key changes - user should click refresh after
      set((state) => ({
        postProcessModelOptions: {
          ...state.postProcessModelOptions,
          [providerId]: [],
        },
      }));
      return get().updatePostProcessSetting("api_key", providerId, apiKey);
    },

    updatePostProcessModel: async (providerId, model) => {
      return get().updatePostProcessSetting("model", providerId, model);
    },

    fetchPostProcessModels: async (providerId) => {
      const updateKey = `post_process_models_fetch:${providerId}`;
      const { setUpdating, setPostProcessModelOptions, refreshSettings } =
        get();

      // One fetch per provider at a time: the on-open effect and the
      // provider-switch handler can both request the same list; the second
      // caller rides on the in-flight one instead of doubling the request.
      if (get().isUpdating[updateKey]) {
        return [];
      }

      setUpdating(updateKey, true);

      try {
        // Call Tauri backend command instead of fetch
        const result = await commands.fetchPostProcessModels(providerId);
        if (result.status === "ok") {
          setPostProcessModelOptions(providerId, result.data);
          set((state) => ({
            postProcessModelFetchErrors: withModelFetchError(
              state.postProcessModelFetchErrors,
              providerId,
              null,
            ),
          }));
          // The backend persisted this list to the settings cache; pull the
          // store copy so the fetched-at hint reflects this fetch.
          await refreshSettings();
          return result.data;
        } else {
          console.error("Failed to fetch models:", result.error);
          // Surface the classified failure in the panel instead of only
          // the console. The previously shown list stays: a failed refresh
          // never blanks the dropdown.
          set((state) => ({
            postProcessModelFetchErrors: withModelFetchError(
              state.postProcessModelFetchErrors,
              providerId,
              result.error,
            ),
          }));
          return [];
        }
      } catch (error) {
        console.error("Failed to fetch models:", error);
        // An error thrown by the invoke layer is transport-shaped; record
        // it as a network failure so the inline alert still shows.
        set((state) => ({
          postProcessModelFetchErrors: withModelFetchError(
            state.postProcessModelFetchErrors,
            providerId,
            {
              kind: "network",
              detail: error instanceof Error ? error.message : String(error),
            },
          ),
        }));
        // Don't cache empty array on error - let user retry
        return [];
      } finally {
        setUpdating(updateKey, false);
      }
    },

    setPostProcessModelOptions: (providerId, models) =>
      set((state) => ({
        postProcessModelOptions: {
          ...state.postProcessModelOptions,
          [providerId]: models,
        },
      })),

    // Load default settings from Rust
    loadDefaultSettings: async () => {
      try {
        const result = await commands.getDefaultSettings();
        if (result.status === "ok") {
          set({ defaultSettings: result.data });
        } else {
          console.error("Failed to load default settings:", result.error);
        }
      } catch (error) {
        console.error("Failed to load default settings:", error);
      }
    },

    // Check whether update checks are locked by system configuration
    // (e.g. HANDY_DISABLE_UPDATER, set by the Nix package). Shared via the
    // module-level updateChecksLockedPromise: the first caller runs the
    // probe and every later caller (the update gate, AUD-10) awaits the
    // same in-flight promise instead of kicking off a duplicate.
    loadUpdateChecksLocked: () => {
      if (!updateChecksLockedPromise) {
        updateChecksLockedPromise = (async () => {
          try {
            const locked = await commands.isUpdateChecksLocked();
            set({ updateChecksLocked: locked });
          } catch (error) {
            console.error("Failed to check update checks lock state:", error);
            // Fail open: an unknown lock state means "not locked", otherwise the
            // update checker waits for it forever and checks never start.
            set({ updateChecksLocked: false });
          }
        })();
      }
      return updateChecksLockedPromise;
    },

    // Initialize everything. App-lifetime listeners are registered exactly
    // once no matter how many callers race in (React StrictMode mounts every
    // effect twice in development): the first call's promise is shared, and
    // the unlisten handles are kept so a future teardown path could use them.
    initialize: () => {
      if (!initializePromise) {
        initializePromise = (async () => {
          const {
            refreshSettings,
            checkCustomSounds,
            loadDefaultSettings,
            loadUpdateChecksLocked,
          } = get();

          // Note: Audio devices are NOT refreshed here. The frontend (App.tsx)
          // is responsible for calling refreshAudioDevices/refreshOutputDevices
          // after onboarding completes. This avoids triggering permission dialogs
          // on macOS before the user is ready.
          await Promise.all([
            loadDefaultSettings(),
            refreshSettings(),
            checkCustomSounds(),
            loadUpdateChecksLocked(),
          ]);

          // Re-fetch settings when the backend changes them (e.g. language
          // reset during model switch). The backend is the source of truth.
          const unlistenModelState = await listen("model-state-changed", () => {
            get().refreshSettings();
          });
          const unlistenSettingsChanged = await listen<{
            setting?: string;
          }>("settings-changed", (event) => {
            get().refreshSettings();
            if (event.payload.setting === "selected_microphone") {
              get().refreshAudioDevices();
            }
          });
          initializeUnlisteners.push(
            unlistenModelState,
            unlistenSettingsChanged,
          );
        })();
      }
      return initializePromise;
    },
  })),
);

// AUD-10: the lock probe the update gate (updateCheckGate) awaits. Routed
// through the store action so the very first caller starts the probe and
// every later caller shares it; never null, so the gate always holds on the
// real in-flight promise rather than deciding against a missing one.
export function getUpdateChecksLockedProbe(): Promise<void> {
  return useSettingsStore.getState().loadUpdateChecksLocked();
}
