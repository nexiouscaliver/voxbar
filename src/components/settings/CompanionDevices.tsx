import React, { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { commands } from "@/bindings";
import type { CompanionStatus } from "@/bindings";
import { Button } from "../ui/Button";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";

/**
 * Companion devices: phones/tablets on the same Wi-Fi as remote microphones
 * and push-to-talk triggers, with this Mac as the engine. The OFF-by-default
 * toggle starts/stops the LAN server (change_companion_devices_setting);
 * the expanded panel carries the pairing QR (token in the URL hash), the
 * certificate fingerprint for the phone's first-visit interstitial, the
 * connected device list, and reset-pairing.
 */
export const CompanionDevices: React.FC = () => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, isUpdating } = useSettings();

  const enabled = getSetting("companion_devices_enabled") || false;
  const [status, setStatus] = useState<CompanionStatus | null>(null);
  const [resetting, setResetting] = useState(false);

  const refreshStatus = useCallback(async () => {
    try {
      const result = await commands.getCompanionStatus();
      setStatus(result);
    } catch (error) {
      console.error("Failed to load companion status:", error);
    }
  }, []);

  useEffect(() => {
    void refreshStatus();
  }, [refreshStatus, enabled]);

  // While the server runs, phones pair and disconnect on their own schedule;
  // a light poll keeps the device list and any server error current.
  useEffect(() => {
    if (!enabled) return;
    const id = window.setInterval(() => void refreshStatus(), 5000);
    return () => window.clearInterval(id);
  }, [enabled, refreshStatus]);

  const resetPairing = useCallback(async () => {
    setResetting(true);
    try {
      await commands.resetCompanionPairing();
      await refreshStatus();
    } catch (error) {
      console.error("Failed to reset companion pairing:", error);
    } finally {
      setResetting(false);
    }
  }, [refreshStatus]);

  const supported = status?.supported ?? true;
  const running = status?.running ?? false;
  const fingerprint = (status?.fingerprint ?? "").match(/.{1,8}/g) ?? [];
  const address = status?.url
    ? status.url.replace(/^https:\/\//, "").split("/")[0]
    : null;

  return (
    <div className="space-y-3">
      <ToggleSwitch
        checked={enabled}
        onChange={(value) => updateSetting("companion_devices_enabled", value)}
        isUpdating={isUpdating("companion_devices_enabled")}
        label={t("settings.advanced.companionDevices.label")}
        description={t("settings.advanced.companionDevices.description")}
        descriptionMode="tooltip"
        grouped
        tooltipPosition="bottom"
      />

      {enabled && !supported && (
        <p className="pl-4 text-xs text-error">
          {t("settings.advanced.companionDevices.unavailable")}
        </p>
      )}

      {enabled && (
        <div className="space-y-4 rounded-lg border border-mid-gray/20 p-4">
          <div className="flex items-center gap-2 text-sm">
            <span
              className={`inline-block h-2 w-2 rounded-full ${
                running ? "bg-logo-primary" : "bg-mid-gray/50"
              }`}
            />
            {running
              ? t("settings.advanced.companionDevices.statusRunning")
              : t("settings.advanced.companionDevices.statusStopped")}
          </div>

          {status?.error && (
            <p className="text-xs text-error">
              {t("settings.advanced.companionDevices.serverError", {
                error: status.error,
              })}
            </p>
          )}

          {running && status?.qr_svg && (
            <div className="flex flex-col items-center gap-2">
              <p className="text-sm font-medium">
                {t("settings.advanced.companionDevices.qrTitle")}
              </p>
              {/* The QR is generated Rust-side from the pairing URL; the
                  SVG comes straight from the qrcode crate. */}
              <div
                className="w-[220px] [&_svg]:h-auto [&_svg]:w-full"
                role="img"
                aria-label={t("settings.advanced.companionDevices.qrTitle")}
                dangerouslySetInnerHTML={{ __html: status.qr_svg }}
              />
              <p className="text-xs text-secondary/70">
                {t("settings.advanced.companionDevices.qrHint")}
              </p>
            </div>
          )}

          <dl className="space-y-2 text-sm">
            <div className="flex items-baseline justify-between gap-3">
              <dt className="text-secondary/70">
                {t("settings.advanced.companionDevices.addressLabel")}
              </dt>
              <dd className="font-mono text-xs">{address ?? "-"}</dd>
            </div>
            <div className="flex items-baseline justify-between gap-3">
              <dt className="text-secondary/70">
                {t("settings.advanced.companionDevices.portLabel")}
              </dt>
              <dd className="font-mono text-xs">{status?.port ?? "-"}</dd>
            </div>
            <div className="flex items-baseline justify-between gap-3">
              <dt className="text-secondary/70">
                {t("settings.advanced.companionDevices.devicesLabel")}
              </dt>
              <dd className="text-right text-xs">
                {status?.devices?.length ? (
                  <span>{status.devices.join(", ")}</span>
                ) : (
                  <span className="text-secondary/70">
                    {t("settings.advanced.companionDevices.noDevices")}
                  </span>
                )}
              </dd>
            </div>
            {fingerprint.length > 0 && (
              <div className="flex flex-col gap-1">
                <dt className="text-secondary/70">
                  {t("settings.advanced.companionDevices.fingerprintLabel")}
                </dt>
                <dd className="break-all font-mono text-[11px] leading-4 text-secondary">
                  {fingerprint.join(" ")}
                </dd>
              </div>
            )}
          </dl>

          <p className="text-xs text-secondary/70">
            {t("settings.advanced.companionDevices.firewallNote")}
          </p>
          <p className="text-xs text-secondary/50">
            {t("settings.advanced.companionDevices.offNote")}
          </p>

          <div className="flex items-center gap-2">
            <Button
              variant="secondary"
              size="sm"
              onClick={() => void refreshStatus()}
            >
              {t("settings.advanced.companionDevices.refresh")}
            </Button>
            <Button
              variant="secondary"
              size="sm"
              disabled={resetting}
              onClick={() => void resetPairing()}
            >
              {t("settings.advanced.companionDevices.resetPairing")}
            </Button>
          </div>
        </div>
      )}
    </div>
  );
};
