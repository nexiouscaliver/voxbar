import React, { useEffect, useRef } from "react";
import { useTranslation } from "react-i18next";
import { useSettings } from "../../hooks/useSettings";
import { runUpdateCheck } from "./updaterFlow";

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

  // One silent check on startup once settings and the lock state are known;
  // a toast appears only when an update actually exists. No polling loop.
  const hasAutoChecked = useRef(false);
  useEffect(() => {
    if (!settingsLoaded || !updateChecksEnabled || hasAutoChecked.current) {
      return;
    }
    hasAutoChecked.current = true;
    void runUpdateCheck({ silent: true });
  }, [settingsLoaded, updateChecksEnabled]);

  if (!settingsLoaded) {
    return null;
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
      onClick={() => void runUpdateCheck()}
      className={`text-text/60 hover:text-text/80 transition-colors tabular-nums ${className}`}
    >
      {t("footer.checkForUpdates")}
    </button>
  );
};

export default UpdateChecker;
