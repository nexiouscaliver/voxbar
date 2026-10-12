import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

/* KB-021 pin: the voice-deletion description teaches spoken phrases, so the
 * phrases it advertises must be the ones the recognizer actually carries.
 * All 26 locales shipped the inverse: they quoted the dead "start over"
 * (deliberately NOT a default phrase in command_matrix.rs - it is ordinary
 * English like "let me start over and try again") and hid the live
 * "scratch everything". The undo-shortcut description is exempt: its
 * "(start over)" is prose about the keyboard action, not a quoted spoken
 * phrase.
 *
 * Two-sided pin:
 *   1. the Rust default table's ClearAll arm carries "scratch everything"
 *      and not "start over" (the ground truth);
 *   2. every locale's voiceDeletion.description quotes "scratch everything"
 *      and never quotes "start over".
 */

const here = path.dirname(fileURLToPath(import.meta.url));
const localesDir = path.join(here, "locales");

// 1. The recognizer's ground truth: command_matrix.rs's ClearAll default
// phrases. Scoped to the ClearAll arm so a phrase listed for another
// command cannot satisfy the pin.
const matrixSource = fs.readFileSync(
  path.join(here, "../../src-tauri/src/audio_toolkit/command_matrix.rs"),
  "utf8",
);
const clearAllAt = matrixSource.indexOf("(ClearAll, &[");
const clearAllEnd = matrixSource.indexOf("]),", clearAllAt);
assert.ok(
  clearAllAt >= 0 && clearAllEnd > clearAllAt,
  "the ClearAll default phrase arm must exist",
);
const clearAllArm = matrixSource.slice(clearAllAt, clearAllEnd);
assert.ok(
  clearAllArm.includes('"scratch everything"'),
  'the ClearAll defaults must carry the live "scratch everything" phrase',
);
assert.ok(
  !clearAllArm.includes('"start over"'),
  '"start over" is deliberately not a default phrase (ordinary English; see the arm\'s comment)',
);

// 2. Every locale's copy advertises the live phrase and drops the dead one.
const locales = fs
  .readdirSync(localesDir, { withFileTypes: true })
  .filter((entry) => entry.isDirectory())
  .map((entry) => entry.name);
assert.ok(
  locales.length >= 26,
  `expected the full locale set, saw ${locales.length}`,
);

for (const locale of locales) {
  const data = JSON.parse(
    fs.readFileSync(path.join(localesDir, locale, "translation.json"), "utf8"),
  ) as {
    settings?: {
      advanced?: {
        voiceDeletion?: { description?: string };
      };
    };
  };
  const description = data.settings?.advanced?.voiceDeletion?.description ?? "";
  assert.ok(
    description.length > 0,
    `${locale}: settings.advanced.voiceDeletion.description must exist`,
  );
  assert.ok(
    description.includes('"scratch everything"'),
    `${locale}: the description must advertise the live "scratch everything" phrase`,
  );
  assert.ok(
    !description.includes('"start over"'),
    `${locale}: the description must not advertise the dead "start over" phrase (KB-021)`,
  );
}

console.log("voiceDeletionPhrases tests passed");
