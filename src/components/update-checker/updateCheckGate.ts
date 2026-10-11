// AUD-10: the tray "Check for Updates" click vs. the settings store's lock
// probe. The Rust tray handler reveals the main window FIRST and then emits
// request-update-check (src-tauri/src/lib.rs), so a click landing while
// loadUpdateChecksLocked() (settingsStore.ts) is still in flight used to
// flash the window open and then do nothing: runUpdateCheck's old
// synchronous guard read updateChecksLocked while it was still null and
// answered "not allowed" instantly - the silently-dropped click.
//
// This module is the await-gate that fixes it, pinned as a pure helper by
// updateCheckGate.test.ts. Deliberately free of imports (same convention as
// updaterPlatform.ts) so the contract is unit-testable with a plain bun
// script and reusable by every update entrypoint (tray, footer, About).

/**
 * The inputs the gate decides from, read fresh from the settings store.
 * `locked` is useSettingsStore.getState().updateChecksLocked (null while the
 * probe is in flight); `checksEnabled` is the user's stored preference
 * (settings?.update_checks_enabled !== false).
 */
export interface UpdateCheckGateSnapshot {
  locked: boolean | null;
  checksEnabled: boolean;
}

/**
 * Decide whether an update check may run, waiting (bounded) for the lock
 * probe to settle when its state is still unknown.
 *
 * - A settled `locked` (true/false) decides IMMEDIATELY from that snapshot:
 *   the probe is not awaited and the bounded wait is not burned.
 * - `locked === null` holds the decision PENDING until the probe settles
 *   (or timeoutMs elapses), then re-reads the snapshot and decides from
 *   what the store now says. Allowed === locked === false && checksEnabled.
 * - Still unknown after the bound fails CLOSED (false, KB-033): the call
 *   can never hang, and a locked install cannot slip a check through the
 *   loading window.
 *
 * snapshot() is a function precisely so a pending probe's settlement can
 * change the answer after the wait.
 */
export function awaitUpdateChecksAllowed(
  snapshot: () => UpdateCheckGateSnapshot,
  lockProbe: Promise<unknown> | null,
  timeoutMs: number,
): Promise<boolean> {
  return new Promise<boolean>((resolve) => {
    // Decide from whatever the store says right now. Called once on a
    // settled snapshot (synchronously) and again after the probe settles
    // or the bound burns, so a null lock that never resolves lands here as
    // `null === false` -> false (fail closed).
    const decide = () => {
      const state = snapshot();
      resolve(state.locked === false && state.checksEnabled);
    };

    // Settled lock state decides from the current snapshot, synchronously:
    // no probe await, no timer armed, so a stuck probe can never tax the
    // common path.
    if (snapshot().locked !== null) {
      decide();
      return;
    }

    // Unknown lock: hold the decision until the probe settles or the bound
    // burns, then re-read the store and decide. Whichever fires first wins.
    let settled = false;
    const timer: ReturnType<typeof setTimeout> = setTimeout(() => {
      if (settled) return;
      settled = true;
      decide();
    }, timeoutMs);

    const settle = () => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      decide();
    };

    if (lockProbe !== null) {
      // A rejected probe settles the race here too (the store's own catch
      // has already flipped its state to "not locked"); the snapshot
      // re-read decides. Promise resolution is not timer-dependent, so
      // this path ends the hold even where macOS suspends setTimeout in a
      // hidden webview - only the fail-safe bound rides the timer.
      Promise.resolve(lockProbe).then(settle, settle);
    }
  });
}
