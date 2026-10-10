// Runs every assert-style frontend test file (git-tracked src/**/*.test.ts).
//
// These tests are node:assert scripts, not bun:test modules, so `bun test`
// cannot collect them; each file is executed directly and must exit 0. The
// sweep exists so CI runs the whole suite (the KB-136 failure class: test
// files that exist but are wired to no script and no CI step rot silently).
// Exits nonzero on the first failing file.

const TEST_GLOB = "src/**/*.test.ts";

const listing = Bun.spawnSync(["git", "ls-files", TEST_GLOB], {
  stdout: "pipe",
  stderr: "inherit",
});
if (listing.exitCode !== 0) {
  console.error(`failed to list test files: git ls-files "${TEST_GLOB}"`);
  process.exit(listing.exitCode ?? 1);
}

const files = listing.stdout
  .toString()
  .split("\n")
  .map((line) => line.trim())
  .filter((line) => line.length > 0)
  .sort();

if (files.length === 0) {
  // An empty sweep would be a silent green CI: fail loudly instead.
  console.error(`no test files matched "${TEST_GLOB}" (did the glob break?)`);
  process.exit(1);
}

console.log(`running ${files.length} frontend test files`);

for (const file of files) {
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

console.log(`all ${files.length} frontend test files passed`);
