import React from "react";
import { platform } from "@tauri-apps/plugin-os";
import { useTranslation } from "react-i18next";

// Shared button chrome for the update-flow cards. These render inside sonner
// toasts mounted with `unstyled: true`, so they carry their own borders and
// hover states instead of leaning on the Toaster's action/cancel classes
// (which only apply to the built-in action/cancel buttons).
const primaryButtonClass =
  "px-2.5 py-1.5 text-xs font-medium rounded-lg border cursor-pointer whitespace-nowrap " +
  "bg-mid-gray/10 border-mid-gray/20 hover:bg-background-ui/30 hover:border-logo-primary";
const secondaryButtonClass =
  "px-2.5 py-1.5 text-xs font-medium rounded-lg border cursor-pointer whitespace-nowrap " +
  "bg-transparent border-mid-gray/20 hover:bg-mid-gray/10 text-mid-gray";

export const UpdateProgressBar: React.FC<{ percent: number | null }> = ({
  percent,
}) => (
  <div className="mt-1.5 h-1 w-full overflow-hidden rounded-full bg-mid-gray/20">
    {percent === null ? (
      // Unknown download size: an indeterminate pulse instead of a frozen 0%.
      <div className="h-full w-1/3 animate-pulse rounded-full bg-logo-primary" />
    ) : (
      <div
        className="h-full rounded-full bg-logo-primary transition-[width] duration-300"
        style={{ width: `${percent}%` }}
      />
    )}
  </div>
);

interface ConfirmUpdateCardProps {
  version: string;
  releaseDate: string | null;
  notes: string;
  onDownload: () => void;
  onLater: () => void;
}

export const ConfirmUpdateCard: React.FC<ConfirmUpdateCardProps> = ({
  version,
  releaseDate,
  notes,
  onDownload,
  onLater,
}) => {
  const { t } = useTranslation();
  return (
    <div className="flex w-full flex-col gap-2 text-left">
      <div className="text-sm font-medium">
        {t("footer.updater.availableTitle", { version })}
      </div>
      <div className="line-clamp-5 whitespace-pre-line text-[13px] leading-relaxed text-mid-gray">
        {releaseDate
          ? `${t("footer.updater.availableDate", { date: releaseDate })}\n`
          : ""}
        {notes}
      </div>
      <div className="mt-1 flex justify-end gap-2">
        <button onClick={onLater} className={secondaryButtonClass}>
          {t("footer.updater.laterAction")}
        </button>
        <button onClick={onDownload} className={primaryButtonClass}>
          {t("footer.updater.downloadAction")}
        </button>
      </div>
    </div>
  );
};

interface RestartPromptCardProps {
  version: string;
  autoInstalled: boolean;
  onRestartNow: () => void;
  onLater: () => void;
}

export const RestartPromptCard: React.FC<RestartPromptCardProps> = ({
  version,
  autoInstalled,
  onRestartNow,
  onLater,
}) => {
  const { t } = useTranslation();
  // The updater plugin's install() exits the app on Windows whichever button
  // the user picks, so the copy must say so instead of promising the
  // macOS behavior ("Later keeps the session running"). See updaterFlow.ts
  // finishInstall.
  const isWindows = platform() === "windows";
  return (
    <div className="flex w-full flex-col gap-2 text-left">
      <div className="text-sm font-medium">
        {t("footer.updater.restartTitle")}
      </div>
      <div className="text-[13px] leading-relaxed text-mid-gray">
        {isWindows
          ? t("footer.updater.restartDescriptionWindows", { version })
          : autoInstalled
            ? t("footer.updater.installedDescription", { version })
            : t("footer.updater.restartDescription", { version })}
      </div>
      <div className="mt-1 flex justify-end gap-2">
        <button onClick={onLater} className={secondaryButtonClass}>
          {isWindows
            ? t("footer.updater.restartLaterWindows")
            : t("footer.updater.restartLater")}
        </button>
        <button onClick={onRestartNow} className={primaryButtonClass}>
          {t("footer.updater.restartNow")}
        </button>
      </div>
    </div>
  );
};
