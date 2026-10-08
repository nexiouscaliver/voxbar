import React from "react";
import { useTranslation } from "react-i18next";
import { LogLevelSelector } from "./LogLevelSelector";
import { LiveLogViewer } from "./LiveLogViewer";
import { SettingsGroup } from "../../ui/SettingsGroup";
import { WhatsNewPreview } from "./WhatsNewPreview";
import { KeyboardDiagnostic } from "./KeyboardDiagnostic";
import {
  OnboardingPreview,
  type OnboardingPreviewStep,
} from "./OnboardingPreview";

interface DebugSettingsProps {
  onPreviewOnboarding?: (step: OnboardingPreviewStep) => void;
}

/**
 * Actual diagnostics only: log level, live log, preview hooks and the
 * keyboard event diagnostic. Every setting that used to hide here has moved
 * to the tab that owns the behavior it configures.
 */
export const DebugSettings: React.FC<DebugSettingsProps> = ({
  onPreviewOnboarding,
}) => {
  const { t } = useTranslation();

  return (
    <div className="max-w-3xl w-full mx-auto space-y-6">
      <SettingsGroup title={t("settings.debug.title")}>
        <LogLevelSelector grouped={true} />
        <WhatsNewPreview descriptionMode="tooltip" grouped={true} />
        {onPreviewOnboarding && (
          <OnboardingPreview
            onPreview={onPreviewOnboarding}
            descriptionMode="tooltip"
            grouped={true}
          />
        )}
        <KeyboardDiagnostic />
        <LiveLogViewer descriptionMode="tooltip" grouped={true} />
      </SettingsGroup>
    </div>
  );
};
