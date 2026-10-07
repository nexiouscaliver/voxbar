import React from "react";
import { useTranslation } from "react-i18next";
import { SettingContainer } from "../ui/SettingContainer";
import { useSettings } from "@/hooks/useSettings";
import { ACCENTS, applyAccent, DEFAULT_ACCENT_ID } from "@/lib/utils/accent";

interface AccentColorSelectorProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

/**
 * Fixed-swatch accent picker (deliberately not a color wheel). Every cell
 * carries a border so dark swatches stay visible on the dark theme and light
 * swatches on light. Selecting applies immediately — the CSS palette
 * override lands before the setting round-trips, so the UI recolors at once.
 */
export const AccentColorSelector: React.FC<AccentColorSelectorProps> =
  React.memo(({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { settings, updateSetting } = useSettings();

    const currentAccent = settings?.accent_color ?? DEFAULT_ACCENT_ID;

    const handleAccentChange = (id: string) => {
      if (id === currentAccent) return;
      applyAccent(id);
      updateSetting("accent_color", id);
    };

    return (
      <SettingContainer
        title={t("theme.accentColor.label")}
        description={t("theme.accentColor.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      >
        <div
          className="flex flex-wrap gap-2 justify-end max-w-[15rem]"
          role="radiogroup"
          aria-label={t("theme.accentColor.label")}
        >
          {ACCENTS.map((accent) => {
            const selected = accent.id === currentAccent;
            const name = t(`theme.accentColors.${accent.id}`);
            return (
              <button
                key={accent.id}
                type="button"
                role="radio"
                aria-checked={selected}
                title={name}
                aria-label={name}
                onClick={() => handleAccentChange(accent.id)}
                className={`w-6 h-6 rounded-full cursor-pointer transition-transform duration-100 focus:outline-none focus-visible:ring-2 focus-visible:ring-logo-primary focus-visible:ring-offset-1 focus-visible:ring-offset-background ${
                  selected
                    ? "border-2 border-text ring-1 ring-text ring-offset-1 ring-offset-background scale-110"
                    : "border border-mid-gray/50 hover:scale-110 hover:border-mid-gray"
                }`}
                style={{ backgroundColor: accent.swatch }}
              />
            );
          })}
        </div>
      </SettingContainer>
    );
  });

AccentColorSelector.displayName = "AccentColorSelector";
