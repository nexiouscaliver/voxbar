import React from "react";
import { useTranslation } from "react-i18next";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";

interface VoiceDeletionCommandsProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const VoiceDeletionCommands: React.FC<VoiceDeletionCommandsProps> =
  React.memo(({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();
    const enabled = getSetting("voice_deletion_commands") ?? true;

    return (
      <ToggleSwitch
        checked={enabled}
        onChange={(nextEnabled) =>
          updateSetting("voice_deletion_commands", nextEnabled)
        }
        isUpdating={isUpdating("voice_deletion_commands")}
        label={t("settings.advanced.voiceDeletion.title")}
        description={t("settings.advanced.voiceDeletion.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      />
    );
  });
