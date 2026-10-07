import React from "react";
import { useTranslation } from "react-i18next";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";

interface SpokenPunctuationProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const SpokenPunctuation: React.FC<SpokenPunctuationProps> = React.memo(
  ({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();
    const enabled = getSetting("spoken_punctuation") ?? true;

    return (
      <ToggleSwitch
        checked={enabled}
        onChange={(nextEnabled) =>
          updateSetting("spoken_punctuation", nextEnabled)
        }
        isUpdating={isUpdating("spoken_punctuation")}
        label={t("settings.advanced.spokenPunctuation.title")}
        description={t("settings.advanced.spokenPunctuation.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      />
    );
  },
);
