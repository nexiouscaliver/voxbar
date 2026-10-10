/**
 * Pure cache helpers for the overlay webview's first paint.
 *
 * The overlay used to gate its first paint on two sequential awaited IPC
 * round trips per press (language via getAppSettings, then placement via
 * getAppSettings again). These helpers mirror the theme/accent pattern
 * instead: the last-seen language and overlay placement live in the
 * localStorage shared with the settings window, are applied synchronously
 * at boot, and a background reconcile refreshes them after the overlay is
 * already visible.
 *
 * Everything here is deliberately free of DOM and Tauri imports (the
 * storage is injected) so the cache logic is testable without a DOM.
 */

export const OVERLAY_LANGUAGE_CACHE_KEY = "handy.overlay.language";
export const OVERLAY_PLACEMENT_CACHE_KEY = "handy.overlay.position";

export type OverlayPlacement = "top" | "bottom";

/** The slice of localStorage the cache helpers need. */
export interface OverlayCacheStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

/** Only "top" is a top overlay; anything else means "bottom". */
export const normalizePlacement = (value: unknown): OverlayPlacement =>
  value === "top" ? "top" : "bottom";

/** The webview's localStorage, or null when storage is unavailable. */
export const overlayCacheStorage = (): OverlayCacheStorage | null => {
  try {
    return globalThis.localStorage;
  } catch {
    return null;
  }
};

export const readCachedLanguage = (
  storage: OverlayCacheStorage | null,
): string | null => {
  try {
    const cached = storage?.getItem(OVERLAY_LANGUAGE_CACHE_KEY);
    return cached && cached.length > 0 ? cached : null;
  } catch {
    return null;
  }
};

export const writeCachedLanguage = (
  storage: OverlayCacheStorage | null,
  language: string,
): void => {
  try {
    storage?.setItem(OVERLAY_LANGUAGE_CACHE_KEY, language);
  } catch {
    // Cache misses only cost a one-frame default; the reconcile still runs.
  }
};

export const readCachedPlacement = (
  storage: OverlayCacheStorage | null,
): OverlayPlacement => {
  try {
    return normalizePlacement(storage?.getItem(OVERLAY_PLACEMENT_CACHE_KEY));
  } catch {
    return "bottom";
  }
};

export const writeCachedPlacement = (
  storage: OverlayCacheStorage | null,
  placement: OverlayPlacement,
): void => {
  try {
    storage?.setItem(OVERLAY_PLACEMENT_CACHE_KEY, placement);
  } catch {
    // Same trade-off as the language cache.
  }
};
