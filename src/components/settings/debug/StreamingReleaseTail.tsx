import React from "react";
import { useTranslation } from "react-i18next";
import { Slider } from "../../ui/Slider";
import { useSettings } from "../../../hooks/useSettings";

interface StreamingReleaseTailProps {
  descriptionMode?: "tooltip" | "inline";
  grouped?: boolean;
}

/**
 * Post-release capture floor for STREAMING sessions: releasing the hotkey
 * the instant a spoken command word ends otherwise truncates its tail and
 * the command silently fails. Applies only when the recording ran with an
 * active stream (batch sessions use the extra recording buffer alone);
 * 0 restores the old no-tail behavior.
 */
export const StreamingReleaseTail: React.FC<StreamingReleaseTailProps> = ({
  descriptionMode = "tooltip",
  grouped = false,
}) => {
  const { t } = useTranslation();
  const { settings, updateSetting, resetSetting, isUpdating } = useSettings();

  const handleTailChange = (value: number) => {
    updateSetting("streaming_release_tail_ms", value);
  };

  return (
    <Slider
      value={settings?.streaming_release_tail_ms ?? 200}
      onChange={handleTailChange}
      onReset={() => resetSetting("streaming_release_tail_ms")}
      isResetting={isUpdating("streaming_release_tail_ms")}
      min={0}
      max={1000}
      step={50}
      label={t("settings.debug.streamingTail.title")}
      description={t("settings.debug.streamingTail.description")}
      descriptionMode={descriptionMode}
      grouped={grouped}
      formatValue={(v) => `${v}ms`}
    />
  );
};
