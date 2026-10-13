import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";

/* KB-268 pin: the App Data Directory and Log Directory open buttons only
 * caught TRANSPORT errors, but the generated Tauri bindings resolve a Rust
 * Err as {status:"error"} instead of throwing, so every real open failure
 * (the ACL-death class this toast exists to surface) was swallowed. Both
 * handlers now check result.status like HistorySettings does and toast on
 * the error resolution.
 *
 * No React render harness exists in this repo, so this pin EXECUTES the
 * committed handler bodies against stubbed bindings - the same technique
 * as the KB-260 fallback pin in companionClientPage.test.ts: the handler
 * source is sliced out of the component verbatim, wrapped in an async
 * function, and driven with (1) an error RESULT, (2) an ok result, and
 * (3) a transport REJECTION, so all three resolutions stay pinned. */

const here = import.meta.dirname;

interface Toast {
  errors: string[];
  error(message: string): void;
}

function makeToast(): Toast {
  const errors: string[] = [];
  return { errors, error: (message: string) => void errors.push(message) };
}

// The project's t() seam: render the key plus the interpolated error so a
// missing interpolation is visible in the captured toast text.
const t = (key: string, opts?: { error?: string }): string =>
  `${key}:${opts?.error ?? ""}`;

const quietConsole = { error: () => {} };

type Commands = Record<string, () => Promise<unknown>>;

async function runHandleOpen(
  componentName: string,
  sourcePath: string,
  dirVar: "appDirPath" | "logDir",
  commands: Commands,
  toast: Toast,
): Promise<void> {
  const source = fs.readFileSync(path.join(here, sourcePath), "utf8");
  const start = source.indexOf("const handleOpen = async () => {");
  assert.ok(start >= 0, `${componentName}: handleOpen must exist`);
  const end = source.indexOf("\n  };", start);
  assert.ok(end > start, `${componentName}: handleOpen must close cleanly`);
  // The inner statements, verbatim from the committed source apart from
  // TS-only `as` casts (which a bundler erases before the browser ever
  // runs this code), wrapped in an async arrow whose parameters carry the
  // stubs.
  const rawInner = source
    .slice(start + "const handleOpen = async () => {".length, end)
    .trim()
    .replace(/\s+as\s+[A-Za-z]+/g, "");
  assert.ok(
    !rawInner.includes(" as "),
    `${componentName}: cast stripping must be total (unknown cast left: ${rawInner.match(/.{0,40}as.{0,40}/)?.[0]})`,
  );
  const handler = new Function(
    "commands",
    "toast",
    "t",
    dirVar,
    "console",
    `"use strict"; return (async () => {\n${rawInner}\n})();`,
  ) as (
    commands: Commands,
    toast: Toast,
    t: (key: string, opts?: { error?: string }) => string,
    dir: string,
    consoleLike: { error: () => void },
  ) => Promise<void>;
  await handler(commands, toast, t, "/tmp/voxbar-test-dir", quietConsole);
}

for (const [componentName, sourcePath, dirVar, commandName] of [
  ["AppDataDirectory", "AppDataDirectory.tsx", "appDirPath", "openAppDataDir"],
  [
    "LogDirectory",
    path.join("debug", "LogDirectory.tsx"),
    "logDir",
    "openLogDir",
  ],
] as const) {
  // 1. A {status:"error"} RESULT (what the bindings actually deliver for a
  //    Rust Err): must toast, must not throw.
  {
    const toast = makeToast();
    await runHandleOpen(
      componentName,
      sourcePath,
      dirVar,
      {
        [commandName]: () =>
          Promise.resolve({ status: "error", error: "open denied" }),
      },
      toast,
    );
    assert.equal(
      toast.errors.length,
      1,
      `KB-268: ${componentName} must toast exactly once for a {status:"error"} result`,
    );
    assert.ok(
      toast.errors[0].includes("errors.openDirectory"),
      `KB-268: ${componentName} error toast must use the errors.openDirectory key`,
    );
    assert.ok(
      toast.errors[0].includes("open denied"),
      `KB-268: ${componentName} error toast must carry the resolution's error string`,
    );
  }

  // 2. An ok result: silent success, no toast.
  {
    const toast = makeToast();
    await runHandleOpen(
      componentName,
      sourcePath,
      dirVar,
      { [commandName]: () => Promise.resolve({ status: "ok", data: null }) },
      toast,
    );
    assert.equal(
      toast.errors.length,
      0,
      `KB-268: ${componentName} must stay silent on an ok result`,
    );
  }

  // 3. A transport REJECTION: the original catch path still toasts.
  {
    const toast = makeToast();
    await runHandleOpen(
      componentName,
      sourcePath,
      dirVar,
      {
        [commandName]: () =>
          Promise.reject(new Error("channel closed before command ran")),
      },
      toast,
    );
    assert.equal(
      toast.errors.length,
      1,
      `KB-268: ${componentName} transport catch must keep toasting`,
    );
    assert.ok(
      toast.errors[0].includes("channel closed"),
      `KB-268: ${componentName} transport toast must carry the thrown message`,
    );
  }
}

console.log("directoryOpenErrorPins (KB-268): error results now toast");
