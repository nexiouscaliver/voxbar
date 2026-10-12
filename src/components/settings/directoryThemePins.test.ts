import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";

/* KB-092 pin: the App Data Directory and Log Directory cards hardcoded a
 * light-theme skeleton (bg-gray-100/200) and a light-theme error wash
 * (bg-red-50 border-red-200 text-red-600). On the dark theme the skeleton
 * glowed and the error block kept light-theme reds. Both now carry theme
 * tokens: mid-gray washes for the skeleton and the semantic error pair
 * (the ui/Alert error recipe) for failures. Static source assertions in
 * the repo's assert-script style (same technique as Footer.test.ts). */

const here = import.meta.dirname;
const appData = fs.readFileSync(
  path.join(here, "AppDataDirectory.tsx"),
  "utf8",
);
const logDir = fs.readFileSync(
  path.join(here, "debug/LogDirectory.tsx"),
  "utf8",
);

for (const [name, source] of [
  ["AppDataDirectory.tsx", appData],
  ["LogDirectory.tsx", logDir],
] as const) {
  // No hardcoded light grays or reds survive in any className.
  for (const dead of [
    "bg-gray-100",
    "bg-gray-200",
    "bg-red-50",
    "border-red-200",
    "text-red-600",
  ]) {
    assert.ok(
      !new RegExp(`className="[^"]*${dead.replace("/", "\\/")}`).test(source),
      `KB-092: ${name} must not use the hardcoded light-theme class ${dead}`,
    );
  }
  // The tokenized replacements are present.
  assert.ok(
    source.includes("bg-mid-gray/10") || source.includes("bg-mid-gray/20"),
    `KB-092: ${name} skeleton must wash with the mid-gray token`,
  );
  assert.ok(
    source.includes("bg-error/10"),
    `KB-092: ${name} error block must use the semantic error token`,
  );
}

console.log("directoryThemePins tests passed");
