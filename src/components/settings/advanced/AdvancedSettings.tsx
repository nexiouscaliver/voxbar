import React from "react";
import { useTranslation } from "react-i18next";
import { ModelUnloadTimeoutSetting } from "../ModelUnloadTimeout";
import { MemoryPressureGuard } from "../MemoryPressureGuard";
import { MemoryHeadroom } from "../MemoryHeadroom";
import { AutoFallback } from "../AutoFallback";
import { MenuBarModelTitle } from "../MenuBarModelTitle";
import { CustomWords } from "../CustomWords";
import { SettingsGroup } from "../../ui/SettingsGroup";
import { PostProcessingToggle } from "../PostProcessingToggle";
import { useSettings } from "../../../hooks/useSettings";
import { KeyboardImplementationSelector } from "../debug/KeyboardImplementationSelector";
import { VoiceActivityDetection } from "../VoiceActivityDetection";
import { AccelerationSelector } from "../AccelerationSelector";
import { LazyStreamClose } from "../LazyStreamClose";
import { FillerWordRemoval } from "../FillerWordRemoval";
import { WordCorrectionThreshold } from "../debug/WordCorrectionThreshold";
import { ChineseScriptSetting } from "../ChineseScript";
import { VadBackendSelector } from "../VadBackendSelector";
import { ExperimentalToggle } from "../ExperimentalToggle";

/**
 * Plumbing only: model residency and memory, the transcription passes that
 * rewrite text, and the single gated Experimental group. The model/memory
 * group stays always visible, and with it the ExperimentalToggle, because
 * experimental_enabled defaults to false: a master toggle hidden behind its
 * own flag would be unreachable for every default-config user. The unload
 * timeout also stays under this section id: the tray's "Unload After" ->
 * "Custom..." flow opens the advanced section and focuses its field.
 */
export const AdvancedSettings: React.FC = () => {
  const { t } = useTranslation();
  const { getSetting } = useSettings();
  const experimentalEnabled = getSetting("experimental_enabled") || false;

  return (
    <div className="max-w-3xl w-full mx-auto space-y-6">
      <SettingsGroup title={t("settings.advanced.groups.model")}>
        <ModelUnloadTimeoutSetting descriptionMode="tooltip" grouped={true} />
        <MemoryPressureGuard descriptionMode="tooltip" grouped={true} />
        <MemoryHeadroom descriptionMode="tooltip" grouped={true} />
        <AutoFallback descriptionMode="tooltip" grouped={true} />
        <MenuBarModelTitle descriptionMode="tooltip" grouped={true} />
        <ExperimentalToggle descriptionMode="tooltip" grouped={true} />
      </SettingsGroup>

      <SettingsGroup title={t("settings.advanced.groups.transcription")}>
        <VoiceActivityDetection descriptionMode="tooltip" grouped={true} />
        <FillerWordRemoval descriptionMode="tooltip" grouped={true} />
        <ChineseScriptSetting descriptionMode="tooltip" grouped={true} />
        <CustomWords descriptionMode="tooltip" grouped />
        <WordCorrectionThreshold descriptionMode="tooltip" grouped={true} />
      </SettingsGroup>

      {experimentalEnabled && (
        <SettingsGroup title={t("settings.advanced.groups.experimental")}>
          <PostProcessingToggle descriptionMode="tooltip" grouped={true} />
          <KeyboardImplementationSelector
            descriptionMode="tooltip"
            grouped={true}
          />
          <AccelerationSelector descriptionMode="tooltip" grouped={true} />
          <LazyStreamClose descriptionMode="tooltip" grouped={true} />
          <VadBackendSelector descriptionMode="tooltip" grouped={true} />
        </SettingsGroup>
      )}
    </div>
  );
};
