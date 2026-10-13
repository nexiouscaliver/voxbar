import React, { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { CircleAlert, Info, Trash2 } from "lucide-react";
import { ToggleSwitch } from "../../ui/ToggleSwitch";
import { SettingsGroup } from "../../ui/SettingsGroup";
import { Button } from "../../ui/Button";
import { noticeMessage } from "@/lib/noticeMessage";
import {
  clearNotificationLog,
  getNotificationLog,
  subscribeNotificationLog,
  type NotificationLogEntry,
} from "@/lib/notificationLog";
import {
  getOsNotificationPrefs,
  setOsNotificationPrefs,
  type OsNotificationPrefs,
} from "@/lib/notificationPrefs";

// v1.6.0 W1-L4: the Notifications settings section. Two parts:
// 1. the OS-notification toggles (master, default on, plus the
//    outcomes-vs-errors-only sub-toggle; off restores the pre-v1.6.0
//    notification behavior exactly - see planNoticeDispatch in
//    noticeRouting.ts), and
// 2. the in-app notification log (last 50 error/outcome notices with a
//    clear-all action), the durable backstop so a missed toast is never
//    lost even with OS notifications disabled.

const formatLogTime = (ms: number, locale: string): string => {
  try {
    return new Intl.DateTimeFormat(locale, {
      month: "short",
      day: "numeric",
      hour: "2-digit",
      minute: "2-digit",
    }).format(new Date(ms));
  } catch {
    return new Date(ms).toISOString();
  }
};

export const NotificationsSettings: React.FC = () => {
  const { t, i18n } = useTranslation();
  // Local mirror of the prefs store: this screen is the only writer, and
  // the store remains the emit-time source of truth for the notice router.
  const [prefs, setPrefs] = useState(getOsNotificationPrefs());
  const [entries, setEntries] =
    useState<NotificationLogEntry[]>(getNotificationLog());

  useEffect(() => subscribeNotificationLog(setEntries), []);

  const writePrefs = (next: OsNotificationPrefs) => {
    setPrefs(next);
    setOsNotificationPrefs(next);
  };

  return (
    <div className="max-w-3xl w-full mx-auto space-y-6">
      <SettingsGroup title={t("settings.notifications.title")}>
        <ToggleSwitch
          checked={prefs.enabled}
          onChange={(enabled) => writePrefs({ ...prefs, enabled })}
          label={t("settings.notifications.toggle.label")}
          description={t("settings.notifications.toggle.description")}
          descriptionMode="tooltip"
          grouped={true}
        />
        {/* The dependency on the master toggle is stated on the row itself
            (inline, not behind a hover tooltip) whenever it is the active
            constraint - a disabled row that hides its reason reads as
            broken rather than governed. */}
        <ToggleSwitch
          checked={prefs.enabled && prefs.outcomes}
          disabled={!prefs.enabled}
          onChange={(outcomes) => writePrefs({ ...prefs, outcomes })}
          label={t("settings.notifications.outcomes.label")}
          description={
            prefs.enabled
              ? t("settings.notifications.outcomes.description")
              : t("settings.notifications.outcomes.requiresMaster")
          }
          descriptionMode={prefs.enabled ? "tooltip" : "inline"}
          grouped={true}
        />
      </SettingsGroup>

      <SettingsGroup
        title={t("settings.notifications.log.title")}
        description={t("settings.notifications.log.description")}
      >
        <div className="px-4 py-3 space-y-3">
          {entries.length === 0 ? (
            <p className="text-sm text-secondary">
              {t("settings.notifications.log.empty")}
            </p>
          ) : (
            <ul className="space-y-1.5 max-h-96 overflow-y-auto">
              {entries.map((entry) => {
                const message = noticeMessage(t, entry.code, entry.detail);
                return (
                  <li
                    key={entry.id}
                    className="flex items-start gap-2 text-sm py-1"
                  >
                    {entry.kind === "error" ? (
                      <CircleAlert
                        className="w-4 h-4 mt-0.5 shrink-0 text-red-500"
                        aria-hidden="true"
                      />
                    ) : (
                      <Info
                        className="w-4 h-4 mt-0.5 shrink-0 text-secondary"
                        aria-hidden="true"
                      />
                    )}
                    <span className="min-w-0 flex-1 break-words">
                      {message ?? entry.code}
                    </span>
                    <span className="text-xs text-secondary whitespace-nowrap pt-0.5">
                      {formatLogTime(entry.receivedAt, i18n.language)}
                    </span>
                  </li>
                );
              })}
            </ul>
          )}
          {/* Clear all only exists when there is something to clear: the
              empty log spent its strongest bottom-right position on a
              correctly-disabled, dead button. */}
          {entries.length > 0 && (
            <div className="flex justify-end">
              <Button
                onClick={clearNotificationLog}
                variant="secondary"
                size="sm"
                className="flex items-center gap-2"
                title={t("settings.notifications.log.clear")}
              >
                <Trash2 className="w-4 h-4" />
                <span>{t("settings.notifications.log.clear")}</span>
              </Button>
            </div>
          )}
        </div>
      </SettingsGroup>
    </div>
  );
};
