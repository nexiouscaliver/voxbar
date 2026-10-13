import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";

/* KB-029 pin: reset-pairing failures were invisible on both halves of
 * the seam. The Rust command returned its Err without ever writing the
 * manager's last_error (so the panel's status line rendered a clean
 * "Stopped" even though the restart leg had failed and the fresh pairing
 * token had already persisted), and this handler only caught TRANSPORT
 * errors - the generated Tauri bindings resolve a Rust Err as
 * {status:"error"} instead of throwing, so the real failure shape never
 * reached any toast. The command now routes through the same
 * set_last_error + CompanionServerFailed path restart_companion_server
 * uses (pinned on the Rust side), and this handler checks
 * result.status and toasts, exactly like the restart button and the
 * KB-268 directory-open pins.
 *
 * No React render harness exists in this repo, so this pin EXECUTES the
 * committed handler body against stubbed bindings - the same technique
 * as directoryOpenErrorPins.test.ts: the handler source is sliced out of
 * the component verbatim, wrapped in an async function, and driven with
 * (1) an error RESULT, (2) an ok result, and (3) a transport REJECTION,
 * so all three resolutions stay pinned. */

const here = import.meta.dirname;

interface Toast {
  errors: string[];
  error(message: string): void;
}

function makeToast(): Toast {
  const errors: string[] = [];
  return { errors, error: (message: string) => void errors.push(message) };
}

// The project's i18n.t seam: render the key plus the interpolated error
// so a missing interpolation is visible in the captured toast text.
const i18n = {
  t: (key: string, opts?: { error?: string }): string =>
    `${key}:${opts?.error ?? ""}`,
};

const quietConsole = { error: () => {} };

type Commands = Record<string, () => Promise<unknown>>;

async function runResetPairing(
  commands: Commands,
  toast: Toast,
): Promise<void> {
  const source = fs.readFileSync(
    path.join(here, "CompanionDevices.tsx"),
    "utf8",
  );
  const start = source.indexOf(
    "const resetPairing = useCallback(async () => {",
  );
  assert.ok(
    start >= 0,
    "CompanionDevices: the resetPairing handler must exist",
  );
  const end = source.indexOf("\n  }, [", start);
  assert.ok(end > start, "CompanionDevices: resetPairing must close cleanly");
  // The inner statements, verbatim from the committed source, wrapped in
  // an async arrow whose parameters carry the stubs. (No cast stripping
  // needed, unlike the KB-268 pins: this handler carries no TS casts, and
  // a prose " as " inside its comments is not code.)
  const rawInner = source
    .slice(start + "const resetPairing = useCallback(async () => {".length, end)
    .trim();
  assert.ok(
    !/\)\s+as\s+[A-Za-z]/.test(rawInner),
    `a TS cast would need stripping before execution (left: ${rawInner.match(/.{0,40}\)\s+as\s+[A-Za-z].{0,40}/)?.[0] ?? "none"})`,
  );
  const handler = new Function(
    "commands",
    "toast",
    "i18n",
    "refreshStatus",
    "setResetting",
    "console",
    `"use strict"; return (async () => {\n${rawInner}\n})();`,
  ) as (
    commands: Commands,
    toast: Toast,
    i18n: { t: (key: string, opts?: { error?: string }) => string },
    refreshStatus: () => Promise<void>,
    setResetting: (value: boolean) => void,
    consoleLike: { error: () => void },
  ) => Promise<void>;
  await handler(
    commands,
    toast,
    i18n,
    async () => {},
    () => {},
    quietConsole,
  );
}

// 1. A {status:"error"} RESULT (what the bindings actually deliver for a
//    Rust Err - the restart leg's failure): must toast exactly once with
//    the shared key and the command's error string.
{
  const toast = makeToast();
  await runResetPairing(
    {
      resetCompanionPairing: () =>
        Promise.resolve({ status: "error", error: "port already in use" }),
    },
    toast,
  );
  assert.equal(
    toast.errors.length,
    1,
    'KB-029: a {status:"error"} reset result must toast exactly once',
  );
  assert.ok(
    toast.errors[0].includes("toast.settingNotSaved"),
    "KB-029: the reset error toast must use the shared settingNotSaved key",
  );
  assert.ok(
    toast.errors[0].includes("port already in use"),
    "KB-029: the reset error toast must carry the command's error string",
  );
}

// 2. An ok result: silent success, no toast.
{
  const toast = makeToast();
  await runResetPairing(
    {
      resetCompanionPairing: () =>
        Promise.resolve({ status: "ok", data: null }),
    },
    toast,
  );
  assert.equal(
    toast.errors.length,
    0,
    "KB-029: a successful reset stays silent",
  );
}

// 3. A transport REJECTION (the bindings rethrow real Error objects):
//    must not throw out of the handler and must not toast twice.
{
  const toast = makeToast();
  await runResetPairing(
    {
      resetCompanionPairing: () => Promise.reject(new Error("transport died")),
    },
    toast,
  );
  assert.equal(
    toast.errors.length,
    0,
    "KB-029: a transport rejection stays in the catch (console-only), no toast",
  );
}

console.log("companionResetPairingPins (KB-029) tests passed");
