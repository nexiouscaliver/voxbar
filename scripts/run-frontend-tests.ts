// Runs every assert-style frontend test file (git-tracked src, docs, and
// ui-shots *.test.ts).
//
// These tests are node:assert scripts, not bun:test modules, so `bun test`
// cannot collect them; each file is executed directly and must exit 0. The
// sweep exists so CI runs the whole suite (the KB-136 failure class: test
// files that exist but are wired to no script and no CI step rot silently).
// Exits nonzero on the first failing file.
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
  const listing = Bun.spawnSync(["git", "ls-files", glob], {
    stdout: "pipe",
    stderr: "inherit",
  });
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

for (const file of uniqueFiles) {
  const run = Bun.spawnSync(["bun", file], {
    stdout: "inherit",
    stderr: "inherit",
  });
  if (run.exitCode !== 0) {
    console.error(`FAIL ${file} (exit ${run.exitCode})`);
    process.exit(run.exitCode ?? 1);
  }
  console.log(`ok   ${file}`);
}

console.log(`all ${uniqueFiles.length} frontend test files passed`);
