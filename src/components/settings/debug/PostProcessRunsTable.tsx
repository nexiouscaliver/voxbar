import React, { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Copy } from "lucide-react";
import { commands, events } from "@/bindings";
import type { PostProcessRunRecord } from "@/bindings";
import { SettingContainer } from "../../ui/SettingContainer";
import { copyToClipboard } from "../history/clipboard";

// How many runs the table requests from the backend ring (which caps at 100).
const RUNS_LIMIT = 100;
// Live refresh cadence while the tab is open, so a dictation polishing in
// the background appears without a manual reload (the outcome event also
// triggers an immediate refresh).
const REFRESH_MS = 2000;

const formatClock = (unixMs: number): string => {
  const date = new Date(unixMs);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(
    date.getSeconds(),
  )}`;
};

/* The outcome token (`applied` | `skipped:<reason>` | `failed:<class>`)
 * built from the wire shape { kind, reason?/class? }, matching the token
 * the backend writes into history and the pp: log line. */
const outcomeToken = (
  outcome: NonNullable<PostProcessRunRecord["outcome"]>,
): string => {
  if (outcome.kind === "skipped") {
    return `skipped:${outcome.reason}`;
  }
  if (outcome.kind === "failed") {
    return `failed:${outcome.class}`;
  }
  return "applied";
};

/* The outcome cell: the base word plus the failure class when the run
 * failed. The token's full form rides the tooltip. */
const OutcomeCell: React.FC<{ record: PostProcessRunRecord }> = ({
  record,
}) => {
  const { t } = useTranslation();
  if (!record.outcome) {
    return (
      <span className="text-text/50">{t("settings.debug.ppRuns.live")}</span>
    );
  }
  const token = outcomeToken(record.outcome);
  const base = token.split(":")[0];
  const detail = token.includes(":")
    ? token.split(":").slice(1).join(":")
    : null;
  return (
    <span
      className={
        base === "failed"
          ? "text-red-600 dark:text-red-400"
          : base === "skipped"
            ? "text-amber-600 dark:text-amber-400"
            : "text-emerald-600 dark:text-emerald-400"
      }
      title={token}
    >
      {t(`settings.ppOutcome.${base}`)}
      {detail ? ` (${detail})` : ""}
    </span>
  );
};

interface PostProcessRunsTableProps {
  descriptionMode?: "tooltip" | "inline";
  grouped?: boolean;
}

/**
 * The pp: observability table (WS3): the last post-process runs with their
 * engine, prompt, latency, and outcome, plus one click to copy the run's
 * captured `pp:` log lines (exactly what voxbar.log holds for that run).
 */
export const PostProcessRunsTable: React.FC<PostProcessRunsTableProps> = ({
  descriptionMode = "tooltip",
  grouped = false,
}) => {
  const { t } = useTranslation();
  const [runs, setRuns] = useState<PostProcessRunRecord[]>([]);
  const [copiedRun, setCopiedRun] = useState<number | null>(null);
  const aliveRef = useRef(true);

  const refresh = useCallback(async () => {
    try {
      const result = await commands.getPostProcessRuns(RUNS_LIMIT);
      if (result.status === "ok" && aliveRef.current) {
        setRuns(result.data);
      }
    } catch (error) {
      console.error("Failed to load post-process runs:", error);
    }
  }, []);

  useEffect(() => {
    aliveRef.current = true;
    void refresh();

    // Live refresh: an outcome event means a run just concluded (the table
    // picks it up immediately), and the poll covers in-flight updates while
    // a dictation runs in the background.
    const unlisten = events.postProcessRunEvent.listen((event) => {
      if (
        event.payload.phase === "outcome" ||
        event.payload.phase === "requested"
      ) {
        void refresh();
      }
    });
    const interval = setInterval(() => void refresh(), REFRESH_MS);

    return () => {
      aliveRef.current = false;
      clearInterval(interval);
      unlisten.then((fn) => fn());
    };
  }, [refresh]);

  const handleCopyLog = async (record: PostProcessRunRecord) => {
    const copied = await copyToClipboard(record.log_lines.join("\n"));
    if (!copied) return;
    setCopiedRun(record.run_id);
    setTimeout(() => setCopiedRun(null), 2000);
  };

  return (
    <SettingContainer
      title={t("settings.debug.ppRuns.title")}
      description={t("settings.debug.ppRuns.description")}
      descriptionMode={descriptionMode}
      grouped={grouped}
    >
      {runs.length === 0 ? (
        <div className="px-4 py-3 text-sm text-text/60">
          {t("settings.debug.ppRuns.empty")}
        </div>
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full text-xs">
            <thead>
              <tr className="text-left text-secondary border-b border-mid-gray/20">
                <th className="px-4 py-2 font-medium">
                  {t("settings.debug.ppRuns.time")}
                </th>
                <th className="px-2 py-2 font-medium">
                  {t("settings.debug.ppRuns.engine")}
                </th>
                <th className="px-2 py-2 font-medium">
                  {t("settings.debug.ppRuns.prompt")}
                </th>
                <th className="px-2 py-2 font-medium">
                  {t("settings.debug.ppRuns.latency")}
                </th>
                <th className="px-2 py-2 font-medium">
                  {t("settings.debug.ppRuns.outcome")}
                </th>
                <th className="px-2 py-2 font-medium" aria-label="copy" />
              </tr>
            </thead>
            <tbody>
              {runs.map((record) => (
                <tr
                  key={record.run_id}
                  className="border-b border-mid-gray/10 last:border-0"
                >
                  <td className="px-4 py-1.5 whitespace-nowrap text-secondary">
                    {formatClock(record.started_at_unix_ms)}
                  </td>
                  <td
                    className="px-2 py-1.5 whitespace-nowrap"
                    title={record.model}
                  >
                    {record.provider_id}/{record.model}
                  </td>
                  <td
                    className="px-2 py-1.5 max-w-40 truncate"
                    title={record.prompt_name ?? record.prompt_id ?? ""}
                  >
                    {record.prompt_name ?? record.prompt_id ?? "-"}
                  </td>
                  <td className="px-2 py-1.5 whitespace-nowrap">
                    {record.total_ms !== null
                      ? t("settings.debug.ppRuns.latencyMs", {
                          ms: record.total_ms,
                        })
                      : "-"}
                  </td>
                  <td className="px-2 py-1.5 whitespace-nowrap">
                    <OutcomeCell record={record} />
                  </td>
                  <td className="px-2 py-1.5">
                    <button
                      type="button"
                      onClick={() => void handleCopyLog(record)}
                      className="p-1 rounded-md text-text/50 hover:text-logo-primary cursor-pointer transition-colors"
                      title={t("settings.debug.ppRuns.copyLog")}
                      aria-label={t("settings.debug.ppRuns.copyLog")}
                    >
                      <Copy width={14} height={14} />
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {copiedRun !== null && (
        <div className="px-4 py-1.5 text-xs text-emerald-600 dark:text-emerald-400">
          {t("settings.debug.ppRuns.copied", { run: copiedRun })}
        </div>
      )}
    </SettingContainer>
  );
};
