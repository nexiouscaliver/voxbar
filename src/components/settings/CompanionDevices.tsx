import React, { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import i18n from "../../i18n";
import { commands } from "@/bindings";
import type { CompanionStatus } from "@/bindings";
import { Button } from "../ui/Button";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";
import {
  parseAdvertisedHost,
  parseCompanionPort,
  tlsPairShape,
} from "./companionNetworkFields";

/**
 * Companion devices: phones/tablets on the same Wi-Fi (or tailnet) as
 * remote microphones and push-to-talk triggers, with this Mac as the
 * engine. The OFF-by-default toggle starts/stops the LAN server
 * (change_companion_devices_setting); the expanded panel carries:
 *
 * v1.6.0:
 * - the interface picker (every discovered IPv4 with its interface
 *   name: en0, utun9 Tailscale, bridges), Automatic keeping the
 *   LAN-preferred ranking as the default;
 * - manual override fields (advertised host or IP, port) validated
 *   client-side through companionNetworkFields (the Rust side
 *   re-validates) plus a restart-server button that applies them
 *   (restart_pending badges a running server with stale config);
 * - an optional manual TLS certificate pair (tailscale-issued or own
 *   certs); both paths or neither, and unset keeps the self-signed
 *   certificate behavior exactly;
 * - the pairing QR/URL derived from the chosen or manual address, so
 *   the panel always previews what the next start advertises.
 */
export const CompanionDevices: React.FC = () => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, isUpdating, refreshSettings } =
    useSettings();

  const enabled = getSetting("companion_devices_enabled") || false;
  const storedBindIp = getSetting("companion_bind_ip")?.trim() || "";
  const storedHost = getSetting("companion_advertised_host") ?? "";
  const storedPort = getSetting("companion_port") ?? 4177;
  const storedCert = getSetting("companion_tls_cert_path") ?? "";
  const storedKey = getSetting("companion_tls_key_path") ?? "";

  const [status, setStatus] = useState<CompanionStatus | null>(null);
  const [resetting, setResetting] = useState(false);
  const [restarting, setRestarting] = useState(false);
  // Draft echoes while editing; null renders the persisted value.
  const [hostDraft, setHostDraft] = useState<string | null>(null);
  const [portDraft, setPortDraft] = useState<string | null>(null);
  const [certDraft, setCertDraft] = useState<string | null>(null);
  const [keyDraft, setKeyDraft] = useState<string | null>(null);
  const [fieldError, setFieldError] = useState<string | null>(null);

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

  /**
   * Persist the network override (interface choice, host, port).
   * Validation is client-side first (a draft that does not parse never
   * leaves the panel); the backend re-validates and a rejection rolls
   * back with the shared toast, the mapCommandResult standard.
   */
  const applyNetwork = useCallback(
    async (bindIp: string | null, hostRaw: string, portRaw: string) => {
      const host = parseAdvertisedHost(hostRaw);
      if (host.kind === "invalid") {
        setFieldError(t("settings.advanced.companionDevices.hostInvalid"));
        return;
      }
      const port = parseCompanionPort(portRaw);
      if (port.kind === "invalid" || port.kind === "unset") {
        setFieldError(t("settings.advanced.companionDevices.portInvalid"));
        return;
      }
      setFieldError(null);
      try {
        const result = await commands.changeCompanionNetworkSetting(
          bindIp,
          host.kind === "set" ? host.value : null,
          port.value,
        );
        if (result.status === "error") {
          console.error("Failed to update companion network setting:", result.error);
          toast.error(i18n.t("toast.settingNotSaved", { error: result.error }));
          return;
        }
        setHostDraft(null);
        setPortDraft(null);
        await Promise.all([refreshSettings(), refreshStatus()]);
      } catch (error) {
        console.error("Failed to update companion network setting:", error);
      }
    },
    [refreshSettings, refreshStatus, t],
  );

  /** Persist the TLS pair: both paths or neither, and the pair must load
   * (the backend reads and parses both files before writing). */
  const applyTls = useCallback(
    async (certRaw: string, keyRaw: string) => {
      const shape = tlsPairShape(certRaw, keyRaw);
      if (shape === "half") {
        setFieldError(t("settings.advanced.companionDevices.tlsPairInvalid"));
        return;
      }
      setFieldError(null);
      try {
        const result = await commands.changeCompanionTlsSetting(
          shape === "both" ? certRaw.trim() : null,
          shape === "both" ? keyRaw.trim() : null,
        );
        if (result.status === "error") {
          console.error("Failed to update companion TLS setting:", result.error);
          toast.error(i18n.t("toast.settingNotSaved", { error: result.error }));
          return;
        }
        setCertDraft(null);
        setKeyDraft(null);
        await Promise.all([refreshSettings(), refreshStatus()]);
      } catch (error) {
        console.error("Failed to update companion TLS setting:", error);
      }
    },
    [refreshSettings, refreshStatus, t],
  );

  const restartServer = useCallback(async () => {
    setRestarting(true);
    try {
      const result = await commands.restartCompanionServer();
      if (result.status === "error") {
        // The panel's serverError line carries the failure (the command
        // stores it as the manager's last_error); the toast is instant
        // feedback, port conflicts included.
        toast.error(i18n.t("toast.settingNotSaved", { error: result.error }));
      }
      await Promise.all([refreshSettings(), refreshStatus()]);
    } catch (error) {
      console.error("Failed to restart companion server:", error);
    } finally {
      setRestarting(false);
    }
  }, [refreshSettings, refreshStatus]);

  const supported = status?.supported ?? true;
  const running = status?.running ?? false;
  const fingerprint = (status?.fingerprint ?? "").match(/.{1,8}/g) ?? [];
  const address = status?.url
    ? status.url.replace(/^https:\/\//, "").split("/")[0]
    : null;

  const hostValue = hostDraft ?? storedHost;
  const portValue = portDraft ?? String(storedPort);
  const certValue = certDraft ?? storedCert;
  const keyValue = keyDraft ?? storedKey;

  const kindLabel = (kind: string): string =>
    t(`settings.advanced.companionDevices.interfaceKind.${kind}`, {
      defaultValue: kind,
    });

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
            {status?.restart_pending && (
              <span className="ml-2 rounded bg-logo-primary/10 px-2 py-0.5 text-xs text-logo-primary">
                {t("settings.advanced.companionDevices.restartPending")}
              </span>
            )}
          </div>

          {status?.error && (
            <p className="text-xs text-error">
              {t("settings.advanced.companionDevices.serverError", {
                error: status.error,
              })}
            </p>
          )}

          {status?.url && status.qr_svg && (
            <div className="flex flex-col items-center gap-2">
              <p className="text-sm font-medium">
                {t("settings.advanced.companionDevices.qrTitle")}
              </p>
              {/* The QR is generated Rust-side from the pairing URL
                  (chosen or manual address included); the SVG comes
                  straight from the qrcode crate. */}
              <div
                className="w-[220px] [&_svg]:h-auto [&_svg]:w-full"
                role="img"
                aria-label={t("settings.advanced.companionDevices.qrTitle")}
                dangerouslySetInnerHTML={{ __html: status.qr_svg }}
              />
              <p className="text-xs text-secondary/70">
                {t("settings.advanced.companionDevices.qrHint")}
              </p>
              {(!running || status?.restart_pending) && (
                <p className="text-xs text-secondary/50">
                  {t("settings.advanced.companionDevices.qrPreviewNote")}
                </p>
              )}
            </div>
          )}

          {/* v1.6.0: every discovered interface, named and classified;
              Automatic keeps the LAN-preferred ranking as the default. */}
          <div className="space-y-2">
            <p className="text-xs font-medium text-secondary/70">
              {t("settings.advanced.companionDevices.interfaceTitle")}
            </p>
            <div className="space-y-1">
              <button
                type="button"
                onClick={() =>
                  void applyNetwork(null, hostValue, portValue)
                }
                className={`flex w-full items-center justify-between rounded-md border px-3 py-1.5 text-left text-xs ${
                  storedBindIp
                    ? "border-mid-gray/20 hover:border-mid-gray/40"
                    : "border-logo-primary/50 bg-logo-primary/5"
                }`}
              >
                <span>
                  {t("settings.advanced.companionDevices.interfaceAuto")}
                </span>
                {status?.bind_ip && !storedBindIp && (
                  <span className="font-mono text-secondary/70">
                    {status.bind_ip}
                  </span>
                )}
              </button>
              {status?.interfaces?.map((iface) => {
                const selected = storedBindIp === iface.ip;
                return (
                  <button
                    key={`${iface.name}-${iface.ip}`}
                    type="button"
                    onClick={() =>
                      void applyNetwork(iface.ip, hostValue, portValue)
                    }
                    className={`flex w-full items-center justify-between gap-3 rounded-md border px-3 py-1.5 text-left text-xs ${
                      selected
                        ? "border-logo-primary/50 bg-logo-primary/5"
                        : "border-mid-gray/20 hover:border-mid-gray/40"
                    }`}
                  >
                    <span className="font-mono">{iface.name}</span>
                    <span className="flex items-center gap-2">
                      <span className="rounded bg-mid-gray/15 px-1.5 py-0.5 text-[10px] uppercase tracking-wide text-secondary/70">
                        {kindLabel(iface.kind)}
                      </span>
                      <span className="font-mono text-secondary/70">
                        {iface.ip}
                      </span>
                    </span>
                  </button>
                );
              })}
              {status?.interfaces?.length === 0 && (
                <p className="text-xs text-secondary/50">
                  {t("settings.advanced.companionDevices.noInterfaces")}
                </p>
              )}
            </div>
            {status?.bind_error && (
              <p className="text-xs text-error">
                {t("settings.advanced.companionDevices.bindError", {
                  error: status.bind_error,
                })}
              </p>
            )}
          </div>

          {/* v1.6.0 manual overrides: advertised host or IP, and port.
              Validated before they ever leave the panel; a restart
              applies them. */}
          <div className="space-y-2">
            <p className="text-xs font-medium text-secondary/70">
              {t("settings.advanced.companionDevices.manualTitle")}
            </p>
            <div className="flex items-center gap-2">
              <label
                className="w-40 shrink-0 text-xs text-secondary/70"
                htmlFor="companion-advertised-host"
              >
                {t("settings.advanced.companionDevices.hostLabel")}
              </label>
              <input
                id="companion-advertised-host"
                type="text"
                className="min-w-0 flex-1 rounded-md border border-mid-gray/30 bg-background-ui/40 px-2 py-1 font-mono text-xs text-text focus:border-logo-primary focus:outline-none"
                placeholder={t("settings.advanced.companionDevices.hostPlaceholder")}
                value={hostValue}
                onChange={(e) => setHostDraft(e.target.value)}
                onBlur={() =>
                  void applyNetwork(storedBindIp || null, hostValue, portValue)
                }
              />
            </div>
            <div className="flex items-center gap-2">
              <label
                className="w-40 shrink-0 text-xs text-secondary/70"
                htmlFor="companion-port"
              >
                {t("settings.advanced.companionDevices.portLabel")}
              </label>
              <input
                id="companion-port"
                type="text"
                inputMode="numeric"
                className="w-24 rounded-md border border-mid-gray/30 bg-background-ui/40 px-2 py-1 font-mono text-xs text-text focus:border-logo-primary focus:outline-none"
                value={portValue}
                onChange={(e) => setPortDraft(e.target.value)}
                onBlur={() =>
                  void applyNetwork(storedBindIp || null, hostValue, portValue)
                }
              />
            </div>
            {fieldError && <p className="text-xs text-error">{fieldError}</p>}
            <p className="text-xs text-secondary/50">
              {t("settings.advanced.companionDevices.sharedNetworkNote")}
            </p>
          </div>

          {/* v1.6.0 optional TLS: a manual certificate pair for the
              listener; unset keeps the self-signed certificate. */}
          <div className="space-y-2">
            <p className="text-xs font-medium text-secondary/70">
              {t("settings.advanced.companionDevices.tlsTitle")}
            </p>
            <p className="text-xs text-secondary/50">
              {status?.tls_manual
                ? t("settings.advanced.companionDevices.tlsManual")
                : t("settings.advanced.companionDevices.tlsSelfSigned")}
            </p>
            <div className="flex items-center gap-2">
              <label
                className="w-40 shrink-0 text-xs text-secondary/70"
                htmlFor="companion-tls-cert"
              >
                {t("settings.advanced.companionDevices.tlsCertLabel")}
              </label>
              <input
                id="companion-tls-cert"
                type="text"
                className="min-w-0 flex-1 rounded-md border border-mid-gray/30 bg-background-ui/40 px-2 py-1 font-mono text-xs text-text focus:border-logo-primary focus:outline-none"
                placeholder="/path/to/cert.pem"
                value={certValue}
                onChange={(e) => setCertDraft(e.target.value)}
                onBlur={() => void applyTls(certValue, keyValue)}
              />
            </div>
            <div className="flex items-center gap-2">
              <label
                className="w-40 shrink-0 text-xs text-secondary/70"
                htmlFor="companion-tls-key"
              >
                {t("settings.advanced.companionDevices.tlsKeyLabel")}
              </label>
              <input
                id="companion-tls-key"
                type="text"
                className="min-w-0 flex-1 rounded-md border border-mid-gray/30 bg-background-ui/40 px-2 py-1 font-mono text-xs text-text focus:border-logo-primary focus:outline-none"
                placeholder="/path/to/key.pem"
                value={keyValue}
                onChange={(e) => setKeyDraft(e.target.value)}
                onBlur={() => void applyTls(certValue, keyValue)}
              />
            </div>
          </div>

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
            <Button
              variant="primary"
              size="sm"
              disabled={restarting}
              onClick={() => void restartServer()}
            >
              {t("settings.advanced.companionDevices.restartButton")}
            </Button>
          </div>
        </div>
      )}
    </div>
  );
};
