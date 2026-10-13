import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";

/* KB-155 pin: model delete and download-cancel failures were invisible in
 * the settings UI. The modelStore never throws the backend error - it
 * records it in state ("Failed to delete model: ...") and returns false -
 * and the old handlers console.error'd (delete, non-refusal branch) or
 * ignored the result entirely (cancel), so a swap-guard refusal or an fs
 * error left the user with zero feedback right after confirming a native
 * dialog. Both handlers now route EVERY failure through the row alert
 * (setSelectionError) with localized copy (deleteFailed / cancelFailed),
 * keeping the pre-existing live-session refusal branch (KB-117/KB-220).
 *
 * No React render harness exists in this repo, so this pin EXECUTES the
 * committed handler bodies against stubbed store actions - the same
 * technique as micSettingsErrorPins.test.ts: the handler source is sliced
 * out of the component verbatim, wrapped in an async function, and driven
 * with (1) a stubbed FALSE result + recorded store error (what the real
 * store delivers for a backend Err), (2) the live-session refusal string,
 * (3) a transport REJECTION, and (4) a successful result. */

const here = import.meta.dirname;

const t = (key: string, opts?: Record<string, unknown>): string =>
  opts && "error" in opts ? `${key}|${String(opts.error)}` : key;
const quietConsole = { error: () => {} };

interface AlertState {
  modelId: string;
  message: string;
}

interface StubModel {
  id: string;
  name: string;
}

interface HandlerScope {
  alerts: AlertState[];
  models: StubModel[];
  currentModel: string;
  ask: () => Promise<boolean>;
  deleteModel: (modelId: string) => Promise<boolean>;
  cancelDownload: (modelId: string) => Promise<boolean>;
  isSessionRefusalError: (raw: string) => boolean;
  useModelStore: { getState: () => { error: string | null } };
  setSelectionError: (alert: AlertState) => void;
}

function makeScope(
  deleteResult: Promise<boolean>,
  storeError: string | null,
): HandlerScope {
  const alerts: AlertState[] = [];
  return {
    alerts,
    models: [{ id: "m1", name: "Test Model" }],
    currentModel: "other",
    ask: async () => true,
    deleteModel: () => deleteResult,
    cancelDownload: () => deleteResult,
    isSessionRefusalError: (raw: string) =>
      raw.startsWith("dictation-in-progress"),
    useModelStore: { getState: () => ({ error: storeError }) },
    setSelectionError: (alert: AlertState) => void alerts.push(alert),
  };
}

function sliceHandler(source: string, name: string): string {
  const signature = `const ${name} = async (modelId: string) => {`;
  const start = source.indexOf(signature);
  assert.ok(start >= 0, `${name} must exist`);
  const end = source.indexOf("\n  };", start);
  assert.ok(end > start, `${name} must close cleanly`);
  const rawInner = source
    .slice(start + signature.length, end)
    // Strip the TS-only annotations the real body carries so `new
    // Function` (plain JS) can parse it. The logic stays verbatim.
    .replace(/\(m: ModelInfo\)/g, "(m)");
  assert.ok(
    !rawInner.includes("ModelInfo"),
    `${name}: annotation stripping must be total`,
  );
  return rawInner;
}

function readComponent(): string {
  return fs.readFileSync(
    path.join(here, "ModelsSettings.tsx"),
    "utf8",
  );
}

async function runHandler(
  name: "handleModelDelete" | "handleModelCancel",
  scope: HandlerScope,
  modelId: string,
): Promise<void> {
  const rawInner = sliceHandler(readComponent(), name);
  const handler = new Function(
    "models",
    "currentModel",
    "ask",
    name === "handleModelDelete" ? "deleteModel" : "cancelDownload",
    "isSessionRefusalError",
    "useModelStore",
    "setSelectionError",
    "t",
    "console",
    `"use strict"; return (async (modelId) => {\n${rawInner}\n});`,
  ) as (
    models: unknown[],
    currentModel: string,
    ask: () => Promise<boolean>,
    action: (modelId: string) => Promise<boolean>,
    isSessionRefusalError: (raw: string) => boolean,
    useModelStore: { getState: () => { error: string | null } },
    setSelectionError: (alert: AlertState) => void,
    t: (key: string, opts?: Record<string, unknown>) => string,
    consoleLike: { error: () => void },
  ) => (modelId: string) => Promise<void>;
  await handler(
    scope.models,
    scope.currentModel,
    scope.ask,
    name === "handleModelDelete" ? scope.deleteModel : scope.cancelDownload,
    scope.isSessionRefusalError,
    scope.useModelStore,
    scope.setSelectionError,
    t,
    quietConsole,
  )(modelId);
}

// 1. KB-155 delete: a stubbed FALSE delete result with a NON-refusal store
//    error (the fs/swap-guard class the finding names) must reach the row
//    alert with the localized deleteFailed copy and the store's recorded
//    cause - not the console.
{
  const scope = makeScope(
    Promise.resolve(false),
    "Failed to delete model: No such file or directory (os error 2)",
  );
  await runHandler("handleModelDelete", scope, "m1");
  assert.equal(
    scope.alerts.length,
    1,
    `KB-155: a failed delete must raise the row alert exactly once`,
  );
  assert.equal(scope.alerts[0].modelId, "m1");
  assert.ok(
    scope.alerts[0].message.includes("settings.models.deleteFailed"),
    "KB-155: the delete failure alert must use the deleteFailed copy",
  );
  assert.ok(
    scope.alerts[0].message.includes("No such file or directory"),
    "KB-155: the alert must carry the store's recorded cause (prefix stripped)",
  );
  assert.ok(
    !scope.alerts[0].message.includes("Failed to delete model:"),
    "KB-155: the raw store prefix must not double up in the message",
  );
}

// 2. The pre-existing live-session refusal branch still wins (KB-117/220).
{
  const scope = makeScope(
    Promise.resolve(false),
    "Failed to delete model: dictation-in-progress: session 7",
  );
  await runHandler("handleModelDelete", scope, "m1");
  assert.equal(scope.alerts.length, 1);
  assert.ok(
    scope.alerts[0].message.includes("settings.models.deleteRefusedLive"),
    "the live-session refusal keeps its dedicated retry-hint copy",
  );
}

// 3. KB-155 delete: a transport REJECTION rides the same alert.
{
  const scope = makeScope(
    Promise.reject(new Error("channel closed before command ran")),
    null,
  );
  await runHandler("handleModelDelete", scope, "m1");
  assert.equal(
    scope.alerts.length,
    1,
    "KB-155: a rejected delete must raise the row alert, not just console.error",
  );
  assert.ok(
    scope.alerts[0].message.includes("settings.models.deleteFailed"),
    "KB-155: the transport-failure alert uses the deleteFailed copy",
  );
}

// 4. A successful delete stays silent.
{
  const scope = makeScope(Promise.resolve(true), null);
  await runHandler("handleModelDelete", scope, "m1");
  assert.equal(scope.alerts.length, 0, "a successful delete raises no alert");
}

// 5. KB-155 cancel: the old handler ignored cancelDownload's result
//    entirely; a stubbed FALSE result with the store's recorded error
//    ("Failed to cancel download: ...") must now reach the row alert with
//    the cancelFailed copy.
{
  const scope = makeScope(
    Promise.resolve(false),
    "Failed to cancel download: cancel token already gone",
  );
  await runHandler("handleModelCancel", scope, "m1");
  assert.equal(
    scope.alerts.length,
    1,
    `KB-155: a failed cancel must raise the row alert exactly once`,
  );
  assert.equal(scope.alerts[0].modelId, "m1");
  assert.ok(
    scope.alerts[0].message.includes("settings.models.cancelFailed"),
    "KB-155: the cancel failure alert must use the cancelFailed copy",
  );
  assert.ok(
    scope.alerts[0].message.includes("cancel token already gone"),
    "KB-155: the alert must carry the store's recorded cause (prefix stripped)",
  );
}

// 6. KB-155 cancel: a transport REJECTION rides the same alert.
{
  const scope = makeScope(
    Promise.reject(new Error("channel closed before command ran")),
    null,
  );
  await runHandler("handleModelCancel", scope, "m1");
  assert.equal(scope.alerts.length, 1);
  assert.ok(
    scope.alerts[0].message.includes("settings.models.cancelFailed"),
    "KB-155: the transport-failure cancel alert uses the cancelFailed copy",
  );
}

// 7. A successful cancel stays silent.
{
  const scope = makeScope(Promise.resolve(true), null);
  await runHandler("handleModelCancel", scope, "m1");
  assert.equal(scope.alerts.length, 0, "a successful cancel raises no alert");
}

// 8. The copy ships in every locale (the row alert is user-facing).
{
  const localesDir = path.join(here, "..", "..", "..", "i18n", "locales");
  const shipped = fs.readdirSync(localesDir);
  assert.ok(shipped.length >= 26, `expected the full locale set`);
  for (const locale of shipped) {
    const models = JSON.parse(
      fs.readFileSync(
        path.join(localesDir, locale, "translation.json"),
        "utf8",
      ),
    ).settings.models;
    for (const key of ["deleteFailed", "cancelFailed"]) {
      assert.ok(
        typeof models[key] === "string" && models[key].includes("{{error}}"),
        `KB-155: ${locale} must ship settings.models.${key} interpolating {{error}}`,
      );
    }
  }
}

console.log(
  "modelActionErrorPins (KB-155): delete and cancel failures reach the row alert in every resolution shape",
);
