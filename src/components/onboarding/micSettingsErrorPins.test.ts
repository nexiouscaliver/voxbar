import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";

/* KB-268 pin (fleet pass 1): the onboarding microphone handlers only
 * caught TRANSPORT errors, but the generated Tauri bindings resolve a Rust
 * Err as {status:"error"} instead of throwing (openMicrophonePrivacySettings
 * has real Err paths on every platform), so a failed settings-pane open
 * was swallowed - and handleGrantMicrophone then armed the 60s waiting
 * spinner that no settings flip could ever satisfy, while
 * handleOpenMicSettings' button silently did nothing. The same defect
 * class the KB-268 lane already fixed for the AppData/LogDirectory
 * buttons; this pins the onboarding siblings the lane missed.
 *
 * No React render harness exists in this repo, so this pin EXECUTES the
 * committed handler bodies against stubbed bindings - the same technique
 * as directoryOpenErrorPins.test.ts: the handler source is sliced out of
 * the component verbatim, wrapped in an async function, and driven with
 * (1) an error RESULT, (2) an ok result, and (3) a transport REJECTION. */

const here = import.meta.dirname;

interface Toast {
  errors: string[];
  error(message: string): void;
}

function makeToast(): Toast {
  const errors: string[] = [];
  return { errors, error: (message: string) => void errors.push(message) };
}

const t = (key: string): string => key;
const quietConsole = { error: () => {} };

type OpenResult = { status: "ok" | "error"; error?: string };

// Everything handleGrantMicrophone reads from the component scope, with
// call counters for the spinner-arming seams the defect left dangling.
function makeGrantScope(openResult: Promise<OpenResult>) {
  const calls = {
    waiting: 0,
    micDeadline: 0,
    polling: 0,
    requestedMicPerm: 0,
  };
  return {
    calls,
    commands: { openMicrophonePrivacySettings: () => openResult },
    isWindows: true,
    requestMicrophonePermission: () => {
      calls.requestedMicPerm++;
      return Promise.resolve();
    },
    setPermissions: (
      update: (prev: { accessibility: string; microphone: string }) => {
        accessibility: string;
        microphone: string;
      },
    ) => {
      const next = update({ accessibility: "needed", microphone: "needed" });
      if (next.microphone === "waiting") calls.waiting++;
    },
    startMicDeadline: () => {
      calls.micDeadline++;
    },
    startPolling: () => {
      calls.polling++;
    },
  };
}

type GrantScope = ReturnType<typeof makeGrantScope>;

function sliceHandler(source: string, name: string): string {
  const signature = `const ${name} = async () => {`;
  const start = source.indexOf(signature);
  assert.ok(start >= 0, `${name} must exist`);
  const end = source.indexOf("\n  };", start);
  assert.ok(end > start, `${name} must close cleanly`);
  const rawInner = source
    .slice(start + signature.length, end)
    .trim()
    .replace(/\s+as\s+[A-Za-z]+/g, "");
  assert.ok(
    !rawInner.includes(" as "),
    `${name}: cast stripping must be total`,
  );
  return rawInner;
}

function readComponent(): string {
  return fs.readFileSync(
    path.join(here, "AccessibilityOnboarding.tsx"),
    "utf8",
  );
}

async function runHandleGrant(scope: GrantScope, toast: Toast): Promise<void> {
  const rawInner = sliceHandler(readComponent(), "handleGrantMicrophone");
  const handler = new Function(
    "preview",
    "isWindows",
    "commands",
    "requestMicrophonePermission",
    "setPermissions",
    "startMicDeadline",
    "startPolling",
    "toast",
    "t",
    "console",
    `"use strict"; return (async () => {\n${rawInner}\n})();`,
  ) as (
    preview: boolean,
    isWindows: boolean,
    commands: GrantScope["commands"],
    requestMicrophonePermission: GrantScope["requestMicrophonePermission"],
    setPermissions: GrantScope["setPermissions"],
    startMicDeadline: GrantScope["startMicDeadline"],
    startPolling: GrantScope["startPolling"],
    toast: Toast,
    t: (key: string) => string,
    consoleLike: { error: () => void },
  ) => Promise<void>;
  await handler(
    false,
    scope.isWindows,
    scope.commands,
    scope.requestMicrophonePermission,
    scope.setPermissions,
    scope.startMicDeadline,
    scope.startPolling,
    toast,
    t,
    quietConsole,
  );
}

async function runHandleOpenMicSettings(
  commands: GrantScope["commands"],
  toast: Toast,
): Promise<void> {
  const rawInner = sliceHandler(readComponent(), "handleOpenMicSettings");
  const handler = new Function(
    "preview",
    "commands",
    "toast",
    "t",
    "console",
    `"use strict"; return (async () => {\n${rawInner}\n})();`,
  ) as (
    preview: boolean,
    commands: GrantScope["commands"],
    toast: Toast,
    t: (key: string) => string,
    consoleLike: { error: () => void },
  ) => Promise<void>;
  await handler(false, commands, toast, t, quietConsole);
}

// 1. A {status:"error"} RESULT (what the bindings deliver for a Rust Err):
//    must toast and must NOT arm the waiting spinner - the unsatisfiable
//    60s deadline was the user-visible half of the defect.
{
  const toast = makeToast();
  const scope = makeGrantScope(
    Promise.resolve({ status: "error", error: "spawn failed" }),
  );
  await runHandleGrant(scope, toast);
  assert.equal(
    toast.errors.length,
    1,
    `KB-268: handleGrantMicrophone must toast exactly once for a {status:"error"} result`,
  );
  assert.ok(
    toast.errors[0].includes("onboarding.permissions.errors.requestFailed"),
    "KB-268: handleGrantMicrophone error toast must use the requestFailed key",
  );
  assert.equal(
    scope.calls.waiting,
    0,
    "KB-268: a failed settings open must not enter the waiting state",
  );
  assert.equal(
    scope.calls.micDeadline,
    0,
    "KB-268: a failed settings open must not arm the 60s mic deadline",
  );
}

// 2. An ok result: the spinner arms and no toast fires.
{
  const toast = makeToast();
  const scope = makeGrantScope(Promise.resolve({ status: "ok" }));
  await runHandleGrant(scope, toast);
  assert.equal(toast.errors.length, 0, "ok result stays silent");
  assert.equal(scope.calls.waiting, 1, "ok result arms the waiting state");
  assert.equal(scope.calls.micDeadline, 1, "ok result arms the deadline");
  assert.equal(scope.calls.polling, 1, "ok result starts polling");
}

// 3. A transport REJECTION: the original catch path still toasts, and the
//    spinner still does not arm.
{
  const toast = makeToast();
  const scope = makeGrantScope(
    Promise.reject(new Error("channel closed before command ran")),
  );
  await runHandleGrant(scope, toast);
  assert.equal(
    toast.errors.length,
    1,
    "KB-268: handleGrantMicrophone transport catch must keep toasting",
  );
  assert.equal(
    scope.calls.waiting,
    0,
    "a rejected settings open must not enter the waiting state either",
  );
}

// handleOpenMicSettings: the same three resolutions.
{
  const toast = makeToast();
  await runHandleOpenMicSettings(
    {
      openMicrophonePrivacySettings: () =>
        Promise.resolve({ status: "error", error: "opener failed" }),
    },
    toast,
  );
  assert.equal(
    toast.errors.length,
    1,
    `KB-268: handleOpenMicSettings must toast for a {status:"error"} result`,
  );
}
{
  const toast = makeToast();
  await runHandleOpenMicSettings(
    { openMicrophonePrivacySettings: () => Promise.resolve({ status: "ok" }) },
    toast,
  );
  assert.equal(toast.errors.length, 0, "ok result stays silent");
}
{
  const toast = makeToast();
  await runHandleOpenMicSettings(
    {
      openMicrophonePrivacySettings: () =>
        Promise.reject(new Error("channel closed before command ran")),
    },
    toast,
  );
  assert.equal(
    toast.errors.length,
    1,
    "KB-268: handleOpenMicSettings transport catch must keep toasting",
  );
}

console.log(
  "micSettingsErrorPins (KB-268): onboarding mic-settings error results toast, spinner never arms on failure",
);
