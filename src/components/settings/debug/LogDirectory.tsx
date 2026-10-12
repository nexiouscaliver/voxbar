import React, { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { commands } from "@/bindings";
import { SettingContainer } from "../../ui/SettingContainer";
import { PathDisplay } from "../../ui/PathDisplay";

interface LogDirectoryProps {
  descriptionMode?: "tooltip" | "inline";
  grouped?: boolean;
}

export const LogDirectory: React.FC<LogDirectoryProps> = ({
  descriptionMode = "tooltip",
  grouped = false,
}) => {
  const { t } = useTranslation();
  const [logDir, setLogDir] = useState<string>("");
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const loadLogDirectory = async () => {
      try {
        const result = await commands.getLogDirPath();
        if (result.status === "ok") {
          setLogDir(result.data);
        } else {
          setError(result.error);
        }
      } catch (err) {
        const errorMessage =
          err && typeof err === "object" && "message" in err
            ? String(err.message)
            : "Failed to load log directory";
        setError(errorMessage);
      } finally {
        setLoading(false);
      }
    };

    loadLogDirectory();
  }, []);

  const handleOpen = async () => {
    if (!logDir) return;
    try {
      await commands.openLogDir();
    } catch (openError) {
      // Visible failure: this button silently ACL-died for a whole release
      // because the error only reached the console.
      console.error("Failed to open log directory:", openError);
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

  return (
    <SettingContainer
      title={t("settings.debug.logDirectory.title")}
      description={t("settings.debug.logDirectory.description")}
      descriptionMode={descriptionMode}
      grouped={grouped}
      layout="stacked"
    >
      {/* KB-092: tokenized skeleton and error block (mid-gray washes plus the
          semantic error pair, as in AppDataDirectory), not the hardcoded
          light-theme grays and red wash. */}
      {loading ? (
        <div className="animate-pulse">
          <div className="h-8 bg-mid-gray/10 rounded" />
        </div>
      ) : error ? (
        <div className="p-3 bg-error/10 border border-error/30 rounded text-xs text-error">
          {t("errors.loadDirectory", { error })}
        </div>
      ) : (
        <PathDisplay path={logDir} onOpen={handleOpen} disabled={!logDir} />
      )}
    </SettingContainer>
  );
};
