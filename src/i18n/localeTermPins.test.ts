import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";

/* KB-258 residual pin (fleet, cycle 14): the Portuguese hint string for
 * the phone post-process toggle shipped the false friend "ditadura"
 * (DICTATORSHIP) where every other string in the locale file says "ditado"
 * (dictation, 30+ occurrences), so users were told the toggle "takes
 * effect from the next dictatorship". The lone pre-existing twin at
 * models.deleteRefusedLive (the string the KB-155 row alert renders) had
 * the same wrong token. Both are corrected to the locale's established
 * term, and the embedded companion blob regenerated, so the token must
 * never reappear in either the source locale or the shipped blob. */

const here = import.meta.dirname;
const repoRoot = path.join(here, "..", "..");

const pt = fs.readFileSync(
  path.join(repoRoot, "src", "i18n", "locales", "pt", "translation.json"),
  "utf8",
);
const blob = fs.readFileSync(
  path.join(repoRoot, "src-tauri", "src", "companion", "strings_gen.rs"),
  "utf8",
);

// 1. The false friend is gone from the source locale entirely.
assert.ok(
  !pt.includes("ditadura"),
  "KB-258: the pt locale must say ditado (dictation), never ditadura (dictatorship)",
);

// 2. Both corrected strings carry the established term.
assert.ok(
  pt.includes('"postProcessHint": "Surte efeito a partir do próximo ditado."'),
  "KB-258: the companion postProcessHint must use the ditado term",
);
assert.ok(
  pt.includes("depois que o ditado terminar"),
  "KB-258: deleteRefusedLive must use the ditado term too",
);

// 3. The regenerated embedded blob ships the corrected hint (the phone
//    page reads the blob, not the locale file).
assert.ok(
  !blob.includes("ditadura"),
  "KB-258: the embedded companion blob must not ship the ditadura token",
);
assert.ok(
  blob.includes('"postProcessHint":"Surte efeito a partir do próximo ditado."'),
  "KB-258: the embedded blob must carry the corrected pt hint",
);

console.log(
  "localeTermPins (KB-258): pt says ditado everywhere, blob included",
);
