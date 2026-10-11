import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";

// KB-158 (frontend half): the acceleration probe is an async IPC roundtrip
// (get_available_accelerators). Before the fix the component's .then chain
// had no .catch, so a rejected probe (a panicked backend task, a dead IPC
// channel) surfaced as an unhandled promise rejection while the dropdowns
// hung waiting on state that never arrives. The fix catches and logs,
// leaving the dropdowns in their empty fallback. The backend half (the
// spawn_blocking JoinError answered with empty lists) lives inline in the
// AppHandle-typed command and is disclosed as not unit-testable here.
//
// AccelerationSelector is a Tauri settings component with no React render
// harness in this repo, so this is a static source assertion in the repo's
// assert-script style (same technique as Footer.test.ts).

const selectorSrc = fs.readFileSync(
  path.join(import.meta.dirname, "AccelerationSelector.tsx"),
  "utf8",
);

// The probe call itself must keep existing. If this fails, the component
// stopped probing commands.getAvailableAccelerators() at all and this file
// needs updating - that is not the KB-158 failure.
assert.ok(
  /commands\s*\n?\s*\.getAvailableAccelerators\(\)/.test(selectorSrc),
  "AccelerationSelector must keep probing commands.getAvailableAccelerators()",
);

// The contract: the probe's promise chain ends in a .catch attached to the
// same chain (the then callback closes right before it), so a rejection can
// never surface as an unhandled rejection.
const probeHasCatch =
  /getAvailableAccelerators\(\)\s*\.\s*then\([\s\S]{0,2500}?\}\)[\s\S]{0,400}?\.catch\(/;
assert.ok(
  probeHasCatch.test(selectorSrc),
  "KB-158: the getAvailableAccelerators() promise chain must end in a .catch; a rejected probe leaves the dropdowns in the empty fallback instead of an unhandled rejection",
);

console.log("AccelerationSelector: all assertions passed");
