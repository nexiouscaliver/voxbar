import React, { useMemo } from "react";
import { useTranslation } from "react-i18next";
import { diffText, type DiffLine } from "@/lib/diff";

/**
 * GitHub-style red/green renderer for the pure diff util (v1.6.0 W1-L6).
 * Context lines stay plain; removed lines tint red, added lines green,
 * and the changed words (or CJK characters) inside paired lines carry a
 * stronger tint. Shared by History's "View changes" expander and the
 * Prompt Studio result cards, so both surfaces render edits identically.
 *
 * The block is pinned LTR and monospaced: it renders raw transcript bytes,
 * which read as content, not as UI prose, even in RTL locales.
 */
const DiffLineRow: React.FC<{ line: DiffLine }> = ({ line }) => {
  const rowTint =
    line.kind === "removed"
      ? "bg-red-500/10"
      : line.kind === "added"
        ? "bg-green-500/10"
        : "";
  const marker =
    line.kind === "removed" ? "-" : line.kind === "added" ? "+" : " ";
  const markerTint =
    line.kind === "removed"
      ? "text-red-500/80"
      : line.kind === "added"
        ? "text-green-500/80"
        : "text-text/20";
  const plain = line.spans.map((span) => span.text).join("");
  return (
    <div className={`flex items-start ${rowTint}`}>
      <span
        className={`w-4 shrink-0 select-none text-center font-mono text-xs leading-5 ${markerTint}`}
        aria-hidden="true"
      >
        {marker}
      </span>
      <span className="flex-1 font-mono text-xs leading-5 whitespace-pre-wrap break-words">
        {line.spans.map((span, index) =>
          span.changed ? (
            <span
              key={index}
              className={`rounded-sm ${
                line.kind === "removed" ? "bg-red-500/30" : "bg-green-500/30"
              }`}
            >
              {span.text}
            </span>
          ) : (
            <span key={index}>{span.text}</span>
          ),
        )}
        {plain === "" ? "\u00A0" : null}
      </span>
    </div>
  );
};

/** The "+1 added · 1 removed · 19 chars changed" chip above a diff: the
 * numbers that make a 1-character polish in a wall of text visible before
 * the diff opens. Added/removed counts read the semantic success/error
 * tokens (green-700/red-600 on light, green-400/red-400 on dark) instead
 * of raw Tailwind 500s, which measured 2.13:1 on the light card; each
 * locale string carries its own singular/plural form and sign. */
export const DiffSummaryBadge: React.FC<{
  addedLines: number;
  removedLines: number;
  addedChars: number;
  removedChars: number;
  className?: string;
}> = ({ addedLines, removedLines, addedChars, removedChars, className }) => {
  const { t } = useTranslation();
  return (
    <span
      className={`inline-flex items-center gap-1.5 font-mono text-xs ${className ?? ""}`}
    >
      <span className="text-success">
        {t("settings.studio.diff.added", { count: addedLines })}
      </span>
      <span className="text-error">
        {t("settings.studio.diff.removed", { count: removedLines })}
      </span>
      <span className="text-secondary/50" aria-hidden="true">
        &middot;
      </span>
      <span className="text-secondary">
        {t("settings.studio.diff.charsChanged", {
          count: addedChars + removedChars,
        })}
      </span>
    </span>
  );
};

export const DiffView: React.FC<{
  oldText: string;
  newText: string;
  className?: string;
}> = ({ oldText, newText, className }) => {
  const result = useMemo(() => diffText(oldText, newText), [oldText, newText]);
  const identical =
    result.summary.addedLines === 0 && result.summary.removedLines === 0;
  const { t } = useTranslation();

  return (
    <div
      dir="ltr"
      className={`rounded-md border border-mid-gray/20 overflow-hidden py-1 ${className ?? ""}`}
    >
      {identical ? (
        <p className="px-2 py-1 text-xs text-secondary">
          {t("settings.studio.diff.identical")}
        </p>
      ) : (
        result.lines.map((line, index) => (
          <DiffLineRow
            key={`${line.kind}-${line.oldIndex}-${line.newIndex}-${index}`}
            line={line}
          />
        ))
      )}
    </div>
  );
};
