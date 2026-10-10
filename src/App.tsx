import {
  useEffect,
  useLayoutEffect,
  useState,
  useRef,
  type ReactNode,
} from "react";
import { toast, Toaster } from "sonner";
import { useTranslation } from "react-i18next";
import { listen } from "@tauri-apps/api/event";
import { platform } from "@tauri-apps/plugin-os";
import {
  checkAccessibilityPermission,
  checkMicrophonePermission,
} from "tauri-plugin-macos-permissions-api";
import {
  ModelFallbackEvent,
  ModelStateEvent,
  RecordingErrorEvent,
} from "./lib/types/events";
import "./App.css";
import AccessibilityPermissions from "./components/AccessibilityPermissions";
import SecureInputWarning from "./components/SecureInputWarning";
import Footer from "./components/footer";
import Onboarding, { AccessibilityOnboarding } from "./components/onboarding";
import {
  DebugSettings,
  type OnboardingPreviewStep,
} from "./components/settings";
import { ErrorBoundary } from "./components/ErrorBoundary";
import { Sidebar, SidebarSection, SECTIONS_CONFIG } from "./components/Sidebar";
import { SettingsSearch } from "./components/settings/SettingsSearch";
import { WhatsNewGate } from "./components/whats-new";
import { runUpdateCheck } from "./components/update-checker/updaterFlow";
import { useSettings } from "./hooks/useSettings";
import { useSettingsStore } from "./stores/settingsStore";
import { commands, events, type SkipReason } from "@/bindings";
import { getLanguageDirection, initializeRTL } from "@/lib/utils/rtl";
import {
  shouldToast,
  skipToastKey,
} from "./components/settings/post-processing/skipToastDedupe";

type OnboardingStep = "accessibility" | "model" | "done";

// Stable identity so preview effects do not re-run due to callback changes.
const NOOP = () => {};

// Post-process skip reasons already toasted this app session (spec 7.3:
// at most one toast per reason per session). Module-level by design so
// re-renders and remounts never reset the dedupe window.
const toastedSkipReasons = new Set<SkipReason>();

const renderSettingsContent = (
  section: SidebarSection,
  onPreviewOnboarding: (step: OnboardingPreviewStep) => void,
) => {
  if (section === "debug") {
    return <DebugSettings onPreviewOnboarding={onPreviewOnboarding} />;
  }

  const ActiveComponent =
    SECTIONS_CONFIG[section]?.component || SECTIONS_CONFIG.general.component;
  return <ActiveComponent />;
};

function App() {
  const { t, i18n } = useTranslation();
  const [onboardingStep, setOnboardingStep] = useState<OnboardingStep | null>(
    null,
  );
  const [onboardingPreview, setOnboardingPreview] =
    useState<OnboardingPreviewStep | null>(null);
  // Track if this is a returning user who just needs to grant permissions
  // (vs a new user who needs full onboarding including model selection)
  const [isReturningUser, setIsReturningUser] = useState(false);
  const [currentSection, setCurrentSection] =
    useState<SidebarSection>("general");
  const { settings, updateSetting } = useSettings();
  const direction = getLanguageDirection(i18n.language);
  const refreshAudioDevices = useSettingsStore(
    (state) => state.refreshAudioDevices,
  );
  const refreshOutputDevices = useSettingsStore(
    (state) => state.refreshOutputDevices,
  );
  const hasCompletedPostOnboardingInit = useRef(false);
  const settingsScrollRef = useRef<HTMLDivElement>(null);
  const isShowingOnboarding =
    onboardingPreview !== null ||
    onboardingStep === "accessibility" ||
    onboardingStep === "model";

  // Classic scrollbars consume layout space. Reserve a matching gutter on the
  // opposite edge while onboarding is visible so its content stays centered in
  // the physical window. Overlay scrollbars ignore scrollbar-gutter.
  useLayoutEffect(() => {
    const attribute = "data-onboarding-active";
    document.documentElement.toggleAttribute(attribute, isShowingOnboarding);
    return () => document.documentElement.removeAttribute(attribute);
  }, [isShowingOnboarding]);

  // Reset the scroll position whenever the active section changes.
  useLayoutEffect(() => {
    settingsScrollRef.current?.scrollTo({ top: 0 });
  }, [currentSection]);

  useEffect(() => {
    checkOnboardingStatus();
  }, []);

  // Initialize RTL direction when language changes
  useEffect(() => {
    initializeRTL(i18n.language);
  }, [i18n.language]);

  // Initialize Enigo, shortcuts, and refresh audio devices when main app loads
  useEffect(() => {
    if (onboardingStep === "done" && !hasCompletedPostOnboardingInit.current) {
      hasCompletedPostOnboardingInit.current = true;
      Promise.all([
        commands.initializeEnigo(),
        commands.initializeShortcuts(),
      ]).catch((e) => {
        console.warn("Failed to initialize:", e);
      });
      refreshAudioDevices();
      refreshOutputDevices();
    }
  }, [onboardingStep, refreshAudioDevices, refreshOutputDevices]);

  // Handle keyboard shortcuts for debug mode toggle
  useEffect(() => {
    const handleKeyDown = (event: KeyboardEvent) => {
      // Check for Ctrl+Shift+D (Windows/Linux) or Cmd+Shift+D (macOS)
      const isDebugShortcut =
        event.shiftKey &&
        event.key.toLowerCase() === "d" &&
        (event.ctrlKey || event.metaKey);

      if (isDebugShortcut) {
        event.preventDefault();
        const currentDebugMode = settings?.debug_mode ?? false;
        updateSetting("debug_mode", !currentDebugMode);
      }
    };

    // Add event listener when component mounts
    document.addEventListener("keydown", handleKeyDown);

    // Cleanup event listener when component unmounts
    return () => {
      document.removeEventListener("keydown", handleKeyDown);
    };
  }, [settings?.debug_mode, updateSetting]);

  // Listen for recording errors from the backend and show a toast
  useEffect(() => {
    const unlisten = listen<RecordingErrorEvent>("recording-error", (event) => {
      const { error_type, detail } = event.payload;

      if (error_type === "microphone_permission_denied") {
        const currentPlatform = platform();
        const platformKey = `errors.micPermissionDenied.${currentPlatform}`;
        const description = t(platformKey, {
          defaultValue: t("errors.micPermissionDenied.generic"),
        });
        toast.error(t("errors.micPermissionDeniedTitle"), { description });
      } else if (error_type === "no_input_device") {
        toast.error(t("errors.noInputDeviceTitle"), {
          description: t("errors.noInputDevice"),
        });
      } else if (error_type === "no_model_selected") {
        // A hotkey press with no usable model: explain it instead of
        // silently aborting with only a log line (which read as a broken
        // hotkey).
        toast.error(t("errors.noModelSelectedTitle"), {
          description: t("errors.noModelSelected"),
        });
      } else {
        toast.error(
          t("errors.recordingFailed", { error: detail ?? "Unknown error" }),
        );
      }
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, [t]);

  // Listen for paste failures and show a toast.
  // The technical error detail is logged to voxbar.log on the Rust side
  // (see actions.rs `error!("Failed to paste transcription: ...")`),
  // so we show a localized, user-friendly message here instead of the raw error.
  useEffect(() => {
    const unlisten = listen("paste-error", () => {
      toast.error(t("errors.pasteFailedTitle"), {
        description: t("errors.pasteFailed"),
      });
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, [t]);

  // Listen for transcription failures and show a toast.
  // The payload is the backend error message (also logged to voxbar.log).
  useEffect(() => {
    const unlisten = listen<string>("transcription-error", (event) => {
      toast.error(t("errors.transcriptionFailedTitle"), {
        description: event.payload,
      });
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, [t]);

  // Listen for model loading failures and show a toast
  useEffect(() => {
    const unlisten = listen<ModelStateEvent>("model-state-changed", (event) => {
      if (event.payload.event_type === "loading_failed") {
        toast.error(
          t("errors.modelLoadFailed", {
            model:
              event.payload.model_name || t("errors.modelLoadFailedUnknown"),
          }),
          {
            description: event.payload.error,
          },
        );
      }
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, [t]);

  // RAM auto-fallback fired: the selected model did not fit free memory and
  // a smaller already-downloaded model is being loaded for this dictation.
  // Transient warning toast; the tray already reflects the resident model.
  useEffect(() => {
    const unlisten = listen<ModelFallbackEvent>("model-fallback", (event) => {
      toast.warning(t("errors.modelFallbackTitle"), {
        description: t("errors.modelFallback", {
          model: event.payload.fallback_model_name,
        }),
      });
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, [t]);

  // Local post-process fell back to the raw transcript (spec 7.3): at most
  // one toast per reason per app session, so a misbehaving engine cannot
  // spam the user on every dictation. The memory_gate variant carries the
  // formatted refusal numbers in detail; the raw transcript is always
  // pasted regardless.
  useEffect(() => {
    const unlisten = events.postProcessSkipEvent.listen((event) => {
      const { reason, detail } = event.payload;
      if (!shouldToast(toastedSkipReasons, reason)) return;
      const key = `toast.postProcessSkip.${skipToastKey(reason)}`;
      // Engine failures and timeouts get error-level visibility; every
      // other skip is an expected, recoverable condition.
      const toastFn =
        reason === "engine_failed" || reason === "timeout"
          ? toast.error
          : toast.info;
      if (reason === "memory_gate") {
        toastFn(t(key, { detail: detail ?? "" }));
      } else {
        toastFn(t(key));
      }
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, [t]);

  // Tray "Check for Updates": the backend emits request-update-check and the
  // shared flow in components/update-checker/updaterFlow.ts does check ->
  // confirm -> download -> install -> relaunch. Mounted at App level, NOT in
  // UpdateChecker: that component lives in the Footer, which is not rendered
  // during onboarding or debug previews, so a Footer-mounted listener would
  // silently drop tray clicks there. The main window is hidden, never
  // destroyed, on close, so this listener receives the event in every app
  // state. The flow itself no-ops when HANDY_DISABLE_UPDATER locks checks
  // (defense in depth; the backend removes the tray item when locked).
  useEffect(() => {
    const unlisten = listen("request-update-check", () => {
      void runUpdateCheck({ trigger: "manual" });
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, []);

  // macOS App Translocation: the app launched with a quarantine flag
  // straight from its download folder, so it runs from a read-only mount
  // where the updater's install step fails with "read-only file system".
  // Fresh installs see this toast during onboarding (window guaranteed
  // visible); long-term dwellers see it whenever they next open the window
  // via this same check on mount.
  useEffect(() => {
    void (async () => {
      try {
        const result = await commands.isAppTranslocated();
        if (result.status === "ok" && result.data) {
          toast.warning(t("app.translocatedTitle"), {
            description: t("app.translocatedDescription"),
            duration: 15000,
          });
        }
      } catch (error) {
        console.error("Failed to check App Translocation state:", error);
      }
    })();
  }, []);

  // The command-mode key pressed with no live dictation: the binding never
  // starts a recording, so without feedback the press reads as "commands
  // stopped working". Rate-limited: a key held through auto-repeat would
  // otherwise stack toasts.
  const lastCommandIdleToast = useRef(0);
  useEffect(() => {
    const unlisten = listen("command-mode-no-session", () => {
      const now = Date.now();
      if (now - lastCommandIdleToast.current < 2000) return;
      lastCommandIdleToast.current = now;
      toast.info(t("app.commandNoSession"), { duration: 4000 });
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, [t]);

  // No-op key feedback from the notice channel: idle delete/undo presses,
  // live-session presses with no buffer yet, cross-binding presses
  // swallowed to protect a recording, and the Linux setup warnings (Wayland
  // hotkey backend, GNOME overlay fallback). These have no other toast, so
  // this listener is their main-window surface; the overlay shows the same
  // message on its card when visible. Rate-limited per code (2s, the same
  // window command-mode-no-session uses) so auto-repeat cannot stack; the
  // Linux setup warnings stay up longer because they carry instructions.
  const lastNoopNoticeToast = useRef<Record<string, number>>({});
  useEffect(() => {
    const unlisten = events.overlayNoticeEvent.listen((event) => {
      const { code } = event.payload;
      const keySuffix =
        code === "delete_last_word_no_session"
          ? "deleteLastWordNoSession"
          : code === "delete_last_word_no_buffer"
            ? "deleteLastWordNoBuffer"
            : code === "undo_no_session"
              ? "undoNoSession"
              : code === "undo_no_buffer"
                ? "undoNoBuffer"
                : code === "binding_busy"
                  ? "bindingBusy"
                  : code === "wayland_tauri_hotkeys"
                    ? "waylandTauriHotkeys"
                    : code === "gnome_overlay_fallback"
                      ? "gnomeOverlayFallback"
                      : null;
      if (keySuffix === null) return;
      const duration =
        code === "wayland_tauri_hotkeys" || code === "gnome_overlay_fallback"
          ? 12000
          : 4000;
      const now = Date.now();
      if (now - (lastNoopNoticeToast.current[code] ?? 0) < 2000) return;
      lastNoopNoticeToast.current[code] = now;
      toast.info(t(`overlay.notice.${keySuffix}`), { duration });
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, [t]);

  // Tray "Unload After → Custom…": jump to the Advanced settings section and
  // focus the custom-seconds field. The window event is re-dispatched after a
  // short delay so the section (and the field) has mounted before it arrives.
  useEffect(() => {
    const unlisten = listen("open-settings-unload-timeout", () => {
      setCurrentSection("advanced");
      setTimeout(() => {
        window.dispatchEvent(new CustomEvent("voxbar:focus-unload-timeout"));
      }, 150);
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, []);

  const revealMainWindowForPermissions = async () => {
    try {
      await commands.showMainWindowCommand();
    } catch (e) {
      console.warn("Failed to show main window for permission onboarding:", e);
    }
  };

  const checkOnboardingStatus = async () => {
    try {
      const settingsResult = await commands.getAppSettings();
      const hasCompletedOnboarding =
        settingsResult.status === "ok" &&
        settingsResult.data.onboarding_completed === true;
      const currentPlatform = platform();

      if (hasCompletedOnboarding) {
        // Returning user - check if they need to grant permissions first
        setIsReturningUser(true);

        if (currentPlatform === "macos") {
          try {
            const [hasAccessibility, hasMicrophone] = await Promise.all([
              checkAccessibilityPermission(),
              checkMicrophonePermission(),
            ]);
            if (!hasAccessibility || !hasMicrophone) {
              await revealMainWindowForPermissions();
              setOnboardingStep("accessibility");
              return;
            }
          } catch (e) {
            console.warn("Failed to check macOS permissions:", e);
            // If we can't check, proceed to main app and let them fix it there
          }
        }

        if (currentPlatform === "windows") {
          try {
            const microphoneStatus =
              await commands.getWindowsMicrophonePermissionStatus();
            if (
              microphoneStatus.supported &&
              microphoneStatus.overall_access === "denied"
            ) {
              await revealMainWindowForPermissions();
              setOnboardingStep("accessibility");
              return;
            }
          } catch (e) {
            console.warn("Failed to check Windows microphone permissions:", e);
            // If we can't check, proceed to main app and let them fix it there
          }
        }

        setOnboardingStep("done");
      } else {
        // New user - start full onboarding
        setIsReturningUser(false);
        setOnboardingStep("accessibility");
      }
    } catch (error) {
      console.error("Failed to check onboarding status:", error);
      setOnboardingStep("accessibility");
    }
  };

  const handleAccessibilityComplete = () => {
    // Returning users already have models, skip to main app
    // New users need to select a model
    setOnboardingStep(isReturningUser ? "done" : "model");
  };

  const handleModelSelected = () => {
    // Transition to main app - user has started a download
    setOnboardingStep("done");
  };

  // Rendered once around every step below (including onboarding) so
  // toast.error() calls surface to the user. sonner renders via a portal, so
  // its position in the tree doesn't affect layout. Without this, errors during
  // onboarding (e.g. a model download failing because blob.handy.computer is
  // unreachable) are silently swallowed and the wizard just appears to "blink".
  const toaster = (
    <Toaster
      theme="system"
      toastOptions={{
        unstyled: true,
        classNames: {
          // Fixed width for every toast: unstyled mode drops sonner's
          // default 356px, which left compact and card toasts with wildly
          // different widths.
          toast:
            "bg-background border border-mid-gray/20 rounded-lg shadow-lg px-4 py-3 flex items-center gap-3 text-sm w-[360px]",
          title: "font-medium",
          description: "text-mid-gray text-[13px] leading-relaxed",
          // Both buttons need whitespace-nowrap + shrink-0: without sonner's
          // built-in button CSS, a long action label squeezes the cancel
          // button into a one-character-wide column of stacked letters.
          actionButton:
            "px-2.5 py-1.5 text-xs font-medium rounded-lg border bg-mid-gray/10 border-mid-gray/20 hover:bg-background-ui/30 hover:border-logo-primary cursor-pointer whitespace-nowrap shrink-0",
          cancelButton:
            "px-2.5 py-1.5 text-xs font-medium rounded-lg border bg-transparent border-mid-gray/20 hover:bg-mid-gray/10 cursor-pointer whitespace-nowrap shrink-0 text-mid-gray",
          // The loading spinner's own CSS assumes a 16px icon frame; with
          // unstyled mode that frame is gone and the spinner renders as a
          // collapsed starburst floating outside the text line.
          icon: "shrink-0 h-4 w-4 flex items-center justify-center",
          loader: "text-mid-gray",
        },
      }}
    />
  );

  // Still checking onboarding status
  if (onboardingStep === null) {
    return null;
  }

  // Select the content for the current step. The Toaster is rendered once, in a
  // stable wrapper around this node, so crossing between onboarding steps and
  // the main app never remounts it (which would drop any in-flight toast).
  let content: ReactNode;
  if (onboardingPreview) {
    // Render previews in the same top-level slot as real onboarding. Keeping
    // the settings layout unmounted ensures viewport overflow behaves exactly
    // as it does during first-run onboarding.
    content = (
      <>
        {onboardingPreview === "accessibility" ? (
          <AccessibilityOnboarding onComplete={NOOP} preview />
        ) : (
          <Onboarding onModelSelected={NOOP} preview />
        )}
        <button
          type="button"
          onClick={() => setOnboardingPreview(null)}
          className="fixed top-4 end-4 z-50 rounded-lg border border-mid-gray/20 bg-background px-4 py-2 text-sm font-medium text-text shadow-lg hover:bg-background-ui/30 cursor-pointer"
        >
          {t("settings.debug.onboardingPreview.exitButton")}
        </button>
      </>
    );
  } else if (onboardingStep === "accessibility") {
    content = (
      <AccessibilityOnboarding onComplete={handleAccessibilityComplete} />
    );
  } else if (onboardingStep === "model") {
    content = <Onboarding onModelSelected={handleModelSelected} />;
  } else {
    content = (
      <div
        dir={direction}
        className="h-screen flex flex-col select-none cursor-default"
      >
        <ErrorBoundary context="What's New">
          <WhatsNewGate />
        </ErrorBoundary>
        {/* Main content area that takes remaining space */}
        <div className="flex-1 flex overflow-hidden">
          <Sidebar
            activeSection={currentSection}
            onSectionChange={setCurrentSection}
          />
          {/* Scrollable content area */}
          <div className="flex-1 flex flex-col overflow-hidden">
            <div ref={settingsScrollRef} className="flex-1 overflow-y-auto">
              <div className="flex flex-col items-center p-4 gap-4">
                <AccessibilityPermissions />
                <SecureInputWarning />
                {/* Settings-wide search sits above the tab content; picking a
                    match jumps to the section that owns the setting. */}
                <SettingsSearch onSelect={setCurrentSection} />
                {renderSettingsContent(currentSection, setOnboardingPreview)}
              </div>
            </div>
          </div>
        </div>
        {/* Fixed footer at bottom */}
        <Footer />
      </div>
    );
  }

  return (
    <>
      {toaster}
      {content}
    </>
  );
}

export default App;
