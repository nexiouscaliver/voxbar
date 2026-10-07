import React from "react";
import { useTranslation } from "react-i18next";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";

interface AutoFallbackProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const AutoFallback: React.FC<AutoFallbackProps> = React.memo(
  ({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();

    const autoFallbackEnabled = getSetting("auto_fallback") ?? true;

    return (
      <ToggleSwitch
        checked={autoFallbackEnabled}
        onChange={(enabled) => updateSetting("auto_fallback", enabled)}
        isUpdating={isUpdating("auto_fallback")}
        label={t("settings.advanced.autoFallback.label")}
        description={t("settings.advanced.autoFallback.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
        tooltipPosition="bottom"
      />
    );
  },
);
