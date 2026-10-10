import React from "react";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { openUrl } from "@tauri-apps/plugin-opener";
import { platform } from "@tauri-apps/plugin-os";
import { toast } from "sonner";
import i18n from "../../i18n";
import { useSettingsStore } from "../../stores/settingsStore";
import { commands } from "../../bindings";
import {
  ConfirmUpdateCard,
  RestartPromptCard,
  UpdateProgressBar,
} from "./UpdateToasts";
import { updaterAutoUpdateSupported } from "./updaterPlatform";

// Where updates come from. The updater itself downloads from the GitHub
// release (endpoint configured in tauri.conf.json); this URL is the manual
// fallback shown when a check or install fails.
export const RELEASES_URL = "https://github.com/nexiouscaliver/voxbar/releases";

const t = (key: string, options?: Record<string, unknown>): string =>
  i18n.t(key, options);

// Each network call must not hang silently behind a stalled connection, and
// a ~20 MB download on a slow link can legitimately take minutes - but not
// ten. The check budget must exceed a full TCP SYN-retransmission ladder:
// GitHub's asset CDN (Fastly, 185.199.x) resolves to 3 IPs and on lossy ISP
// paths MEASURED 2 of 3 drop SYNs, completing the connect only after the
// ~15s retransmission ladder (documented ISP-to-Fastly peering issue, see
// github/orgs/community discussions 143212 and 127077). A 15s total budget
// dies inside the ladder on every bad-IP draw; 30s lets one attempt absorb
// the ladder (~16-17s) and still finish, and each retry redraws an IP.
const CHECK_TIMEOUT_MS = 30000;
const DOWNLOAD_TIMEOUT_MS = 600000;
// The plugin emits one progress event per network chunk; re-rendering the
// toast per chunk churns the webview and makes the download feel slower than
// it is. Coalesce renders to a fixed rhythm.
const PROGRESS_RENDER_INTERVAL_MS = 250;

export type UpdateTrigger = "manual" | "auto";
export type UpdatePolicyValue = "ask" | "download" | "install";

// Only one flow may run at a time; the tray item, the footer button, the
// About button and the startup auto-check all funnel through here and must
// not stack toasts.
let inFlight = false;

// The id of the currently-visible restart prompt, if any. A later check that
// finds the same update replaces it instead of stacking a second card.
let restartPromptId: string | number | null = null;

// The system lock (HANDY_DISABLE_UPDATER) and the user's stored preference
// gate the whole flow. Unknown lock state fails CLOSED (KB-033): while the
// lock probe is still in flight a locked install must not be able to slip a
// real network check through the loading window. The store's
// loadUpdateChecksLocked resolves this quickly and only falls back to
// "not locked" when the probe itself errors.
function updateChecksAllowed(): boolean {
  const { settings, updateChecksLocked } = useSettingsStore.getState();
  if (updateChecksLocked !== false) return false;
  if (settings && settings.update_checks_enabled === false) return false;
  return true;
}

function currentPolicy(): UpdatePolicyValue {
  const policy = useSettingsStore.getState().settings?.update_policy;
  return policy === "download" || policy === "install" ? policy : "ask";
}

// The file-log seam: webview console output never reaches voxbar.log, so
// every update-flow decision rides through this Rust command and lands as
// one info line per decision (see commands/updater.rs). Fire-and-forget:
// a logging failure must never break the flow it describes.
function logDecision(stage: string, detail?: string): void {
  commands
    .logUpdateDecision(stage, detail ?? null)
    .catch((error) =>
      console.error(`Failed to log update decision (${stage}):`, error),
    );
}

// Update toasts render in the main window's webview, and that window is
// hidden (not destroyed) while the app dwells in the tray. Manual checks
// must surface it first or every toast below renders nowhere.
async function revealMainWindow(): Promise<void> {
  try {
    await commands.showMainWindowCommand();
  } catch (error) {
    console.error("Failed to reveal the main window:", error);
  }
}

// The manifest carries an RFC3339 string; users should read "Oct 8, 2026",
// not "2026-10-08T18:42:51Z". Unparseable values fall back to the raw text.
function formatReleaseDate(iso: string | undefined | null): string | null {
  if (!iso) return null;
  const parsed = new Date(iso);
  if (Number.isNaN(parsed.getTime())) return iso;
  return new Intl.DateTimeFormat(i18n.language || undefined, {
    dateStyle: "medium",
  }).format(parsed);
}

// One or two plain-text sentences from the release body: strip markdown
// headings and emphasis, drop empty lines, cap the total length so the
// confirm card stays a toast rather than becoming a wall of text.
function releaseNotesExcerpt(body: string | undefined): string {
  if (!body) return "";
  const lines = body
    .split(/\r?\n/)
    .map((line) =>
      line
        .replace(/^#+\s*/, "")
        .replace(/\*\*/g, "")
        .replace(/`/g, "")
        .trim(),
    )
    .filter((line) => line.length > 0);
  const excerpt = lines.slice(0, 3).join(" ");
  return excerpt.length > 200 ? `${excerpt.slice(0, 197)}...` : excerpt;
}

function formatMb(bytes: number): string {
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

// Terminal failure toast with the manual escape hatch. The raw error goes to
// the console; users get a sentence and a button, not a RequestError dump.
// When the app is running translocated (read-only mount), the copy swaps to
// the move-to-Applications guidance, because retrying from there can never
// succeed and the manual download would land in the same trap.
async function showFailureToast(id?: string | number): Promise<void> {
  let translocated = false;
  try {
    const result = await commands.isAppTranslocated();
    translocated = result.status === "ok" && result.data === true;
  } catch (error) {
    console.error("Failed to check App Translocation state:", error);
  }
  toast.error(t("footer.updater.failedTitle"), {
    id,
    duration: 6000,
    description: t(
      translocated
        ? "footer.updater.translocatedDescription"
        : "footer.updater.failedDescription",
    ),
    action: {
      label: t("footer.updater.failedAction"),
      onClick: () => void openUrl(RELEASES_URL),
    },
  });
}

// Download with a throttled progress toast (MB counter plus a real progress
// bar, or an indeterminate pulse when the CDN withholds the content length).
// Resolves true only when the payload is fully downloaded and still attached
// to `update`; installation is a separate, later step.
//
// The GitHub assets CDN (release-assets.githubusercontent.com) intermittently
// stalls NEW connections for whole minutes on some networks - observed as
// every check reaching github.com cleanly and then hanging on the redirect
// target until the timeout - so both the check and the download retry a
// bounded number of times before surfacing an error. A stall that outlives
// all attempts is a real network problem the manual fallback covers.
//
// Retries run BACK-TO-BACK with no pause on purpose: the app dwells hidden
// in the tray (start_hidden), and macOS SUSPENDS JavaScript timers in a
// hidden webview - a setTimeout between attempts froze the whole retry
// chain until the window next opened (observed live: the startup auto-check
// stalled mid-retry for minutes). The attempt pacing comes entirely from
// each attempt's own network timeout, which never suspends.
const CHECK_ATTEMPTS = 3;
const DOWNLOAD_ATTEMPTS = 2;

async function checkForUpdates(): Promise<Update | null> {
  let lastError: unknown;
  for (let attempt = 1; attempt <= CHECK_ATTEMPTS; attempt++) {
    try {
      return await check({ timeout: CHECK_TIMEOUT_MS });
    } catch (error) {
      lastError = error;
      console.error(
        `Update check attempt ${attempt}/${CHECK_ATTEMPTS} failed:`,
        error,
      );
    }
  }
  throw lastError;
}

async function downloadUpdate(update: Update): Promise<boolean> {
  const progressId = "updater-download";
  let contentLength: number | null = null;
  let downloaded = 0;
  let lastRender = 0;

  const render = (force: boolean) => {
    const now = Date.now();
    if (!force && now - lastRender < PROGRESS_RENDER_INTERVAL_MS) return;
    lastRender = now;
    let percent: number | null = null;
    if (contentLength && contentLength > 0) {
      percent = Math.min(99, Math.round((downloaded / contentLength) * 100));
    }
    const title =
      percent !== null
        ? t("footer.updater.progressSize", {
            downloaded: formatMb(downloaded),
            total: formatMb(contentLength ?? 0),
            percent,
          })
        : t("footer.updater.progressUnknown", {
            downloaded: formatMb(downloaded),
          });
    toast.loading(title, {
      id: progressId,
      description: React.createElement(UpdateProgressBar, { percent }),
    });
  };

  for (let attempt = 1; attempt <= DOWNLOAD_ATTEMPTS; attempt++) {
    // Counters reset per attempt: a failed attempt may have reported partial
    // progress that the fresh request starts over.
    contentLength = null;
    downloaded = 0;
    try {
      logDecision(
        "download_started",
        `attempt ${attempt}/${DOWNLOAD_ATTEMPTS}`,
      );
      await update.download(
        (event) => {
          switch (event.event) {
            case "Started":
              contentLength = event.data.contentLength ?? null;
              render(true);
              break;
            case "Progress":
              downloaded += event.data.chunkLength;
              render(false);
              break;
            case "Finished":
              break;
          }
        },
        { timeout: DOWNLOAD_TIMEOUT_MS },
      );
      return true;
    } catch (error) {
      console.error(
        `Update download attempt ${attempt}/${DOWNLOAD_ATTEMPTS} failed:`,
        error,
      );
      logDecision(
        "download_failed",
        `attempt ${attempt}/${DOWNLOAD_ATTEMPTS}: ${String(error)}`,
      );
      // No pause before the retry: timers suspend in the hidden tray
      // webview (see the note above CHECK_ATTEMPTS); the download window
      // is only visible during a manual flow, but the same freeze would
      // stall the auto-download policy paths.
      if (attempt < DOWNLOAD_ATTEMPTS) {
        render(true);
      }
    }
  }
  void showFailureToast(progressId);
  return false;
}

// Install the already-downloaded payload. On macOS, "Restart now" relaunches
// into the new version while "Later" swaps the bundle and keeps the session
// running (the app never relaunches itself unprompted). On Windows the
// plugin's install() exits the app whichever button was picked, so the
// restart prompt's copy is platform-split (RestartPromptCard) to keep that
// promise honest.
async function finishInstall(
  update: Update,
  restartNow: boolean,
): Promise<void> {
  const installId = "updater-install";
  try {
    logDecision("install_started", update.version);
    toast.loading(t("footer.updater.installingTitle"), { id: installId });
    await update.install();
    logDecision(
      "install_finished",
      `${update.version}${restartNow ? ", relaunching" : ", active on next launch"}`,
    );
    toast.dismiss(installId);
    if (restartNow) {
      await relaunch();
    }
  } catch (error) {
    console.error("Update install failed:", error);
    logDecision("install_failed", String(error));
    void showFailureToast(installId);
  }
}

// The "restart now / later" prompt. `autoInstalled` is true when the policy
// already swapped the bundle (install policy), so the copy says "active on
// next restart" instead of "downloaded and waiting".
function showRestartPrompt(update: Update, autoInstalled: boolean): void {
  if (restartPromptId !== null) {
    toast.dismiss(restartPromptId);
  }
  restartPromptId = toast.custom(
    (id) =>
      React.createElement(RestartPromptCard, {
        version: update.version,
        autoInstalled,
        onRestartNow: () => {
          toast.dismiss(id);
          void finishInstall(update, true);
        },
        onLater: () => {
          toast.dismiss(id);
          void finishInstall(update, false);
        },
      }),
    { duration: Infinity },
  );
}

export async function runUpdateCheck(
  options: { silent?: boolean; trigger?: UpdateTrigger } = {},
): Promise<void> {
  const silent = options.silent ?? false;
  const trigger: UpdateTrigger =
    options.trigger ?? (silent ? "auto" : "manual");
  if (inFlight || !updateChecksAllowed()) return;

  // Platforms without shipped updater artifacts (Windows/Linux today) are
  // gated before any network work: a check there can only error or find
  // nothing. The visible entrypoints render a one-line notice instead of a
  // button; this guard is the backstop for any path that still calls in
  // (e.g. an outdated tray menu from before a settings change).
  if (!updaterAutoUpdateSupported(platform())) {
    if (trigger === "manual") {
      await revealMainWindow();
      toast.info(t("footer.updater.manualOnlyPlatform"), {
        duration: 8000,
        action: {
          label: t("footer.updater.failedAction"),
          onClick: () => void openUrl(RELEASES_URL),
        },
      });
    }
    logDecision("check_skipped", "platform has no updater artifacts");
    return;
  }

  inFlight = true;
  const release = () => {
    inFlight = false;
  };

  try {
    logDecision("check_started", `trigger=${trigger}`);
    if (trigger === "manual") {
      // Instant feedback in a visible window; without this a tray click on a
      // hidden window shows every toast nowhere and reads as "does nothing".
      await revealMainWindow();
      toast.loading(t("footer.checkingUpdates"), { id: "updater-checking" });
    }

    const update = await checkForUpdates();

    if (!update) {
      if (trigger === "manual") {
        // Same id as the checking toast: this REPLACES the spinner rather
        // than stacking next to it, and is never dismissed afterwards (the
        // old flow killed this success toast one frame after showing it).
        toast.success(t("footer.upToDate"), {
          id: "updater-checking",
          duration: 4000,
        });
      }
      release();
      return;
    }

    if (trigger === "manual") {
      toast.dismiss("updater-checking");
    }

    // The policy only ever governs the automatic startup check; a manual
    // check was initiated on purpose and always asks what to do next.
    const policy = trigger === "auto" ? currentPolicy() : "ask";
    logDecision("offered", `version=${update.version} policy=${policy}`);

    if (policy === "install") {
      // Chrome-style: fetch and swap silently (no surprise window for a
      // tray app), then leave a persistent restart prompt.
      if (await downloadUpdate(update)) {
        await finishInstall(update, false);
        showRestartPrompt(update, true);
      }
      release();
      return;
    }

    if (policy === "download") {
      if (await downloadUpdate(update)) {
        showRestartPrompt(update, false);
      }
      release();
      return;
    }

    // ask (the default, and every manual check): confirm card with the
    // release date and a notes excerpt. On download it transitions into the
    // progress toast and then the restart prompt; the card itself is
    // dismissed before any of that starts.
    toast.custom(
      (id) =>
        React.createElement(ConfirmUpdateCard, {
          version: update.version,
          releaseDate: formatReleaseDate(update.date),
          notes: releaseNotesExcerpt(update.body),
          onDownload: () => {
            toast.dismiss(id);
            void (async () => {
              if (await downloadUpdate(update)) {
                showRestartPrompt(update, false);
              }
            })();
          },
          onLater: () => {
            toast.dismiss(id);
            logDecision("declined", update.version);
          },
        }),
      { duration: Infinity },
    );
    release();
  } catch (error) {
    console.error("Update check failed:", error);
    logDecision("check_failed", `trigger=${trigger}: ${String(error)}`);
    if (trigger === "manual") {
      toast.error(t("footer.updater.checkFailedTitle"), {
        id: "updater-checking",
        duration: 6000,
        action: {
          label: t("footer.updater.failedAction"),
          onClick: () => void openUrl(RELEASES_URL),
        },
      });
    }
    release();
  }
}
