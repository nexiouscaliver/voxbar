import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { isSessionRefusalError, SESSION_REFUSAL_PREFIX } from "./modelRefusals";

// The delete-error classification (KB-117/KB-220): only the live-session
// refusal gets the "try again after the dictation" copy; every other error
// (disk error, missing file) is a real failure and keeps its existing
// console-only handling.
assert.equal(
  isSessionRefusalError(
    "dictation-in-progress: a dictation session is in progress using this model, try again after it ends",
  ),
  true,
  "the backend refusal string must classify as the live-session refusal",
);
assert.equal(
  isSessionRefusalError(
    "failed to delete model file: No such file or directory",
  ),
  false,
  "an IO error must NOT classify as the live-session refusal",
);
assert.equal(
  isSessionRefusalError(
    "a dictation session is in progress using this model, try again after it ends",
  ),
  false,
  "the refusal sentence WITHOUT the stable prefix is treated as a real failure",
);

// The prefix is a cross-language contract: the frontend classifier breaks
// silently if the backend constant drifts, so pin both copies together
// (mirrors the SWAP_REFUSAL_PREFIX sync test in localLlmRouting.test.ts).
const backendSource = fs.readFileSync(
  path.join(
    import.meta.dirname,
    "..",
    "..",
    "..",
    "..",
    "src-tauri",
    "src",
    "commands",
    "models.rs",
  ),
  "utf8",
);
const backendMatch = backendSource.match(
  /pub const SESSION_REFUSAL_PREFIX: &str = "([^"]+)";/,
);
assert.ok(backendMatch, "SESSION_REFUSAL_PREFIX must exist in the backend");
assert.equal(
  backendMatch[1],
  SESSION_REFUSAL_PREFIX,
  "frontend and backend SESSION_REFUSAL_PREFIX must stay in sync",
);

console.log("modelRefusals: all assertions passed");
