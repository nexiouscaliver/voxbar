import React from "react";
import { useTranslation } from "react-i18next";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";

interface MenuBarModelTitleProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const MenuBarModelTitle: React.FC<MenuBarModelTitleProps> = React.memo(
  ({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();

    const titleEnabled = getSetting("menu_bar_model_title") ?? true;

    return (
      <ToggleSwitch
        checked={titleEnabled}
        onChange={(enabled) => updateSetting("menu_bar_model_title", enabled)}
        isUpdating={isUpdating("menu_bar_model_title")}
        label={t("settings.advanced.menuBarModelTitle.label")}
        description={t("settings.advanced.menuBarModelTitle.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
        tooltipPosition="bottom"
      />
    );
  },
);
