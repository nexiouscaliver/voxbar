import React from "react";
import { useTranslation } from "react-i18next";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";

interface TerminalPunctuationProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const TerminalPunctuation: React.FC<TerminalPunctuationProps> =
  React.memo(({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();
    const enabled = getSetting("terminal_punctuation") ?? true;

    return (
      <ToggleSwitch
        checked={enabled}
        onChange={(nextEnabled) =>
          updateSetting("terminal_punctuation", nextEnabled)
        }
        isUpdating={isUpdating("terminal_punctuation")}
        label={t("settings.advanced.terminalPunctuation.title")}
        description={t("settings.advanced.terminalPunctuation.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      />
    );
  });
