// Where the in-app updater actually works today, as a pure predicate over
// the platform string tauri-plugin-os reports. Deliberately free of imports
// so the decision table can be unit-tested with a plain bun script
// (src/components/update-checker/updaterPlatform.test.ts) and reused by every
// update entrypoint (footer, tray backend, About).
//
// Today only macOS ships updater artifacts: the release pipeline
// (scripts/release-macos.sh) signs and publishes darwin entries in
// latest.json, and bundle targets for Windows/Linux exist but no signed
// update payloads are produced for them. A check() on those platforms can
// only error or find nothing, and the releases page has no matching assets
// to hand off to, so the honest UI hides the check behind this predicate
// and points at the releases page instead. Flip a platform to true only
// when its artifacts actually ship (see BUILD.md "Releasing").

/**
 * Platform identifiers as reported by @tauri-apps/plugin-os `platform()`.
 */
export type UpdaterPlatform =
  | "macos"
  | "windows"
  | "linux"
  | "android"
  | "ios"
  | (string & {});

/**
 * Whether the in-app update flow (check, download, install) is supported on
 * this platform. Unknown platform strings are treated as unsupported: a
 * wrong guess would show a check that can only fail.
 */
export function updaterAutoUpdateSupported(platform: string): boolean {
  return platform === "macos";
}
