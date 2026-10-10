// Decision table for the one-shot startup auto-check (KB-191), kept pure so
// it can be unit-tested with a plain bun script
// (src/components/update-checker/updaterAutoCheck.test.ts) the way
// updaterPlatform.ts is, and free of imports so the component stays the
// only place that knows about refs and effects.
//
// The latch exists so the automatic check runs at most once per session.
// Before KB-191 it only ever engaged when the guard passed, so re-enabling
// update checks mid-session fired an immediate silent network check as a
// side effect of the toggle flip itself. The latch is therefore also
// consumed whenever the *loaded* settings say checks are off: that state is
// a user-visible policy ("checks are off"), and leaving it armed would turn
// re-enabling into an implicit phoning home. Merely not being loaded yet
// (settings or the system lock still pending) keeps the latch armed, so a
// fresh launch still auto-checks once when checks are enabled at launch.
// Manual checks bypass this latch entirely.

export interface AutoCheckInputs {
  settingsLoaded: boolean;
  updateChecksEnabled: boolean;
  autoUpdateSupported: boolean;
  hasAutoChecked: boolean;
}

export interface AutoCheckDecision {
  shouldRunCheck: boolean;
  hasAutoChecked: boolean;
}

/**
 * Whether this guard state should trigger the silent startup check, and
 * what the latch should read afterwards. The caller persists
 * `hasAutoChecked` (a ref) and only runs the check when `shouldRunCheck`
 * is true.
 */
export function resolveAutoCheck(inputs: AutoCheckInputs): AutoCheckDecision {
  const guardPasses =
    inputs.settingsLoaded &&
    inputs.updateChecksEnabled &&
    inputs.autoUpdateSupported;
  // Loaded-and-disabled spends the session's automatic check; merely
  // unloaded does not (an unknown state is not a policy).
  const consumedByDisabledSetting =
    inputs.settingsLoaded && !inputs.updateChecksEnabled;
  const shouldRunCheck = guardPasses && !inputs.hasAutoChecked;
  return {
    shouldRunCheck,
    hasAutoChecked:
      inputs.hasAutoChecked || consumedByDisabledSetting || shouldRunCheck,
  };
}
