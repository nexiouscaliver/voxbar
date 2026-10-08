import { useEffect, useState, useCallback, useRef } from "react";
import { useTranslation } from "react-i18next";
import { platform } from "@tauri-apps/plugin-os";
import {
  checkAccessibilityPermission,
  requestAccessibilityPermission,
  checkMicrophonePermission,
  requestMicrophonePermission,
} from "tauri-plugin-macos-permissions-api";
import { toast } from "sonner";
import { commands } from "@/bindings";
import { useSettingsStore } from "@/stores/settingsStore";
import HandyTextLogo from "../icons/HandyTextLogo";
import { Keyboard, Mic, Check, Loader2 } from "lucide-react";
import { formatKeyCombination } from "../../lib/utils/keyboard";
import { useOsType } from "../../hooks/useOsType";

interface AccessibilityOnboardingProps {
  onComplete: () => void;
  preview?: boolean;
}

type PermissionStatus = "checking" | "needed" | "waiting" | "granted";
type PermissionPlatform = "macos" | "windows" | "other";

interface PermissionsState {
  accessibility: PermissionStatus;
  microphone: PermissionStatus;
}

const AccessibilityOnboarding: React.FC<AccessibilityOnboardingProps> = ({
  onComplete,
  preview = false,
}) => {
  const { t } = useTranslation();
  const refreshAudioDevices = useSettingsStore(
    (state) => state.refreshAudioDevices,
  );
  const refreshOutputDevices = useSettingsStore(
    (state) => state.refreshOutputDevices,
  );
  const [permissionPlatform, setPermissionPlatform] =
    useState<PermissionPlatform | null>(null);
  const [permissions, setPermissions] = useState<PermissionsState>({
    accessibility: "checking",
    microphone: "checking",
  });
  const pollingRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const timeoutRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const errorCountRef = useRef<number>(0);
  const MAX_POLLING_ERRORS = 3;
  // Mic-denial recovery: 60s after the mic card enters "waiting", the user
  // almost certainly denied or dismissed the system prompt; guidance
  // replaces the spinner. Polling keeps running, so a grant from System
  // Settings still completes onboarding automatically.
  const [micDenied, setMicDenied] = useState(false);
  const micWaitDeadlineRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const MIC_WAIT_DEADLINE_MS = 60_000;

  const settings = useSettingsStore((state) => state.settings);
  const osType = useOsType();
  const hotkeyBinding =
    settings?.bindings?.["transcribe"]?.current_binding || "option+space";
  const hotkeyDisplay = formatKeyCombination(hotkeyBinding, osType);

  const startMicDeadline = useCallback(() => {
    if (micWaitDeadlineRef.current) {
      clearTimeout(micWaitDeadlineRef.current);
    }
    setMicDenied(false);
    micWaitDeadlineRef.current = setTimeout(() => {
      setMicDenied(true);
    }, MIC_WAIT_DEADLINE_MS);
  }, []);

  const clearMicDeadline = useCallback(() => {
    if (micWaitDeadlineRef.current) {
      clearTimeout(micWaitDeadlineRef.current);
      micWaitDeadlineRef.current = null;
    }
    setMicDenied(false);
  }, []);

  // A grant detected by polling cancels the denial deadline.
  useEffect(() => {
    if (permissions.microphone !== "waiting") {
      clearMicDeadline();
    }
  }, [permissions.microphone, clearMicDeadline]);

  const isMacOS = permissionPlatform === "macos";
  const isWindows = permissionPlatform === "windows";
  const showMicrophonePermission = isMacOS || isWindows;
  const showAccessibilityPermission = isMacOS;

  const allGranted = isMacOS
    ? permissions.accessibility === "granted" &&
      permissions.microphone === "granted"
    : isWindows
      ? permissions.microphone === "granted"
      : true;

  const completeOnboarding = useCallback(async () => {
    await Promise.all([refreshAudioDevices(), refreshOutputDevices()]);
    timeoutRef.current = setTimeout(() => onComplete(), 300);
  }, [onComplete, refreshAudioDevices, refreshOutputDevices]);

  const hasWindowsMicrophoneAccess = useCallback(async (): Promise<boolean> => {
    const microphoneStatus =
      await commands.getWindowsMicrophonePermissionStatus();

    if (!microphoneStatus.supported) {
      return true;
    }

    return microphoneStatus.overall_access !== "denied";
  }, []);

  // Check platform and permission status on mount
  useEffect(() => {
    const currentPlatform = platform();
    const nextPlatform: PermissionPlatform =
      currentPlatform === "macos"
        ? "macos"
        : currentPlatform === "windows"
          ? "windows"
          : "other";

    setPermissionPlatform(nextPlatform);

    // Debug previews are intentionally inert: show the permission request UI
    // without checking or changing operating-system permissions.
    if (preview) {
      setPermissions({
        accessibility: nextPlatform === "macos" ? "needed" : "granted",
        microphone: nextPlatform === "other" ? "granted" : "needed",
      });
      return;
    }

    // Skip immediately on unsupported platforms
    if (nextPlatform === "other") {
      onComplete();
      return;
    }

    const checkInitial = async () => {
      if (nextPlatform === "macos") {
        try {
          const [accessibilityGranted, microphoneGranted] = await Promise.all([
            checkAccessibilityPermission(),
            checkMicrophonePermission(),
          ]);

          // If accessibility is granted, initialize Enigo and shortcuts
          if (accessibilityGranted) {
            try {
              await Promise.all([
                commands.initializeEnigo(),
                commands.initializeShortcuts(),
              ]);
            } catch (e) {
              console.warn("Failed to initialize after permission grant:", e);
            }
          }

          const newState: PermissionsState = {
            accessibility: accessibilityGranted ? "granted" : "needed",
            microphone: microphoneGranted ? "granted" : "needed",
          };

          setPermissions(newState);

          if (accessibilityGranted && microphoneGranted) {
            await completeOnboarding();
          }
        } catch (error) {
          console.error("Failed to check macOS permissions:", error);
          toast.error(t("onboarding.permissions.errors.checkFailed"));
          setPermissions({
            accessibility: "needed",
            microphone: "needed",
          });
        }

        return;
      }

      try {
        const microphoneGranted = await hasWindowsMicrophoneAccess();

        setPermissions({
          accessibility: "granted",
          microphone: microphoneGranted ? "granted" : "needed",
        });

        if (microphoneGranted) {
          await completeOnboarding();
        }
      } catch (error) {
        console.warn("Failed to check Windows microphone permissions:", error);
        setPermissions({
          accessibility: "granted",
          microphone: "granted",
        });
        await completeOnboarding();
      }
    };

    checkInitial();
  }, [completeOnboarding, hasWindowsMicrophoneAccess, onComplete, preview, t]);

  // Polling for permissions after user clicks a button
  const startPolling = useCallback(() => {
    if (pollingRef.current || permissionPlatform === null) return;

    pollingRef.current = setInterval(async () => {
      try {
        if (permissionPlatform === "windows") {
          const microphoneGranted = await hasWindowsMicrophoneAccess();

          if (microphoneGranted) {
            setPermissions((prev) => ({ ...prev, microphone: "granted" }));

            if (pollingRef.current) {
              clearInterval(pollingRef.current);
              pollingRef.current = null;
            }

            await completeOnboarding();
          }

          errorCountRef.current = 0;
          return;
        }

        const [accessibilityGranted, microphoneGranted] = await Promise.all([
          checkAccessibilityPermission(),
          checkMicrophonePermission(),
        ]);

        setPermissions((prev) => {
          const newState = { ...prev };

          if (accessibilityGranted && prev.accessibility !== "granted") {
            newState.accessibility = "granted";
            // Initialize Enigo and shortcuts when accessibility is granted
            Promise.all([
              commands.initializeEnigo(),
              commands.initializeShortcuts(),
            ]).catch((e) => {
              console.warn("Failed to initialize after permission grant:", e);
            });
          }

          if (microphoneGranted && prev.microphone !== "granted") {
            newState.microphone = "granted";
          }

          return newState;
        });

        // If both granted, stop polling, refresh audio devices, and proceed
        if (accessibilityGranted && microphoneGranted) {
          if (pollingRef.current) {
            clearInterval(pollingRef.current);
            pollingRef.current = null;
          }
          await completeOnboarding();
        }

        // Reset error count on success
        errorCountRef.current = 0;
      } catch (error) {
        console.error("Error checking permissions:", error);
        errorCountRef.current += 1;

        if (errorCountRef.current >= MAX_POLLING_ERRORS) {
          // Stop polling after too many consecutive errors
          if (pollingRef.current) {
            clearInterval(pollingRef.current);
            pollingRef.current = null;
          }
          // Reset any "waiting" card to "needed" so its Grant button
          // returns: a dead poller must never leave a spinner with no way
          // to restart the flow.
          setPermissions((prev) => ({
            accessibility:
              prev.accessibility === "waiting" ? "needed" : prev.accessibility,
            microphone:
              prev.microphone === "waiting" ? "needed" : prev.microphone,
          }));
          clearMicDeadline();
          toast.error(t("onboarding.permissions.errors.checkFailed"));
        }
      }
    }, 1000);
  }, [completeOnboarding, hasWindowsMicrophoneAccess, permissionPlatform, t]);

  // Cleanup polling and timeouts on unmount
  useEffect(() => {
    return () => {
      if (pollingRef.current) {
        clearInterval(pollingRef.current);
      }
      if (timeoutRef.current) {
        clearTimeout(timeoutRef.current);
      }
      if (micWaitDeadlineRef.current) {
        clearTimeout(micWaitDeadlineRef.current);
      }
    };
  }, []);

  const handleGrantAccessibility = async () => {
    if (preview) return;

    try {
      await requestAccessibilityPermission();
      setPermissions((prev) => ({ ...prev, accessibility: "waiting" }));
      startPolling();
    } catch (error) {
      console.error("Failed to request accessibility permission:", error);
      toast.error(t("onboarding.permissions.errors.requestFailed"));
    }
  };

  const handleGrantMicrophone = async () => {
    if (preview) return;

    try {
      if (isWindows) {
        await commands.openMicrophonePrivacySettings();
      } else {
        await requestMicrophonePermission();
      }

      setPermissions((prev) => ({ ...prev, microphone: "waiting" }));
      startMicDeadline();
      startPolling();
    } catch (error) {
      console.error("Failed to request microphone permission:", error);
      toast.error(t("onboarding.permissions.errors.requestFailed"));
    }
  };

  // Mic-denial guidance actions: open the System Settings microphone pane
  // (the only place a denied TCC grant can be flipped), or re-enter waiting.
  const handleOpenMicSettings = async () => {
    if (preview) return;
    try {
      await commands.openMicrophonePrivacySettings();
    } catch (error) {
      console.error("Failed to open microphone privacy settings:", error);
      toast.error(t("onboarding.permissions.errors.requestFailed"));
    }
  };

  const handleCheckAgain = () => {
    if (preview) return;
    setPermissions((prev) => ({ ...prev, microphone: "waiting" }));
    startMicDeadline();
    startPolling();
  };

  const isChecking =
    permissionPlatform === null ||
    (isMacOS &&
      permissions.accessibility === "checking" &&
      permissions.microphone === "checking") ||
    (isWindows && permissions.microphone === "checking");

  // Still checking platform/initial permissions
  if (isChecking) {
    return (
      <div className="h-screen w-full flex items-center justify-center">
        <Loader2 className="w-8 h-8 animate-spin text-text/50" />
      </div>
    );
  }

  // All permissions granted - show success briefly, with the dictation
  // hotkey so a first-time user knows how to actually start (it appears
  // nowhere else during onboarding).
  if (allGranted) {
    return (
      <div className="h-screen w-full flex flex-col items-center justify-center gap-4">
        <div className="p-4 rounded-full bg-emerald-500/20">
          <Check className="w-12 h-12 text-emerald-400" />
        </div>
        <p className="text-lg font-medium text-text">
          {t("onboarding.permissions.allGranted")}
        </p>
        <p className="text-sm text-text/60">
          {t("onboarding.hotkeyHint", { hotkey: hotkeyDisplay })}
        </p>
        <p className="text-xs text-text/40">
          {t("onboarding.hotkeyChangeHint")}
        </p>
      </div>
    );
  }

  // Show permissions request screen
  return (
    <div className="h-screen w-full flex flex-col p-6 gap-6 items-center justify-center">
      <div className="flex flex-col items-center gap-2">
        <HandyTextLogo width={200} />
      </div>

      <div className="max-w-md w-full flex flex-col items-center gap-4">
        <div className="text-center mb-2">
          <h2 className="text-xl font-semibold text-text mb-2">
            {t("onboarding.permissions.title")}
          </h2>
          <p className="text-text/70">
            {t("onboarding.permissions.description")}
          </p>
        </div>

        {/* Microphone Permission Card */}
        {showMicrophonePermission && (
          <div className="w-full p-4 rounded-lg bg-white/5 border border-mid-gray/20">
            <div className="flex items-center gap-4">
              <div className="p-3 rounded-full bg-logo-primary/20 shrink-0">
                <Mic className="w-6 h-6 text-logo-primary" />
              </div>
              <div className="flex-1 min-w-0">
                <h3 className="font-medium text-text">
                  {t("onboarding.permissions.microphone.title")}
                </h3>
                <p className="text-sm text-text/60 mb-3">
                  {t("onboarding.permissions.microphone.description")}
                </p>
                {permissions.microphone === "granted" ? (
                  <div className="flex items-center gap-2 text-emerald-400 text-sm">
                    <Check className="w-4 h-4" />
                    {t("onboarding.permissions.granted")}
                  </div>
                ) : micDenied ? (
                  <div className="space-y-2">
                    <p className="text-sm text-text/70">
                      {t("onboarding.permissions.microphone.deniedGuidance")}
                    </p>
                    <div className="flex flex-wrap gap-2">
                      <button
                        onClick={handleOpenMicSettings}
                        className="px-4 py-2 rounded-lg bg-logo-primary hover:bg-logo-primary/90 text-white text-sm font-medium transition-colors"
                      >
                        {t("onboarding.permissions.microphone.openSettings")}
                      </button>
                      <button
                        onClick={handleCheckAgain}
                        className="px-4 py-2 rounded-lg border border-text/20 hover:bg-text/10 text-text text-sm font-medium transition-colors"
                      >
                        {t("onboarding.permissions.microphone.checkAgain")}
                      </button>
                    </div>
                  </div>
                ) : permissions.microphone === "waiting" ? (
                  <div className="flex items-center gap-2 text-text/50 text-sm">
                    <Loader2 className="w-4 h-4 animate-spin" />
                    {t("onboarding.permissions.waiting")}
                  </div>
                ) : (
                  <button
                    onClick={handleGrantMicrophone}
                    className="px-4 py-2 rounded-lg bg-logo-primary hover:bg-logo-primary/90 text-white text-sm font-medium transition-colors"
                  >
                    {isWindows
                      ? t("accessibility.openSettings")
                      : t("onboarding.permissions.grant")}
                  </button>
                )}
              </div>
            </div>
          </div>
        )}

        {/* Accessibility Permission Card */}
        {showAccessibilityPermission && (
          <div className="w-full p-4 rounded-lg bg-white/5 border border-mid-gray/20">
            <div className="flex items-center gap-4">
              <div className="p-3 rounded-full bg-logo-primary/20 shrink-0">
                <Keyboard className="w-6 h-6 text-logo-primary" />
              </div>
              <div className="flex-1 min-w-0">
                <h3 className="font-medium text-text">
                  {t("onboarding.permissions.accessibility.title")}
                </h3>
                <p className="text-sm text-text/60 mb-3">
                  {t("onboarding.permissions.accessibility.description")}
                </p>
                {permissions.accessibility === "granted" ? (
                  <div className="flex items-center gap-2 text-emerald-400 text-sm">
                    <Check className="w-4 h-4" />
                    {t("onboarding.permissions.granted")}
                  </div>
                ) : permissions.accessibility === "waiting" ? (
                  <div className="flex items-center gap-2 text-text/50 text-sm">
                    <Loader2 className="w-4 h-4 animate-spin" />
                    {t("onboarding.permissions.waiting")}
                  </div>
                ) : (
                  <button
                    onClick={handleGrantAccessibility}
                    className="px-4 py-2 rounded-lg bg-logo-primary hover:bg-logo-primary/90 text-white text-sm font-medium transition-colors"
                  >
                    {t("onboarding.permissions.grant")}
                  </button>
                )}
              </div>
            </div>
          </div>
        )}
      </div>
    </div>
  );
};

export default AccessibilityOnboarding;
