import React from "react";
import ReactDOM from "react-dom/client";
import { listen } from "@tauri-apps/api/event";
import RecordingOverlay from "./RecordingOverlay";
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

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <RecordingOverlay />
  </React.StrictMode>,
);
