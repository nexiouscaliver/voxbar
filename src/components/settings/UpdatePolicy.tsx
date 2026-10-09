import React from "react";
import { useTranslation } from "react-i18next";
import { Dropdown } from "../ui/Dropdown";
import { SettingContainer } from "../ui/SettingContainer";
import { useSettings } from "../../hooks/useSettings";
import type { UpdatePolicy } from "@/bindings";

interface UpdatePolicyProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const UpdatePolicySetting: React.FC<UpdatePolicyProps> = React.memo(
  ({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating, updateChecksLocked } =
      useSettings();

    const policyOptions = [
      {
        value: "ask",
        label: t("settings.about.updatePolicy.options.ask"),
      },
      {
        value: "download",
        label: t("settings.about.updatePolicy.options.download"),
      },
      {
        value: "install",
        label: t("settings.about.updatePolicy.options.install"),
      },
    ];

    const selectedPolicy = (getSetting("update_policy") ||
      "ask") as UpdatePolicy;

    return (
      <SettingContainer
        title={t("settings.about.updatePolicy.title")}
        description={
          updateChecksLocked === true
            ? t("settings.debug.updateChecks.lockedDescription")
            : t("settings.about.updatePolicy.description")
        }
        descriptionMode={descriptionMode}
        grouped={grouped}
      >
        <Dropdown
          options={policyOptions}
          selectedValue={selectedPolicy}
          onSelect={(value) =>
            updateSetting("update_policy", value as UpdatePolicy)
          }
          disabled={isUpdating("update_policy") || updateChecksLocked === true}
        />
      </SettingContainer>
    );
  },
);
