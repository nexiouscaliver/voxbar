import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { openUrl } from "@tauri-apps/plugin-opener";
import { toast } from "sonner";
import i18n from "../../i18n";
import { useSettingsStore } from "../../stores/settingsStore";

// Where updates come from. The updater itself downloads from the GitHub
// release (endpoint configured in tauri.conf.json); this URL is the manual
// fallback shown when a check or install fails.
export const RELEASES_URL = "https://github.com/nexiouscaliver/voxbar/releases";

const t = (key: string, options?: Record<string, unknown>): string =>
  i18n.t(key, options);

// Only one flow may run at a time; the tray item, the footer button and the
// startup auto-check all funnel through here and must not stack toasts.
let inFlight = false;

// The system lock (HANDY_DISABLE_UPDATER) and the user's stored preference
// gate the whole flow. Unknown lock state fails open, mirroring the store's
// own loadUpdateChecksLocked fallback (the backend removes the tray item
// entirely when locked, so a locked install cannot reach here anyway).
function updateChecksAllowed(): boolean {
  const { settings, updateChecksLocked } = useSettingsStore.getState();
  if (updateChecksLocked === true) return false;
  if (settings && settings.update_checks_enabled === false) return false;
  return true;
}

export function openReleases(): void {
  openUrl(RELEASES_URL).catch((error) => {
    console.error("Failed to open the releases page:", error);
  });
}

// First non-empty lines of the release notes, capped so the toast stays a
// toast and not a wall of text.
function releaseNotesExcerpt(body: string | undefined | null): string {
  if (!body) return "";
  const lines = body
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line.length > 0);
  return lines.slice(0, 2).join("\n");
}

// Download and install the announced update, showing progress, then relaunch.
// Any failure swaps the progress toast for an error toast whose action opens
// the releases page (the pre-1.1.0 behavior, kept as the fallback path).
async function installUpdate(update: Update): Promise<void> {
  const progressId = `updater-progress-${update.version}`;
  toast.loading(t("footer.updater.progress", { percent: 0 }), {
    id: progressId,
  });
  let downloaded = 0;
  let contentLength = 0;
  try {
    await update.downloadAndInstall((event) => {
      switch (event.event) {
        case "Started":
          contentLength = event.data.contentLength ?? 0;
          break;
        case "Progress":
          downloaded += event.data.chunkLength;
          if (contentLength > 0) {
            const percent = Math.min(
              100,
              Math.floor((downloaded / contentLength) * 100),
            );
            toast.loading(t("footer.updater.progress", { percent }), {
              id: progressId,
            });
          }
          break;
        case "Finished":
          toast.loading(t("footer.updater.installRestart"), { id: progressId });
          break;
      }
    });
    await relaunch();
    // relaunch() restarts the process; if it somehow returns, close the
    // progress toast so the UI is not stuck on "installing".
    toast.dismiss(progressId);
  } catch (error) {
    console.error("Update install failed:", error);
    toast.error(t("footer.updater.failedTitle"), {
      id: progressId,
      description: String(error),
      action: {
        label: t("footer.updater.failedAction"),
        onClick: openReleases,
      },
    });
  }
}

// The shared check -> confirm -> install flow used by the footer button, the
// tray "Check for Updates" item (via the App-level request-update-check
// listener) and the startup auto-check. With silent=true (startup), the
// "up to date" outcome is quiet; an available update still prompts.
export async function runUpdateCheck(
  options: { silent?: boolean } = {},
): Promise<void> {
  if (inFlight) return;
  if (!updateChecksAllowed()) return;
  inFlight = true;
  const release = () => {
    inFlight = false;
  };

  let update: Update | null;
  try {
    update = await check();
  } catch (error) {
    console.error("Update check failed:", error);
    toast.error(t("footer.updater.checkFailedTitle"), {
      description: String(error),
      action: {
        label: t("footer.updater.failedAction"),
        onClick: openReleases,
      },
    });
    release();
    return;
  }

  if (!update) {
    if (!options.silent) {
      toast.success(t("footer.upToDate"));
    }
    release();
    return;
  }

  const description = [
    update.date ? t("footer.updater.availableDate", { date: update.date }) : "",
    releaseNotesExcerpt(update.body),
  ]
    .filter((part) => part.length > 0)
    .join("\n");

  // Confirmation surface: the user decides before anything downloads. Kept
  // until dismissed (no auto-close) so a popped-over-VoxBar user cannot miss
  // it; "Later" simply dismisses and the check can be re-run any time.
  toast(t("footer.updater.availableTitle", { version: update.version }), {
    description,
    duration: Infinity,
    action: {
      label: t("footer.updater.downloadAction"),
      onClick: () => {
        void installUpdate(update);
      },
    },
    cancel: {
      label: t("footer.updater.laterAction"),
      onClick: () => {
        // Release the Rust-side update resource; nothing was downloaded.
        void update?.close();
      },
    },
  });
  release();
}
