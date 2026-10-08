import React from "react";
import { useTranslation } from "react-i18next";
import { Dropdown } from "../ui/Dropdown";
import { SettingContainer } from "../ui/SettingContainer";
import { useSettings } from "../../hooks/useSettings";
import type { NumberFormat } from "@/bindings";

interface NumberFormatProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const NumberFormatSetting: React.FC<NumberFormatProps> = React.memo(
  ({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();

    const formatOptions = [
      {
        value: "digits",
        label: t("settings.advanced.numberFormat.options.digits"),
      },
      {
        value: "smart",
        label: t("settings.advanced.numberFormat.options.smart"),
      },
      {
        value: "as_transcribed",
        label: t("settings.advanced.numberFormat.options.asTranscribed"),
      },
    ];

    const selectedFormat = (getSetting("number_format") ||
      "digits") as NumberFormat;

    return (
      <SettingContainer
        title={t("settings.advanced.numberFormat.title")}
        description={t("settings.advanced.numberFormat.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      >
        <Dropdown
          options={formatOptions}
          selectedValue={selectedFormat}
          onSelect={(value) =>
            updateSetting("number_format", value as NumberFormat)
          }
          disabled={isUpdating("number_format")}
        />
      </SettingContainer>
    );
  },
);
