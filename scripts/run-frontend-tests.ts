// Runs every assert-style frontend test file (src, docs, and ui-shots
// *.test.ts): git-tracked plus untracked-but-not-ignored files, so a
// brand-new local test file runs before it is ever committed (what CI
// would see on the next fresh checkout).
//
// These tests are node:assert scripts, not bun:test modules, so `bun test`
// cannot collect them; each file is executed directly and must exit 0. The
// sweep exists so CI runs the whole suite (the KB-136 failure class: test
// files that exist but are wired to no script and no CI step rot silently).
// Runs every file even when earlier ones fail, reports each failing file,
// and exits nonzero if any file failed (KB-251: the first failure must not
// mask the state of the rest of the suite).
//
// The globs are explicit (not a bare **/*.test.ts) so scripts/ stays out:
// ciTestCoverage.test.ts is a meta-guard with its own CI step, not an app
// test, and running it from inside the sweep it guards would be circular.
// Note git's ** pathspec needs a subdirectory - a *.test.ts sitting
// directly in src/, docs/, or ui-shots/ needs the single-star form too.

const TEST_GLOBS = [
  "src/**/*.test.ts",
  "src/*.test.ts",
  "docs/**/*.test.ts",
  "docs/*.test.ts",
  "ui-shots/**/*.test.ts",
  "ui-shots/*.test.ts",
];

const files: string[] = [];
for (const glob of TEST_GLOBS) {
  // --cached lists tracked files, --others --exclude-standard adds
  // untracked files git does not ignore; plain `git ls-files` alone
  // would hide uncommitted local test files from the sweep.
  const listing = Bun.spawnSync(
    ["git", "ls-files", "--cached", "--others", "--exclude-standard", glob],
    {
      stdout: "pipe",
      stderr: "inherit",
    },
  );
  if (listing.exitCode !== 0) {
    console.error(`failed to list test files: git ls-files "${glob}"`);
    process.exit(listing.exitCode ?? 1);
  }
  files.push(
    ...listing.stdout
      .toString()
      .split("\n")
      .map((line) => line.trim())
      .filter((line) => line.length > 0),
  );
}
files.sort();
const uniqueFiles = [...new Set(files)];

if (uniqueFiles.length === 0) {
  // An empty sweep would be a silent green CI: fail loudly instead.
  console.error(
    `no test files matched [${TEST_GLOBS.join(", ")}] (did the globs break?)`,
  );
  process.exit(1);
}

console.log(`running ${uniqueFiles.length} frontend test files`);

// Collect every failure instead of exiting at the first: one broken test
// file must not mask whether the rest of the suite still passes (KB-251).
const failures: Array<{ file: string; exitCode: number }> = [];
for (const file of uniqueFiles) {
  const run = Bun.spawnSync(["bun", file], {
    stdout: "inherit",
    stderr: "inherit",
  });
  if (run.exitCode !== 0) {
    console.error(`FAIL ${file} (exit ${run.exitCode})`);
    failures.push({ file, exitCode: run.exitCode ?? 1 });
    continue;
  }
  console.log(`ok   ${file}`);
}

if (failures.length > 0) {
  console.error(
    `${failures.length} of ${uniqueFiles.length} frontend test files failed:` +
      failures.map((f) => `\n  ${f.file} (exit ${f.exitCode})`).join(""),
  );
  process.exit(failures[0].exitCode);
}

console.log(`all ${uniqueFiles.length} frontend test files passed`);
