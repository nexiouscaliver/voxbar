// Whether a failed update check must also reach the user as an OS
// notification, as a pure decision over the check's trigger and the main
// window's visibility. Deliberately free of imports so the decision table
// can be unit-tested with a plain bun script
// (src/components/update-checker/checkFailureNotify.test.ts) and wired into
// runUpdateCheck's catch (updaterFlow.ts).
//
// The startup auto check runs while the app typically dwells hidden in the
// tray (start_hidden): its toasts would render in the hidden window's
// webview, and the catch arm has no toast for trigger === "auto" at all, so
// a failed boot check used to vanish completely — the KB-148 hidden-window
// notifyDesktop gates cover download/install failure, the restart prompt and
// the ask card, but not the check-failure path. When an auto check fails AND
// the window is hidden, the failure title must ride an OS notification
// instead (same gate and title key as those call sites).
//
// Every other combination keeps its existing surface: an auto failure in a
// VISIBLE window stays quiet (calm boot — a check the user never asked for
// must not toast), and manual checks never desktop-notify because that arm
// reveals the window before checking and already toasts the failure title,
// so an OS notification would double-report one failure.

/**
 * True only for an auto-triggered check that failed while the main window is
 * hidden: the toast the user would have seen renders nowhere, so the failure
 * must reach them through notifyDesktop. `windowHidden` is a plain boolean so
 * the call site can derive it from whatever visibility source is current
 * (the Rust-fed windowVisibility store AUD-03 added, the same source the
 * other KB-148 gates in updaterFlow read).
 */
export function shouldDesktopNotifyCheckFailure(
  trigger: "manual" | "auto",
  windowHidden: boolean,
): boolean {
  return trigger === "auto" && windowHidden;
}
