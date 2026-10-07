import React, { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { useSettings } from "../../hooks/useSettings";
import { commands, type ModelUnloadTimeout } from "@/bindings";
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
  useEffect(() => {
    const focusField = () => {
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
      await commands.setModelUnloadTimeoutCustomSeconds(seconds);
      updateSetting("model_unload_timeout", {
        custom: { seconds },
      });
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
