import { getVersion } from "@tauri-apps/api/app";

// KB-096: the build-time version vite injects (vite.config.ts defines
// __APP_VERSION__ from package.json). Declared, not imported, so plain
// non-vite contexts (bun tests, browser previews of the raw module) never
// hit a ReferenceError - the typeof check reads them the fallback literal.
declare const __APP_VERSION__: string | undefined;

/**
 * The app version shown in the footer and About, with the historical
 * fallback for contexts where the Tauri app handle is unavailable (tests,
 * plain-browser preview). One source so the two surfaces can never drift
 * apart again.
 *
 * KB-096: the fallback resolves the injected build-time version first;
 * the synced literal below only covers non-vite execution and is pinned
 * to package.json's version by appVersion.test.ts, so a version bump
 * without the literal update fails the test instead of shipping "0.1.2".
 */
export const FALLBACK_APP_VERSION =
  typeof __APP_VERSION__ === "string" && __APP_VERSION__.length > 0
    ? __APP_VERSION__
    : "1.6.6";

export const fetchAppVersion = async (): Promise<string> => {
  try {
    return await getVersion();
  } catch (error) {
    console.error("Failed to get app version:", error);
    return FALLBACK_APP_VERSION;
  }
};
