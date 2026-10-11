import assert from "node:assert/strict";
import { awaitUpdateChecksAllowed } from "./updateCheckGate";

// AUD-10 red test: a tray "Check for Updates" click that lands while the
// settingsStore's lock probe (loadUpdateChecksLocked, settingsStore.ts:749)
// is still in flight is silently dropped today. The Rust tray handler
// reveals the main window FIRST and then emits request-update-check
// (src-tauri/src/lib.rs:318-330), the App-level listener calls
// runUpdateCheck blindly (App.tsx:357), and runUpdateCheck bails at its
// very first guard (updaterFlow.ts:345) because updateChecksAllowed()
// (updaterFlow.ts:60-65) returns false while updateChecksLocked === null.
// Net effect: window flashes open, then nothing happens.
//
// The fix direction is an await-gate: before deciding, WAIT (bounded) for
// the lock probe to settle when the state is still null, then decide from
// the settled state. This test pins that gate as a pure helper whose
// contract is defined here; the module does not exist yet, so this file is
// red until it is implemented and wired into runUpdateCheck.
//
// CONTRACT for ./updateCheckGate:
//
//   export interface UpdateCheckGateSnapshot {
//     locked: boolean | null;   // useSettingsStore.getState().updateChecksLocked
//     checksEnabled: boolean;   // settings?.update_checks_enabled !== false
//   }
//
//   export function awaitUpdateChecksAllowed(
//     snapshot: () => UpdateCheckGateSnapshot,
//     lockProbe: Promise<unknown> | null,
//     timeoutMs: number,
//   ): Promise<boolean>
//
//   - snapshot() is re-read AFTER the probe settles; it is a function
//     precisely so a pending probe's settlement can change the answer.
//   - When snapshot().locked is already settled (true/false) the decision
//     comes from that snapshot IMMEDIATELY: the probe is not awaited and
//     the bounded wait is not burned.
//   - When snapshot().locked === null the returned promise stays PENDING
//     until the probe settles (or timeoutMs elapses), then re-reads the
//     snapshot and decides. Allowed === locked === false && checksEnabled.
//   - A probe that never settles (or lockProbe === null with the state
//     still null) must still decide within the bound: unknown fails CLOSED
//     (false), preserving the KB-033 semantics updaterFlow relies on.
//
// runUpdateCheck's other guards (inFlight, platform support) stay where
// they are; this gate replaces the bare updateChecksAllowed() read at
// updaterFlow.ts:345.

const delay = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

// A probe that never settles: proves the "settled decides immediately"
// cases cannot be satisfied by silently burning the bounded wait instead.
const NEVER = new Promise<never>(() => {});

// --- Settled lock decides immediately -------------------------------------

{
  const started = Date.now();
  const allowed = await awaitUpdateChecksAllowed(
    () => ({ locked: false, checksEnabled: true }),
    NEVER,
    2000,
  );
  assert.equal(
    allowed,
    true,
    "settled lock=false with checks enabled must be allowed",
  );
  assert.ok(
    Date.now() - started < 1000,
    "a settled lock must decide immediately, not by burning the 2000ms bounded wait on a stuck probe",
  );
}

{
  const started = Date.now();
  const allowed = await awaitUpdateChecksAllowed(
    () => ({ locked: true, checksEnabled: true }),
    NEVER,
    2000,
  );
  assert.equal(allowed, false, "settled lock=true must fail closed");
  assert.ok(
    Date.now() - started < 1000,
    "a settled lock must decide immediately, not by burning the bounded wait",
  );
}

{
  const allowed = await awaitUpdateChecksAllowed(
    () => ({ locked: false, checksEnabled: false }),
    null,
    2000,
  );
  assert.equal(
    allowed,
    false,
    "checks disabled by preference must be denied even when unlocked",
  );
}

// --- THE BUG: pending lock must hold the click, then proceed ---------------

{
  // Store state as the tray click sees it: probe in flight, lock unknown.
  let state = { locked: null as boolean | null, checksEnabled: true };
  let resolveProbe!: () => void;
  const probe = new Promise<void>((r) => {
    resolveProbe = r;
  });
  const decision = awaitUpdateChecksAllowed(() => ({ ...state }), probe, 2000);

  // While the probe is in flight the decision must stay pending. Today's
  // code (updaterFlow.ts:345) answers "not allowed" instantly here, which
  // is exactly the silently-dropped tray click.
  const early = await Promise.race([
    decision.then(() => "decided" as const),
    delay(50).then(() => "pending" as const),
  ]);
  assert.equal(
    early,
    "pending",
    "AUD-10: a pending lock probe must HOLD the manual check, not decide against it instantly",
  );

  // The probe resolves not-locked; the store flips; the held click proceeds.
  state = { locked: false, checksEnabled: true };
  resolveProbe();
  assert.equal(
    await decision,
    true,
    "after the probe resolves unlocked, the held manual check must proceed",
  );
}

{
  // Same window, but the probe settles LOCKED: the held click fails closed.
  let state = { locked: null as boolean | null, checksEnabled: true };
  let resolveProbe!: () => void;
  const probe = new Promise<void>((r) => {
    resolveProbe = r;
  });
  const decision = awaitUpdateChecksAllowed(() => ({ ...state }), probe, 2000);
  state = { locked: true, checksEnabled: true };
  resolveProbe();
  assert.equal(
    await decision,
    false,
    "a probe settling to locked must fail closed once it resolves",
  );
}

// --- The wait is bounded ----------------------------------------------------

{
  const started = Date.now();
  const allowed = await awaitUpdateChecksAllowed(
    () => ({ locked: null, checksEnabled: true }),
    NEVER,
    150,
  );
  assert.equal(
    allowed,
    false,
    "an unknown lock after the bounded wait must fail closed (KB-033), never hang",
  );
  assert.ok(
    Date.now() - started < 5000,
    "the bounded wait must actually be bounded",
  );
}

console.log("updateCheckGate tests passed");
