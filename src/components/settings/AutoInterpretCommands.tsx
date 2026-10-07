import React from "react";
import { useTranslation } from "react-i18next";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";

interface AutoInterpretCommandsProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const AutoInterpretCommands: React.FC<AutoInterpretCommandsProps> =
  React.memo(({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();
    const enabled = getSetting("auto_interpret_commands") ?? true;

    return (
      <ToggleSwitch
        checked={enabled}
        onChange={(nextEnabled) =>
          updateSetting("auto_interpret_commands", nextEnabled)
        }
        isUpdating={isUpdating("auto_interpret_commands")}
        label={t("settings.advanced.autoInterpret.title")}
        description={t("settings.advanced.autoInterpret.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      />
    );
  });
