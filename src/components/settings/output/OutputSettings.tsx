import React from "react";
import { useTranslation } from "react-i18next";
import { SettingsGroup } from "../../ui/SettingsGroup";
import { PasteMethodSetting } from "../PasteMethod";
import { TypingToolSetting } from "../TypingTool";
import { ClipboardHandlingSetting } from "../ClipboardHandling";
import { AutoSubmit } from "../AutoSubmit";
import { PreviewBeforePaste } from "../PreviewBeforePaste";
import { AppendTrailingSpace } from "../AppendTrailingSpace";
import { ReliablePasteToggle } from "../debug/ReliablePaste";
import { PasteDelay } from "../debug/PasteDelay";
import { HoldThreshold } from "../debug/HoldThreshold";
import { RecordingBuffer } from "../debug/RecordingBuffer";
import { StreamingReleaseTail } from "../debug/StreamingReleaseTail";

/**
 * Everything about how a finished transcription leaves the app: the paste
 * pipeline first, then the controls that shape the recording window itself.
 * Both paste delay rows render here on purpose: they are intentionally
 * distinct settings (delay before, delay after) that happen to look alike.
 */
export const OutputSettings: React.FC = () => {
  const { t } = useTranslation();

  return (
    <div className="max-w-3xl w-full mx-auto space-y-6">
      <SettingsGroup title={t("settings.output.groups.paste")}>
        <PasteMethodSetting descriptionMode="tooltip" grouped={true} />
        <TypingToolSetting descriptionMode="tooltip" grouped={true} />
        <ClipboardHandlingSetting descriptionMode="tooltip" grouped={true} />
        <AutoSubmit descriptionMode="tooltip" grouped={true} />
        <PreviewBeforePaste descriptionMode="tooltip" grouped={true} />
        <AppendTrailingSpace descriptionMode="tooltip" grouped={true} />
        <ReliablePasteToggle descriptionMode="tooltip" grouped={true} />
        <PasteDelay descriptionMode="tooltip" grouped={true} />
        <PasteDelay
          descriptionMode="tooltip"
          grouped={true}
          settingKey="paste_delay_after_ms"
          labelKey="settings.debug.pasteDelayAfter.title"
          descriptionKey="settings.debug.pasteDelayAfter.description"
        />
      </SettingsGroup>

      <SettingsGroup title={t("settings.output.groups.recording")}>
        <HoldThreshold descriptionMode="tooltip" grouped={true} />
        <RecordingBuffer descriptionMode="tooltip" grouped={true} />
        <StreamingReleaseTail descriptionMode="tooltip" grouped={true} />
      </SettingsGroup>
    </div>
  );
};
