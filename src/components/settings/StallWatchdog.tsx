import React from "react";
import { useTranslation } from "react-i18next";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";

interface StallWatchdogProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const StallWatchdog: React.FC<StallWatchdogProps> = React.memo(
  ({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();

    const enabled = getSetting("stall_watchdog_enabled") ?? true;

    return (
      <ToggleSwitch
        checked={enabled}
        onChange={(value) => updateSetting("stall_watchdog_enabled", value)}
        isUpdating={isUpdating("stall_watchdog_enabled")}
        label={t("settings.advanced.stallWatchdog.label")}
        description={t("settings.advanced.stallWatchdog.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
        tooltipPosition="bottom"
      />
    );
  },
);
