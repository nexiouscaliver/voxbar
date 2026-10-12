import React, { useState, useEffect } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { commands } from "@/bindings";
import { SettingContainer } from "../ui/SettingContainer";
import { PathDisplay } from "../ui/PathDisplay";

interface AppDataDirectoryProps {
  descriptionMode?: "tooltip" | "inline";
  grouped?: boolean;
}

export const AppDataDirectory: React.FC<AppDataDirectoryProps> = ({
  descriptionMode = "inline",
  grouped = false,
}) => {
  const { t } = useTranslation();
  const [appDirPath, setAppDirPath] = useState<string>("");
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const loadAppDirectory = async () => {
      try {
        const result = await commands.getAppDirPath();
        if (result.status === "ok") {
          setAppDirPath(result.data);
        } else {
          setError(result.error);
        }
      } catch (err) {
        setError(
          err instanceof Error ? err.message : "Failed to load app directory",
        );
      } finally {
        setLoading(false);
      }
    };

    loadAppDirectory();
  }, []);

  const handleOpen = async () => {
    if (!appDirPath) return;
    try {
      await commands.openAppDataDir();
    } catch (openError) {
      console.error("Failed to open app data directory:", openError);
      toast.error(
        t("errors.openDirectory", {
          error:
            openError && typeof openError === "object" && "message" in openError
              ? String((openError as Error).message)
              : String(openError),
        }),
      );
    }
  };

  if (loading) {
    // KB-092: tokenized skeleton (mid-gray washes), not bg-gray-100/200 -
    // those hardcoded light grays were invisible-or-loud on the dark theme.
    return (
      <div className="animate-pulse">
        <div className="h-4 bg-mid-gray/20 rounded w-1/3 mb-2"></div>
        <div className="h-8 bg-mid-gray/10 rounded"></div>
      </div>
    );
  }

  if (error) {
    // KB-092: the semantic error token pair (same recipe as ui/Alert's
    // error variant), not the hardcoded light-theme red wash.
    return (
      <div className="p-4 bg-error/10 border border-error/30 rounded-lg">
        <p className="text-error text-sm">
          {t("errors.loadDirectory", { error })}
        </p>
      </div>
    );
  }

  return (
    <SettingContainer
      title={t("settings.about.appDataDirectory.title")}
      description={t("settings.about.appDataDirectory.description")}
      descriptionMode={descriptionMode}
      grouped={grouped}
      layout="stacked"
    >
      <PathDisplay
        path={appDirPath}
        onOpen={handleOpen}
        disabled={!appDirPath}
      />
    </SettingContainer>
  );
};
