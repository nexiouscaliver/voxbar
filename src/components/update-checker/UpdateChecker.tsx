import React, { useEffect, useRef } from "react";
import { platform } from "@tauri-apps/plugin-os";
import { openUrl } from "@tauri-apps/plugin-opener";
import { useTranslation } from "react-i18next";
import { useSettings } from "../../hooks/useSettings";
import { RELEASES_URL, runUpdateCheck } from "./updaterFlow";
import { updaterAutoUpdateSupported } from "./updaterPlatform";

interface UpdateCheckerProps {
  className?: string;
}

const UpdateChecker: React.FC<UpdateCheckerProps> = ({ className = "" }) => {
  const { t } = useTranslation();
  const { settings, isLoading, updateChecksLocked } = useSettings();
  // Wait for the lock state too (null = not loaded yet), so the button never
  // flashes before the system lock (HANDY_DISABLE_UPDATER) is known.
  const settingsLoaded =
    !isLoading && settings !== null && updateChecksLocked !== null;
  // Forced-off by system configuration (HANDY_DISABLE_UPDATER) overrides the
  // stored preference without persisting it, mirroring the backend's gating
  // of the tray menu item. The pre-load fallback is true to match the
  // backend's serde default for update_checks_enabled (settings.rs), which
  // was always true; the old `?? false` made fresh stores briefly read as
  // disabled.
  const updateChecksEnabled =
    (settings?.update_checks_enabled ?? true) && updateChecksLocked === false;
  // Platforms without shipped updater artifacts never get a check button or
  // a startup auto-check; they get a one-line pointer to the releases page
  // instead (a check there can only error or find nothing).
  const autoUpdateSupported = updaterAutoUpdateSupported(platform());

  // One check on startup once settings and the lock state are known; a toast
  // appears only when an update actually exists. No polling loop. The
  // trigger distinguishes this automatic pass (which obeys the user's update
  // policy) from manual checks (which always ask).
  const hasAutoChecked = useRef(false);
  useEffect(() => {
    if (
      !settingsLoaded ||
      !updateChecksEnabled ||
      !autoUpdateSupported ||
      hasAutoChecked.current
    ) {
      return;
    }
    hasAutoChecked.current = true;
    void runUpdateCheck({ silent: true, trigger: "auto" });
  }, [settingsLoaded, updateChecksEnabled, autoUpdateSupported]);

  if (!settingsLoaded) {
    return null;
  }

  if (!autoUpdateSupported) {
    return (
      <button
        onClick={() => void openUrl(RELEASES_URL)}
        className={`text-text/60 hover:text-text/80 transition-colors tabular-nums ${className}`}
      >
        {t("footer.updater.manualOnlyPlatform")}
      </button>
    );
  }

  if (!updateChecksEnabled) {
    return (
      <span className={`text-text/60 tabular-nums ${className}`}>
        {t("footer.updateCheckingDisabled")}
      </span>
    );
  }

  return (
    <button
      onClick={() => void runUpdateCheck({ trigger: "manual" })}
      className={`text-text/60 hover:text-text/80 transition-colors tabular-nums ${className}`}
    >
      {t("footer.checkForUpdates")}
    </button>
  );
};

export default UpdateChecker;
