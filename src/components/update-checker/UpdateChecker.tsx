import React from "react";
import { useTranslation } from "react-i18next";
import { openUrl } from "@tauri-apps/plugin-opener";
import { useSettings } from "../../hooks/useSettings";

// This build ships no in-app updater (the updater plugin is not part of the
// app), so the footer's "Check for updates" affordance opens the GitHub
// releases page in the default browser instead of querying an endpoint.
const RELEASES_URL = "https://github.com/nexiouscaliver/voxbar/releases";

interface UpdateCheckerProps {
  className?: string;
}

const UpdateChecker: React.FC<UpdateCheckerProps> = ({ className = "" }) => {
  const { t } = useTranslation();
  const { settings, isLoading, updateChecksLocked } = useSettings();
  // Wait for the lock state too (null = not loaded yet), so the link never
  // flashes before the system lock (HANDY_DISABLE_UPDATER) is known.
  const settingsLoaded =
    !isLoading && settings !== null && updateChecksLocked !== null;
  // Forced-off by system configuration (HANDY_DISABLE_UPDATER) overrides the
  // stored preference without persisting it, mirroring the backend's gating
  // of the tray menu item.
  const updateChecksEnabled =
    (settings?.update_checks_enabled ?? false) && updateChecksLocked === false;

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

  const openReleases = () => {
    openUrl(RELEASES_URL).catch((error) => {
      console.error("Failed to open the releases page:", error);
    });
  };

  return (
    <button
      onClick={openReleases}
      className={`text-text/60 hover:text-text/80 transition-colors tabular-nums ${className}`}
    >
      {t("footer.checkForUpdates")}
    </button>
  );
};

export default UpdateChecker;
