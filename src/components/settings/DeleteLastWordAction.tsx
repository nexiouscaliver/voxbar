import React from "react";
import { useTranslation } from "react-i18next";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";

interface DeleteLastWordActionProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const DeleteLastWordAction: React.FC<DeleteLastWordActionProps> =
  React.memo(({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();
    const enabled = getSetting("delete_last_word_enabled") ?? true;

    return (
      <ToggleSwitch
        checked={enabled}
        onChange={(nextEnabled) =>
          updateSetting("delete_last_word_enabled", nextEnabled)
        }
        isUpdating={isUpdating("delete_last_word_enabled")}
        label={t("settings.general.editingShortcuts.deleteLastWord.title")}
        description={t(
          "settings.general.editingShortcuts.deleteLastWord.description",
        )}
        descriptionMode={descriptionMode}
        grouped={grouped}
      />
    );
  });
