import React, { useEffect, useRef } from "react";
import { platform } from "@tauri-apps/plugin-os";
import { useSettings } from "../../hooks/useSettings";
import { runUpdateCheck } from "./updaterFlow";
import { resolveAutoCheck } from "./updaterAutoCheck";
import { updaterAutoUpdateSupported } from "./updaterPlatform";

/**
 * Headless update checker: runs the one-shot startup auto-check (and only
 * that). The footer's old "Check for updates" text button rendered in the
 * same gray, weight and size as the static version beside it, so the only
 * clickable element in that run read as a label - and duplicated the About
 * page's own Version-row button. The footer is now purely informational
 * (model + version); the single, clearly interactive check control lives
 * in the About Version row (AboutSettings.tsx).
 */
const UpdateChecker: React.FC = () => {
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
  // policy) from manual checks (which always ask). The latch is also
  // consumed while the loaded settings say checks are off, so re-enabling
  // checks mid-session never phones home as a direct result of the toggle
  // flip (KB-191); a fresh launch still auto-checks once, and manual checks
  // are unaffected. See updaterAutoCheck.ts for the decision table.
  const hasAutoChecked = useRef(false);
  useEffect(() => {
    const decision = resolveAutoCheck({
      settingsLoaded,
      updateChecksEnabled,
      autoUpdateSupported,
      hasAutoChecked: hasAutoChecked.current,
    });
    hasAutoChecked.current = decision.hasAutoChecked;
    if (decision.shouldRunCheck) {
      void runUpdateCheck({ silent: true, trigger: "auto" });
    }
  }, [settingsLoaded, updateChecksEnabled, autoUpdateSupported]);

  // No UI: the manual "Check for updates" control lives once, as a real
  // button, in the About Version row. The startup auto-check above still
  // runs from here, so nothing about update behavior changes - only the
  // duplicated affordance is gone.
  return null;
};

export default UpdateChecker;
