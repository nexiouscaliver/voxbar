import React from "react";
import { useTranslation } from "react-i18next";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";

interface CommandModeToggleProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const CommandModeToggle: React.FC<CommandModeToggleProps> = React.memo(
  ({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();
    const enabled = getSetting("command_mode_enabled") ?? true;

    return (
      <ToggleSwitch
        checked={enabled}
        onChange={(nextEnabled) =>
          updateSetting("command_mode_enabled", nextEnabled)
        }
        isUpdating={isUpdating("command_mode_enabled")}
        label={t("settings.general.commandMode.toggle.title")}
        description={t("settings.general.commandMode.toggle.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      />
    );
  },
);
