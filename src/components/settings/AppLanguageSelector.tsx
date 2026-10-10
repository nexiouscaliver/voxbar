import React from "react";
import { useTranslation } from "react-i18next";
import { Dropdown } from "../ui/Dropdown";
import { SettingContainer } from "../ui/SettingContainer";
import {
  SUPPORTED_LANGUAGES,
  getSupportedLanguage,
  type SupportedLanguageCode,
} from "../../i18n";
import { useSettings } from "@/hooks/useSettings";
import { useSettingsStore } from "@/stores/settingsStore";

interface AppLanguageSelectorProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const AppLanguageSelector: React.FC<AppLanguageSelectorProps> =
  React.memo(({ descriptionMode = "tooltip", grouped = false }) => {
    const { t, i18n } = useTranslation();
    const { settings, updateSetting } = useSettings();

    const currentLanguage = (getSupportedLanguage(settings?.app_language) ||
      i18n.language) as SupportedLanguageCode;

    const languageOptions = SUPPORTED_LANGUAGES.map((lang) => ({
      value: lang.code,
      label: `${lang.nativeName} (${lang.name})`,
    }));

    const handleLanguageChange = async (langCode: string) => {
      // Persist before applying (KB-192, KB-103/104 family): the store
      // resolves updateSetting even when the backend write fails - it rolls
      // the optimistic value back and toasts - so success is read off the
      // post-write store state (the same getState pattern updaterFlow
      // uses). Only then does the webview switch languages; a failed
      // persist leaves it in the old language, matching the store and tray
      // instead of diverging from them until relaunch.
      await updateSetting("app_language", langCode);
      if (useSettingsStore.getState().settings?.app_language === langCode) {
        i18n.changeLanguage(langCode);
      }
    };

    return (
      <SettingContainer
        title={t("appLanguage.title")}
        description={t("appLanguage.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      >
        <Dropdown
          options={languageOptions}
          selectedValue={currentLanguage}
          onSelect={handleLanguageChange}
        />
      </SettingContainer>
    );
  });

AppLanguageSelector.displayName = "AppLanguageSelector";
