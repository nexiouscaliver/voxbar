import React from "react";
import { useTranslation } from "react-i18next";
import { SettingContainer } from "./SettingContainer";
import { ResetButton } from "./ResetButton";
import { RANGE_INPUT_CLASS } from "./rangeSlider";

interface SliderProps {
  value: number;
  onChange: (value: number) => void;
  min: number;
  max: number;
  step?: number;
  disabled?: boolean;
  label: string;
  description: string;
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
  showValue?: boolean;
  formatValue?: (value: number) => string;
  onReset?: () => void;
  isResetting?: boolean;
  /** The setting's default. While the value sits on it there is nothing
   * to restore, so the reset arrow stays hidden instead of rendering as a
   * dead control on every slider row. */
  defaultValue?: number;
}

export const Slider: React.FC<SliderProps> = ({
  value,
  onChange,
  min,
  max,
  step = 0.01,
  disabled = false,
  label,
  description,
  descriptionMode = "tooltip",
  grouped = false,
  showValue = true,
  formatValue = (v) => v.toFixed(2),
  onReset,
  isResetting = false,
  defaultValue,
}) => {
  const { t } = useTranslation();
  const handleChange = (e: React.ChangeEvent<HTMLInputElement>) => {
    onChange(parseFloat(e.target.value));
  };

  return (
    <SettingContainer
      title={label}
      description={description}
      descriptionMode={descriptionMode}
      grouped={grouped}
      layout="horizontal"
      disabled={disabled}
    >
      <div className="w-full">
        <div className="flex items-center gap-2 h-6">
          <input
            type="range"
            min={min}
            max={max}
            step={step}
            value={value}
            onChange={handleChange}
            disabled={disabled}
            className={`flex-grow ${RANGE_INPUT_CLASS}`}
            style={{
              background: `linear-gradient(to right, var(--color-background-ui) ${
                ((value - min) / (max - min)) * 100
              }%, rgba(128, 128, 128, 0.2) ${
                ((value - min) / (max - min)) * 100
              }%)`,
            }}
          />
          {showValue && (
            // Content-fitting width: a fixed w-12 clipped longer localized
            // labels (it-IT "Disattivato") under the reset icon. The value
            // now grows with its text (min-w-12 floor keeps numbers aligned)
            // and never wraps or collides with the reset button.
            <span
              className="min-w-12 max-w-32 shrink-0 truncate text-end text-sm font-medium text-text/90"
              title={formatValue(value)}
            >
              {formatValue(value)}
            </span>
          )}
          {/* Reset exists only while the value actually differs from its
              default (and the row is enabled); hidden otherwise so the
              arrow never renders as a dead control. */}
          {onReset &&
            !disabled &&
            defaultValue !== undefined &&
            value !== defaultValue && (
              <ResetButton
                onClick={onReset}
                disabled={isResetting}
                ariaLabel={t("accessibility.resetSetting", { label })}
              />
            )}
        </div>
      </div>
    </SettingContainer>
  );
};
