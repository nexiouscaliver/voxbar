import assert from "node:assert/strict";
import fs from "fs";
import path from "path";
import { fileURLToPath } from "url";

// AUD-04: CI must invoke every bun test. package.json grew test:notice and
// siblings beyond the single test:accent step ci.yml runs (the KB-136
// pattern repeating), and three assert-script files have no test:* script
// at all — so nothing in CI ever executes them. This guard parses
// package.json + .github/workflows/ci.yml and fails, listing every bun test
// that no CI step runs.
//
// A test counts as run when its .test.ts file path appears in what CI
// executes — either a step invoking it directly (`bun src/….test.ts`), a
// step invoking its script (`bun run test:notice`), or a step invoking any
// package.json wrapper script that (transitively) chains those, so the fix
// may be explicit steps or one chained script.
//
// Scope: test:playwright* drive real browsers and are excluded. The
// assert-file list is the audits' enumeration (commandGroups,
// skipToastDedupe, historyLimitInput, quality-goldmine, shoot); extending
// it to any further uncovered *.test.ts files is welcome but not required
// to go green.

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.join(__dirname, "..");

interface PackageJson {
  scripts?: Record<string, string>;
}

const pkg: PackageJson = JSON.parse(
  fs.readFileSync(path.join(repoRoot, "package.json"), "utf8"),
);
const scripts = pkg.scripts ?? {};
const ciYaml = fs.readFileSync(
  path.join(repoRoot, ".github", "workflows", "ci.yml"),
  "utf8",
);

// Assert-script files the audit named: no test:* script invokes them, so a
// CI step (or a chained wrapper script) must run them directly.
const REQUIRED_ASSERT_FILES = [
  "src/components/settings/commands/commandGroups.test.ts",
  "src/components/settings/post-processing/skipToastDedupe.test.ts",
  "src/components/settings/historyLimitInput.test.ts",
  "docs/quality-goldmine.test.ts",
  "ui-shots/shoot.test.ts",
];

// Every `run:` command ci.yml executes: inline values plus block scalars
// (`run: |`, possibly with a strip indicator). `uses:` and `name:` lines
// are ignored — a comment or step name mentioning a test must never count —
// and comment lines INSIDE a block scalar are skipped too: they are YAML
// comments the shell never executes, so counting them would let a `# runs
// foo.test.ts` note satisfy coverage for a file nothing actually runs.
function ciRunCommands(yaml: string): string[] {
  const lines = yaml.split(/\r?\n/);
  const commands: string[] = [];
  for (let i = 0; i < lines.length; i++) {
    const block = lines[i]!.match(/^(\s*)run:\s*[|>]-?\s*$/);
    if (block) {
      const indent = block[1]!.length;
      let j = i + 1;
      while (j < lines.length) {
        const line = lines[j]!;
        if (line.trim() === "") {
          j++;
          continue;
        }
        const lineIndent = line.match(/^\s*/)?.[0]?.length ?? 0;
        if (lineIndent <= indent) break;
        if (line.trimStart().startsWith("#")) {
          j++;
          continue;
        }
        commands.push(line);
        j++;
      }
      i = j - 1;
      continue;
    }
    const inline = lines[i]!.match(/^\s*run:\s*(.+)$/);
    if (inline) commands.push(inline[1]!);
  }
  return commands;
}

// Inline `bun run <script>` (also npm/yarn/pnpm forms, and bun's bare
// `bun <script>`) with the script's command text, recursively, so a wrapper
// script (test:all -> `bun run test:keyboard && bun run test:accent && …`)
// counts as invoking everything it chains.
function expandScriptInvocations(
  text: string,
  scripts: Record<string, string>,
): string {
  const escapeRe = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  let expanded = text;
  for (let round = 0; round < 10; round++) {
    let next = expanded;
    for (const [name, command] of Object.entries(scripts)) {
      const re = new RegExp(
        `\\b(?:bun|npm|yarn|pnpm)(?: run)? ${escapeRe(name)}(?![\\w:-])`,
        "g",
      );
      next = next.replace(re, () => command);
    }
    if (next === expanded) break;
    expanded = next;
  }
  return expanded;
}

const executedByCi = expandScriptInvocations(
  ciRunCommands(ciYaml).join("\n"),
  scripts,
);

const missing: string[] = [];

// Every non-playwright test:* script must have ALL of its .test.ts files
// executed by CI. A script may chain several files (test:updater runs
// two) - matching only the first would let a chained leaf silently rot -
// and a test script without any direct .test.ts argument is a chain node
// (its leaves are separately required), so it is skipped here.
for (const [name, command] of Object.entries(scripts)) {
  if (!name.startsWith("test:") || command.includes("playwright")) continue;
  const files = command.match(/\S*\.test\.ts/g) ?? [];
  if (files.length === 0) continue;
  for (const file of files) {
    if (!executedByCi.includes(file)) {
      missing.push(`  - ${name} (${command}) does not run ${file} in CI`);
    }
  }
}

for (const file of REQUIRED_ASSERT_FILES) {
  if (!executedByCi.includes(file)) {
    missing.push(`  - assert-script ${file} (no test:* script; run the file)`);
  }
}

assert.equal(
  missing.length,
  0,
  `AUD-04 CI gap: bun tests never invoked by any step in .github/workflows/ci.yml — add steps (or one chained script) that run them:\n${missing.join("\n")}`,
);

console.log("ciTestCoverage: every bun test script and assert file runs in CI");
