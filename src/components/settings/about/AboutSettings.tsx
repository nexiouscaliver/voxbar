import React, { useState, useEffect } from "react";
import { useTranslation } from "react-i18next";
import { platform } from "@tauri-apps/plugin-os";
import { openUrl } from "@tauri-apps/plugin-opener";
import { SettingsGroup } from "../../ui/SettingsGroup";
import { SettingContainer } from "../../ui/SettingContainer";
import { Button } from "../../ui/Button";
import { AppDataDirectory } from "../AppDataDirectory";
import { AppLanguageSelector } from "../AppLanguageSelector";
import { ShowWhatsNewOnUpdate } from "../ShowWhatsNewOnUpdate";
import { UpdateChecksToggle } from "../UpdateChecksToggle";
import { UpdatePolicySetting } from "../UpdatePolicy";
import { ThemeSelector } from "../ThemeSelector";
import { AccentColorSelector } from "../AccentColorSelector";
import { LogDirectory } from "../debug";
import { runUpdateCheck } from "../../update-checker/updaterFlow";
import { updaterAutoUpdateSupported } from "../../update-checker/updaterPlatform";
import { fetchAppVersion } from "../../../lib/utils/appVersion";
import { useSettings } from "../../../hooks/useSettings";

export const AboutSettings: React.FC = () => {
  const { t } = useTranslation();
  const { settings, updateChecksLocked } = useSettings();
  const [version, setVersion] = useState("");

  // Mirrors updaterFlow's updateChecksAllowed() exactly, fail closed while
  // the lock state is unknown (KB-033): the button must never offer a check
  // the flow would silently drop, so it stays disabled until checks are
  // known-allowed. The tooltip reuses the locked/disabled copy the sibling
  // update rows already show.
  const updateChecksAllowed =
    updateChecksLocked === false && (settings?.update_checks_enabled ?? true);

  useEffect(() => {
    let cancelled = false;
    void fetchAppVersion().then((appVersion) => {
      if (!cancelled) setVersion(appVersion);
    });
    return () => {
      cancelled = true;
    };
  }, []);

  return (
    <div className="max-w-3xl w-full mx-auto space-y-6">
      <SettingsGroup title={t("settings.about.title")}>
        <AppLanguageSelector descriptionMode="tooltip" grouped={true} />
        <ThemeSelector descriptionMode="tooltip" grouped={true} />
        <AccentColorSelector descriptionMode="tooltip" grouped={true} />
        <SettingContainer
          title={t("settings.about.version.title")}
          description={t("settings.about.version.description")}
          grouped={true}
        >
          <div className="flex items-center gap-3">
            {/* KB-193: state starts empty and the IPC roundtrip takes a
                frame, so an ungated span would flash a bare "v"; render the
                version only once it has actually loaded. */}
            {version !== "" && (
              /* eslint-disable-next-line i18next/no-literal-string */
              <span className="text-sm font-mono">v{version}</span>
            )}
            {updaterAutoUpdateSupported(platform()) ? (
              <Button
                variant="secondary"
                size="sm"
                disabled={!updateChecksAllowed}
                title={
                  updateChecksAllowed
                    ? undefined
                    : t(
                        updateChecksLocked === true
                          ? "settings.debug.updateChecks.lockedDescription"
                          : "footer.updateCheckingDisabled",
                      )
                }
                onClick={() => void runUpdateCheck({ trigger: "manual" })}
              >
                {t("footer.checkForUpdates")}
              </Button>
            ) : (
              // Platforms without shipped updater artifacts: a check here
              // can only error, so point at the releases page instead.
              <Button
                variant="secondary"
                size="sm"
                onClick={() =>
                  openUrl("https://github.com/nexiouscaliver/voxbar/releases")
                }
              >
                {t("footer.updater.manualOnlyPlatform")}
              </Button>
            )}
          </div>
        </SettingContainer>

        <ShowWhatsNewOnUpdate descriptionMode="tooltip" grouped={true} />
        <UpdateChecksToggle descriptionMode="tooltip" grouped={true} />
        <UpdatePolicySetting descriptionMode="tooltip" grouped={true} />

        <SettingContainer
          title={t("settings.about.sourceCode.title")}
          description={t("settings.about.sourceCode.description")}
          grouped={true}
        >
          <Button
            variant="secondary"
            size="md"
            onClick={() => openUrl("https://github.com/nexiouscaliver/voxbar")}
          >
            {t("settings.about.sourceCode.button")}
          </Button>
        </SettingContainer>

        <AppDataDirectory descriptionMode="tooltip" grouped={true} />
        <LogDirectory grouped={true} />
      </SettingsGroup>

      <SettingsGroup title={t("settings.about.acknowledgments.title")}>
        <SettingContainer
          title={t("settings.about.acknowledgments.ggml.title")}
          description={t("settings.about.acknowledgments.ggml.description")}
          grouped={true}
          layout="stacked"
        >
          <div className="text-sm text-secondary">
            {t("settings.about.acknowledgments.ggml.details")}
          </div>
        </SettingContainer>

        <SettingContainer
          title="Attribution"
          description="Upstream attribution"
          grouped={true}
          layout="stacked"
        >
          {/* eslint-disable i18next/no-literal-string -- proper nouns and an
              upstream credit link; attribution, not navigation, so the link
              stays pointed at the upstream project on purpose. */}
          <div className="text-sm text-secondary">
            VoxBar is a fork of{" "}
            <a
              href="https://github.com/cjpais/Handy"
              onClick={(e) => {
                e.preventDefault();
                openUrl("https://github.com/cjpais/Handy");
              }}
              className="underline"
            >
              Handy
            </a>{" "}
            by CJ Pais (MIT)
          </div>
          {/* eslint-enable i18next/no-literal-string */}
        </SettingContainer>
      </SettingsGroup>
    </div>
  );
};
