import React from "react";
import { useTranslation } from "react-i18next";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";

interface MemoryPressureGuardProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const MemoryPressureGuard: React.FC<MemoryPressureGuardProps> =
  React.memo(({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();

    const guardEnabled = getSetting("memory_pressure_guard") ?? true;

    return (
      <ToggleSwitch
        checked={guardEnabled}
        onChange={(enabled) => updateSetting("memory_pressure_guard", enabled)}
        isUpdating={isUpdating("memory_pressure_guard")}
        label={t("settings.advanced.memoryPressureGuard.label")}
        description={t("settings.advanced.memoryPressureGuard.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
        tooltipPosition="bottom"
      />
    );
  });
