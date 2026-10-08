import React, { useState } from "react";
import { useTranslation } from "react-i18next";
import { useSettings } from "../../hooks/useSettings";
import { Dropdown } from "../ui/Dropdown";
import { SettingContainer } from "../ui/SettingContainer";
import { marginPresets, parseMarginMb } from "./memoryMarginInput";

interface MemoryHeadroomProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

/** The value seeded when the user switches from a preset to Custom: a
 * clearly-custom round number above the largest preset. */
const CUSTOM_SEED_MB = 2048;

export const MemoryHeadroom: React.FC<MemoryHeadroomProps> = ({
  descriptionMode = "tooltip",
  grouped = false,
}) => {
  const { t } = useTranslation();
  const { getSetting, updateSetting } = useSettings();

  const storedValue = getSetting("memory_gate_headroom_mb") ?? 0;
  const isPreset = marginPresets.includes(storedValue);
  // Local echo of the custom input while editing, so an invalid keystroke
  // can be shown (with the rejection message) without persisting it.
  const [customInput, setCustomInput] = useState<string | null>(null);

  const options = [
    {
      value: "0",
      label: t("settings.advanced.memoryHeadroom.options.off"),
    },
    {
      value: "256",
      label: t("settings.advanced.memoryHeadroom.options.mb256"),
    },
    {
      value: "512",
      label: t("settings.advanced.memoryHeadroom.options.mb512"),
    },
    {
      value: "1024",
      label: t("settings.advanced.memoryHeadroom.options.mb1024"),
    },
    {
      value: "1536",
      label: t("settings.advanced.memoryHeadroom.options.mb1536"),
    },
    {
      value: "custom",
      label: t("settings.advanced.memoryHeadroom.options.custom"),
    },
  ];

  const handleSelect = async (value: string) => {
    setCustomInput(null);
    if (value === "custom") {
      if (!isPreset) {
        return; // already custom; the numeric field refines it
      }
      await updateSetting("memory_gate_headroom_mb", CUSTOM_SEED_MB);
      return;
    }
    const preset = Number.parseInt(value, 10);
    if (preset !== storedValue) {
      await updateSetting("memory_gate_headroom_mb", preset);
    }
  };

  const handleCustomChange = async (raw: string) => {
    setCustomInput(raw);
    const parsed = parseMarginMb(raw);
    if (!parsed.ok) {
      return; // the rejection message renders; nothing persists
    }
    if (parsed.valueMb === storedValue) {
      return; // no effective change
    }
    await updateSetting("memory_gate_headroom_mb", parsed.valueMb);
  };

  const handleCustomBlur = () => {
    // Snap the field back to the persisted value (invalid input is
    // discarded, never clamped or zeroed into the store).
    setCustomInput(null);
  };

  const showInvalid =
    customInput !== null &&
    customInput.trim() !== "" &&
    !parseMarginMb(customInput).ok;

  return (
    <SettingContainer
      title={t("settings.advanced.memoryHeadroom.label")}
      description={t("settings.advanced.memoryHeadroom.description")}
      descriptionMode={descriptionMode}
      grouped={grouped}
    >
      <div className="flex flex-col gap-2 items-end">
        <Dropdown
          options={options}
          selectedValue={isPreset ? String(storedValue) : "custom"}
          onSelect={handleSelect}
          disabled={false}
        />
        {!isPreset && (
          <div className="flex flex-col items-end gap-1">
            <div className="flex items-center gap-2">
              <label
                className="text-xs text-text/70"
                htmlFor="memory-headroom-custom-mb"
              >
                {t("settings.advanced.memoryHeadroom.customLabel")}
              </label>
              <input
                id="memory-headroom-custom-mb"
                type="text"
                inputMode="numeric"
                className="w-24 rounded-md border border-mid-gray/30 bg-background-ui/40 px-2 py-1 text-sm text-text focus:border-logo-primary focus:outline-none"
                value={customInput ?? String(storedValue)}
                onChange={(e) => handleCustomChange(e.target.value)}
                onBlur={handleCustomBlur}
              />
              <span className="text-xs text-text/70">
                {t("settings.advanced.memoryHeadroom.customUnit")}
              </span>
            </div>
            {showInvalid && (
              <span className="text-xs text-red-400">
                {t("settings.advanced.memoryHeadroom.invalid")}
              </span>
            )}
          </div>
        )}
      </div>
    </SettingContainer>
  );
};
