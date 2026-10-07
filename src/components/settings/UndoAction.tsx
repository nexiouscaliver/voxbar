import React from "react";
import { useTranslation } from "react-i18next";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";

interface UndoActionProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const UndoAction: React.FC<UndoActionProps> = React.memo(
  ({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();
    const enabled = getSetting("undo_enabled") ?? true;

    return (
      <ToggleSwitch
        checked={enabled}
        onChange={(nextEnabled) => updateSetting("undo_enabled", nextEnabled)}
        isUpdating={isUpdating("undo_enabled")}
        label={t("settings.general.editingShortcuts.undo.title")}
        description={t("settings.general.editingShortcuts.undo.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      />
    );
  },
);
