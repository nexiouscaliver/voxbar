import React, { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { useSettings } from "../../hooks/useSettings";
import { useSettingsStore } from "../../stores/settingsStore";
import i18n from "../../i18n";
import { commands, type ModelUnloadTimeout } from "@/bindings";
import {
  resolveUnloadTimeoutFocusGate,
  resolveUnloadTimeoutOutcome,
} from "./modelUnloadTimeoutFlow";
import { Dropdown } from "../ui/Dropdown";
import { SettingContainer } from "../ui/SettingContainer";

interface ModelUnloadTimeoutProps {
  descriptionMode?: "tooltip" | "inline";
  grouped?: boolean;
}

const CUSTOM_MIN_SECONDS = 5;
const CUSTOM_MAX_SECONDS = 86400;

const isCustom = (
  value: ModelUnloadTimeout | undefined,
): value is { custom: { seconds: number } } =>
  typeof value === "object" && value !== null && "custom" in value;

/** Dropdown values are always plain strings (the custom variant is entered
 * through the numeric field, never picked as an object). */
type TimeoutOptionValue = Exclude<ModelUnloadTimeout, object> | "custom";

function clampCustomSeconds(seconds: number): number {
  return Math.min(
    Math.max(Math.trunc(seconds) || 0, CUSTOM_MIN_SECONDS),
    CUSTOM_MAX_SECONDS,
  );
}

export const ModelUnloadTimeoutSetting: React.FC<ModelUnloadTimeoutProps> = ({
  descriptionMode = "inline",
  grouped = false,
}) => {
  const { t } = useTranslation();
  const { settings, getSetting, updateSetting } = useSettings();
  const customInputRef = useRef<HTMLInputElement>(null);

  const storedValue = getSetting("model_unload_timeout");
  const customActive = isCustom(storedValue);
  const customSeconds = customActive
    ? clampCustomSeconds(storedValue.custom.seconds)
    : CUSTOM_MIN_SECONDS;
  // Local echo of the input while editing, so an in-flight or out-of-range
  // keystroke can be shown without persisting it.
  const [customInput, setCustomInput] = useState<string | null>(null);

  // Tray "Unload After → Custom…": the backend emits its event after
  // revealing the window; App.tsx re-dispatches it as this window event once
  // the Advanced section has mounted. Focus + select + reveal the field.
  // KB-185: the event can arrive while a preset is still stored, and the
  // field only renders in custom mode - switch the stored setting first
  // (same commit path as handleChange: backend command + optimistic store
  // update, seeding 90s like the dropdown does), or the focus below lands
  // on a null ref and the tray item is a silent no-op. updateSetting sets
  // the store synchronously, so the field has rendered by the time the
  // await resolves and the ref is attached. getSetting/updateSetting are
  // stable store actions reading live state, so capturing the first
  // render's copies is safe here.
  // AUD-06: two ways this used to lie. (1) The command rejects by
  // resolving {status: "error"} rather than throwing, and the error was
  // ignored - the optimistic 90s survived in a store the backend never
  // accepted. resolveUnloadTimeoutOutcome fails that closed (keep the
  // previous value, surface the standard console.error + toast path).
  // (2) On a cold window the event can beat the store's hydration:
  // getSetting() returns undefined, which is not "a preset is stored",
  // and an optimistic updateSetting leaves settings null so the field
  // never renders and the focus silently no-ops even though the backend
  // committed. resolveUnloadTimeoutFocusGate defers until the shared
  // initialize() promise resolves (refreshSettings lands the
  // backend-committed value), then re-evaluates so focus still happens.
  useEffect(() => {
    const focusField = async () => {
      const liveStore = () => useSettingsStore.getState();
      let gate = resolveUnloadTimeoutFocusGate(
        getSetting("model_unload_timeout"),
        liveStore().settings !== null,
      );
      if (gate.action === "defer") {
        try {
          await liveStore().initialize();
        } catch (error) {
          // Hydration failed: stay deferred (no backend write we cannot
          // reflect in the UI) and still attempt the focus below.
          console.error("Failed to hydrate settings before focus:", error);
        }
        gate = resolveUnloadTimeoutFocusGate(
          getSetting("model_unload_timeout"),
          liveStore().settings !== null,
        );
      }
      if (gate.action === "switch") {
        const prev = getSetting("model_unload_timeout");
        try {
          const result = await commands.setModelUnloadTimeoutCustomSeconds(90);
          const outcome = resolveUnloadTimeoutOutcome(result, 90, prev);
          if (outcome.apply) {
            await updateSetting("model_unload_timeout", outcome.value);
          } else {
            console.error(
              "Failed to switch to custom unload timeout:",
              outcome.error,
            );
            toast.error(
              i18n.t("toast.settingNotSaved", { error: outcome.error }),
            );
          }
        } catch (error) {
          console.error("Failed to switch to custom unload timeout:", error);
        }
      }
      customInputRef.current?.focus();
      customInputRef.current?.select();
      customInputRef.current?.scrollIntoView({
        block: "center",
        behavior: "smooth",
      });
    };
    window.addEventListener("voxbar:focus-unload-timeout", focusField);
    return () =>
      window.removeEventListener("voxbar:focus-unload-timeout", focusField);
  }, []);

  const timeoutOptions = [
    {
      value: "never" as TimeoutOptionValue,
      label: t("settings.advanced.modelUnload.options.never"),
    },
    {
      value: "immediately" as TimeoutOptionValue,
      label: t("settings.advanced.modelUnload.options.immediately"),
    },
    {
      value: "min2" as TimeoutOptionValue,
      label: t("settings.advanced.modelUnload.options.min2"),
    },
    {
      value: "min5" as TimeoutOptionValue,
      label: t("settings.advanced.modelUnload.options.min5"),
    },
    {
      value: "min10" as TimeoutOptionValue,
      label: t("settings.advanced.modelUnload.options.min10"),
    },
    {
      value: "min15" as TimeoutOptionValue,
      label: t("settings.advanced.modelUnload.options.min15"),
    },
    {
      value: "hour1" as TimeoutOptionValue,
      label: t("settings.advanced.modelUnload.options.hour1"),
    },
    {
      value: "custom" as TimeoutOptionValue,
      label: t("settings.advanced.modelUnload.options.custom"),
    },
  ];

  // Debug mode keeps its 15s preset; Custom stays last in both lists.
  const debugTimeoutOptions = [
    ...timeoutOptions.slice(0, -1),
    {
      value: "sec15" as TimeoutOptionValue,
      label: t("settings.advanced.modelUnload.options.sec15"),
    },
    timeoutOptions[timeoutOptions.length - 1],
  ];

  const persistCustomSeconds = async (seconds: number) => {
    try {
      const prev = getSetting("model_unload_timeout");
      const result = await commands.setModelUnloadTimeoutCustomSeconds(seconds);
      // AUD-06: a backend rejection resolves (it does not throw); applying
      // the optimistic seconds anyway would leave the field showing a value
      // the backend rejected. Fail closed like updateSetting's
      // mapCommandResult standard: keep the previous value, say so.
      const outcome = resolveUnloadTimeoutOutcome(result, seconds, prev);
      if (outcome.apply) {
        await updateSetting("model_unload_timeout", outcome.value);
      } else {
        console.error("Failed to update custom unload timeout:", outcome.error);
        toast.error(i18n.t("toast.settingNotSaved", { error: outcome.error }));
      }
    } catch (error) {
      console.error("Failed to update custom unload timeout:", error);
    }
  };

  const handleCustomChange = async (raw: string) => {
    setCustomInput(raw);
    const parsed = Number.parseInt(raw, 10);
    if (Number.isNaN(parsed)) {
      return; // still editing; nothing persists until a valid number lands
    }
    const clamped = clampCustomSeconds(parsed);
    if (clamped === customSeconds) {
      return; // no effective change (e.g. clamped back to the current value)
    }
    await persistCustomSeconds(clamped);
  };

  const handleCustomBlur = () => {
    // Snap the field back to the persisted (already-clamped) value.
    setCustomInput(null);
  };

  const handleChange = async (
    event: React.ChangeEvent<HTMLSelectElement> | { target: { value: string } },
  ) => {
    const newTimeout = event.target.value;
    if (newTimeout === "custom") {
      // Entering custom mode: seed a valid custom value immediately (the
      // previous custom one when there is one, else 90s); the numeric field
      // refines it from there.
      if (!customActive) {
        await persistCustomSeconds(90);
      }
      setCustomInput(null);
      return;
    }
    try {
      await commands.setModelUnloadTimeout(newTimeout as ModelUnloadTimeout);
      updateSetting("model_unload_timeout", newTimeout as ModelUnloadTimeout);
    } catch (error) {
      console.error("Failed to update model unload timeout:", error);
    }
  };

  const storedValueForDropdown = (
    customActive ? "custom" : ((storedValue as string | undefined) ?? "never")
  ) as TimeoutOptionValue;

  const options = useMemo(() => {
    return settings?.debug_mode === true ? debugTimeoutOptions : timeoutOptions;
  }, [settings, t]);

  return (
    <SettingContainer
      title={t("settings.advanced.modelUnload.title")}
      description={t("settings.advanced.modelUnload.description")}
      descriptionMode={descriptionMode}
      grouped={grouped}
    >
      <div className="flex flex-col gap-2 items-end">
        <Dropdown
          options={options}
          selectedValue={storedValueForDropdown}
          onSelect={(value) => handleChange({ target: { value } })}
          disabled={false}
        />
        {customActive && (
          <div className="flex items-center gap-2">
            <label
              className="text-xs text-text/70"
              htmlFor="model-unload-custom-seconds"
            >
              {t("settings.advanced.modelUnload.customSecondsLabel")}
            </label>
            <input
              id="model-unload-custom-seconds"
              ref={customInputRef}
              type="number"
              className="w-24 rounded-md border border-mid-gray/30 bg-background-ui/40 px-2 py-1 text-sm text-text focus:border-logo-primary focus:outline-none"
              min={CUSTOM_MIN_SECONDS}
              max={CUSTOM_MAX_SECONDS}
              step={1}
              value={customInput ?? String(customSeconds)}
              onChange={(e) => handleCustomChange(e.target.value)}
              onBlur={handleCustomBlur}
            />
            <span className="text-xs text-text/70">
              {t("settings.advanced.modelUnload.customSecondsUnit")}
            </span>
          </div>
        )}
      </div>
    </SettingContainer>
  );
};
