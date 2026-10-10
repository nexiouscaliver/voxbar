import { getVersion } from "@tauri-apps/api/app";

/**
 * The app version shown in the footer and About, with the historical
 * fallback for contexts where the Tauri app handle is unavailable (tests,
 * plain-browser preview). One source so the two surfaces can never drift
 * apart again.
 */
export const FALLBACK_APP_VERSION = "0.1.2";

export const fetchAppVersion = async (): Promise<string> => {
  try {
    return await getVersion();
  } catch (error) {
    console.error("Failed to get app version:", error);
    return FALLBACK_APP_VERSION;
  }
};
