import assert from "node:assert/strict";
import { shouldDesktopNotifyCheckFailure } from "./checkFailureNotify";

// AUD-09 — an auto-triggered (boot) update-check failure is completely
// silent today. In runUpdateCheck's catch (updaterFlow.ts:458-471) only the
// trigger === "manual" arm toasts; when the startup auto check fails (all
// CHECK_ATTEMPTS = 3 attempts), the user gets nothing at all — not even
// when the main window is hidden in the tray, which is its normal state at
// boot. The KB-148 hidden-window notifyDesktop gates already cover
// download/install failures, the restart prompt and the ask card
// (updaterFlow.ts:157, 334, 431) but NOT the check-failure catch, so the
// KB-148 claim does not cover this path.
//
// CONTRACT for the extracted pure decision (the module does not exist yet —
// that import failing IS the red test):
//
//   shouldDesktopNotifyCheckFailure(
//     trigger: "manual" | "auto",
//     windowHidden: boolean,
//   ): boolean
//
// Wired at the call site (runUpdateCheck's catch, after
// logDecision("check_failed", ...)) roughly as:
//
//   } else if (
//     shouldDesktopNotifyCheckFailure(
//       trigger,
//       document.visibilityState !== "visible",
//     )
//   ) {
//     void notifyDesktop(t("footer.updater.checkFailedTitle"));
//   }
//
// — reusing the SAME failure title key the manual toast uses
// (footer.updater.checkFailedTitle, updaterFlow.ts:462) and the SAME hidden
// gate event (document.visibilityState) that AUD-03's shared module will
// formalize; until that module lands, gate on the event directly, exactly
// as updaterFlow.ts already does in showFailureToast / showRestartPrompt /
// the ask card. No toast is added for auto failures in either visibility
// state: a visible window keeps the calm boot.

// The one case AUD-09 is about: the auto (boot) check has no toast arm,
// so its failure must reach the user as an OS notification. Window
// visibility is no longer an input here on purpose: notifyDesktop's live
// getCurrentWindow query (KB-195) makes the final deliver/quiet call, which
// stays correct for minimize and Cmd+H where any store-fed gate goes
// stale - a visible window is simply told "no" inside notifyDesktop.
assert.equal(
  shouldDesktopNotifyCheckFailure("auto"),
  true,
  "AUD-09: auto check failure offers the OS notification; notifyDesktop's live query decides delivery",
);

// Manual failures keep their existing surface: the manual arm reveals the
// window before checking and toasts checkFailedTitle on failure, so it
// must not start double-notifying.
assert.equal(
  shouldDesktopNotifyCheckFailure("manual"),
  false,
  "manual failures already reveal the window and toast; no desktop notification",
);

console.log("checkFailureNotify tests passed");
