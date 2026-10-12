import React from "react";
import { useTranslation } from "react-i18next";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";

interface ExperimentalToggleProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
  /** Host-level hook around the commit (e.g. revealing the Experimental
   * group into view when it turns on); defaults to the plain write. */
  onChange?: (enabled: boolean) => void;
}

export const ExperimentalToggle: React.FC<ExperimentalToggleProps> = React.memo(
  ({
    descriptionMode = "tooltip",
    grouped = false,
    onChange,
  }: ExperimentalToggleProps) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();

    const enabled = getSetting("experimental_enabled") || false;

    return (
      <ToggleSwitch
        checked={enabled}
        onChange={
          onChange ??
          ((value: boolean) => updateSetting("experimental_enabled", value))
        }
        isUpdating={isUpdating("experimental_enabled")}
        label={t("settings.advanced.experimentalToggle.label")}
        description={t("settings.advanced.experimentalToggle.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      />
    );
  },
);
