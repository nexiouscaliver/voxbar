import React, { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { useSettings } from "../../hooks/useSettings";
import { Input } from "../ui/Input";
import { SettingContainer } from "../ui/SettingContainer";
import { parseHistoryLimit } from "./historyLimitInput";

interface HistoryLimitProps {
  descriptionMode?: "tooltip" | "inline";
  grouped?: boolean;
}

// How long the field stays quiet before a typed value is committed. Long
// enough that typing "150" fires one commit, short enough that a spinner
// click or a single-key edit feels immediate.
const COMMIT_DEBOUNCE_MS = 700;

export const HistoryLimit: React.FC<HistoryLimitProps> = ({
  descriptionMode = "inline",
  grouped = false,
}) => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, isUpdating } = useSettings();

  const historyLimit = getSetting("history_limit") ?? 5;

  // Local draft while the field is being edited. The backend command is
  // destructive (every committed value deletes unsaved entries and their
  // WAV recordings down to that count), so partial keystrokes are held
  // here and only a quiet, finished value is ever committed - on debounce,
  // blur, or Enter. Escape reverts the draft to the persisted value.
  const [draft, setDraft] = useState<string | null>(null);

  // Refs keep the commit path free of stale closures: the unmount flush
  // and the debounce timer both outlive the render that created them.
  const draftRef = useRef<string | null>(null);
  draftRef.current = draft;
  const historyLimitRef = useRef(historyLimit);
  historyLimitRef.current = historyLimit;
  const updateSettingRef = useRef(updateSetting);
  updateSettingRef.current = updateSetting;
  const commitTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const clearCommitTimer = () => {
    if (commitTimer.current !== null) {
      clearTimeout(commitTimer.current);
      commitTimer.current = null;
    }
  };

  const commitDraft = (raw: string | null) => {
    if (raw === null) {
      return;
    }
    const parsed = parseHistoryLimit(raw);
    if (parsed.ok && parsed.value !== historyLimitRef.current) {
      void updateSettingRef.current("history_limit", parsed.value);
    }
    setDraft(null);
  };

  // Flush or drop the pending draft exactly once on unmount: a typed value
  // is committed (closing the panel must not silently discard it), and the
  // timer is always cleared.
  useEffect(
    () => () => {
      if (commitTimer.current !== null) {
        clearCommitTimer();
        commitDraft(draftRef.current);
      }
    },
    [],
  );

  const scheduleCommit = (raw: string) => {
    clearCommitTimer();
    commitTimer.current = setTimeout(() => {
      commitTimer.current = null;
      commitDraft(draftRef.current);
    }, COMMIT_DEBOUNCE_MS);
  };

  const handleChange = (event: React.ChangeEvent<HTMLInputElement>) => {
    const raw = event.target.value;
    setDraft(raw);
    scheduleCommit(raw);
  };

  const handleKeyDown = (event: React.KeyboardEvent<HTMLInputElement>) => {
    if (event.key === "Enter") {
      event.preventDefault();
      clearCommitTimer();
      commitDraft(draftRef.current);
    } else if (event.key === "Escape") {
      event.preventDefault();
      clearCommitTimer();
      setDraft(null);
    }
  };

  const handleBlur = () => {
    clearCommitTimer();
    commitDraft(draftRef.current);
  };

  return (
    <SettingContainer
      title={t("settings.debug.historyLimit.title")}
      description={t("settings.debug.historyLimit.description")}
      descriptionMode={descriptionMode}
      grouped={grouped}
      layout="horizontal"
    >
      <div className="flex items-center space-x-2">
        <Input
          type="number"
          min="0"
          max="1000"
          value={draft ?? historyLimit}
          onChange={handleChange}
          onBlur={handleBlur}
          onKeyDown={handleKeyDown}
          disabled={isUpdating("history_limit")}
          className="w-20"
        />
        <span className="text-sm text-text">
          {t("settings.debug.historyLimit.entries")}
        </span>
      </div>
    </SettingContainer>
  );
};
