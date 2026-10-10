import React, { useEffect } from "react";
import ReactDOM from "react-dom/client";
import { listen } from "@tauri-apps/api/event";
import RecordingOverlay from "./RecordingOverlay";
import { ErrorBoundary } from "../components/ErrorBoundary";
import { commands } from "@/bindings";
import i18n from "@/i18n";
import {
  applyTheme,
  getStoredTheme,
  syncThemeFromSettings,
} from "@/lib/utils/theme";
import {
  applyAccent,
  getStoredAccent,
  syncAccentFromSettings,
} from "@/lib/utils/accent";
import {
  overlayCacheStorage,
  readCachedLanguage,
  writeCachedLanguage,
} from "./overlayBootstrap";
import type { Theme } from "@/bindings";
import "@/i18n";

// A separate webview from the settings window, so the overlay has to set
// `data-theme` on its own document: last-known theme before render (shared
// localStorage) to avoid a flash, reconcile with the persisted setting in case
// the overlay booted first, then follow live changes.
applyTheme(getStoredTheme());
syncThemeFromSettings();
listen<Theme>("theme-changed", (event) => applyTheme(event.payload));

// Accent boot/live-follow, mirroring the theme handling above: the swatch
// palette override lands on this document too (shared localStorage avoids a
// first-frame flash, the settings sync covers boot-order races, the event
// follows live changes from the settings window).
applyAccent(getStoredAccent());
syncAccentFromSettings();
listen<string>("accent-changed", (event) => applyAccent(event.payload));

// Language boot, same pattern: apply the last-seen language synchronously
// so the first paint is already localized. RecordingOverlay then reconciles
// with AppSettings on mount, refreshes the cache, and keeps following
// "settings-changed" so a language switch in the settings window lands here
// without waiting for the next hotkey press.
const cachedLanguage = readCachedLanguage(overlayCacheStorage());
if (cachedLanguage) {
  void i18n.changeLanguage(cachedLanguage);
}

// A render throw inside the overlay must not leave a phantom transparent
// pill on screen: recover by cancelling the current operation, whose hide
// path fades the overlay window out from the backend side.
const OverlayCrashFallback: React.FC = () => {
  useEffect(() => {
    commands.cancelOperation().catch(() => {
      // The window may already be gone; nothing else to do.
    });
  }, []);
  return null;
};

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <ErrorBoundary context="recording overlay" fallback={OverlayCrashFallback}>
      <RecordingOverlay />
    </ErrorBoundary>
  </React.StrictMode>,
);
