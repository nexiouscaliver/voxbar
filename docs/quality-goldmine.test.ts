import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

// AUD-15 — docs ledger drift after the cycle-3 wave-1 fixes (a9e1159e..a6997aec).
//
// The goldmine is the repo's bug ledger. Its entry convention is a bold
// heading span whose STATUS slot sits between the id and the one-line claim:
//
//   **KB-150 · FIXED · <one-line claim>.** <evidence prose…>
//   **KB-002 · FIXED · cancel leaks the forced system mute.** …
//   **KB-104 · PARTIALLY FIXED.** … (+ a residual line)
//   **KB-016 · OPEN (re-verified 3×).** …   <- stale after wave 1
//
// 17 items were fixed by the wave-1 commits but their headings still read
// OPEN (or carry no status at all, e.g. `**KB-038**`, `**KB-158 ·**`).
// This test pins the ledger: every wave-1-fixed id must carry a FIXED marker
// in its status slot and must not read OPEN there any more. Partial fixes
// (KB-020/033/148/154/190) are expected to keep a residual line —
// `· PARTIALLY FIXED ·` satisfies the FIXED-marker check like `· FIXED ·`.

const docsDir = dirname(fileURLToPath(import.meta.url));
const goldmine = readFileSync(join(docsDir, "quality-goldmine.md"), "utf8");

// The 17 ids the wave-1 commits (a9e1159e..a6997aec) claim fixed.
const WAVE_1_FIXED = [
  "016",
  "020",
  "027",
  "031",
  "033",
  "034",
  "038",
  "107",
  "148",
  "151",
  "154",
  "158",
  "162",
  "185",
  "187",
  "190",
  "193",
] as const;

// Status-slot contract: the first non-empty `·`-separated segment after the
// id inside the entry's bold heading span (`**KB-<id> · STATUS …**`).
// Entries written with no status (`**KB-038** …`, `**KB-158 ·** …`) have an
// empty slot. Only the slot is examined, so OPEN/FIXED words in the prose of
// the same heading line (e.g. KB-033's "fails OPEN while updateChecksLocked
// is still null" residual, or a "fade-window TOCTOU open" residual line)
// can never trip these checks.
const entryStatus = (md: string, id: string): string => {
  const span = md.match(new RegExp(`\\*\\*KB-${id}(?![0-9])([^\\n]*?)\\*\\*`));
  assert.ok(
    span,
    `KB-${id} has no bold entry heading (**KB-${id} …**) in quality-goldmine.md`,
  );
  const segments = (span[1] ?? "").split("·").map((s) => s.trim());
  return segments.find((s) => s.length > 0) ?? "";
};

for (const id of WAVE_1_FIXED) {
  const status = entryStatus(goldmine, id);
  assert.ok(
    !/\bOPEN\b/.test(status),
    `KB-${id} heading still reads OPEN (status slot: "${status.slice(0, 80)}") — ` +
      `the wave-1 commits claim it fixed; flip the entry to the FIXED convention ` +
      `(· FIXED · or, where partial, · PARTIALLY FIXED · + residual) with ` +
      `one-line evidence (commit sha + what landed)`,
  );
  assert.ok(
    /\bFIXED\b/.test(status),
    `KB-${id} heading has no FIXED marker (status slot: "${status.slice(0, 80)}") — ` +
      `expected the repo's · FIXED · convention (PARTIALLY FIXED also accepted)`,
  );
}

// The cycle3 assessment (voxbar-build-docs/cycle3-assessment.md, OUTSIDE the
// worktree) carries two label errors this worktree must record corrections
// for. No worktree copy of the assessment exists to correct in place, so the
// deltas land in docs/cycle3-assessment-corrections.md:
//   1. wave 1 is labeled "All P1" but KB-158/187/190/193 sit in the
//      goldmine's P2 section (## P2 starts before their entries);
//   2. wave 2's class enumeration omits KB-162/163.
const correctionsPath = join(docsDir, "cycle3-assessment-corrections.md");
assert.ok(
  existsSync(correctionsPath),
  "docs/cycle3-assessment-corrections.md is missing — it must summarize the " +
    "assessment corrections: the wave-1 P1/P2 label fix (wave 1 is not " +
    "'All P1'; KB-158/187/190/193 are P2 in the goldmine) and the wave-2 " +
    "additions of KB-162/163",
);
const corrections = readFileSync(correctionsPath, "utf8");

// Id mentions must be found even when the doc compresses lists
// ("KB-158/187/190/193"), the way the audit report itself writes them.
const mentionedIds = (text: string): Set<string> => {
  const ids = new Set<string>();
  for (const m of text.matchAll(/KB-(\d{3})/g)) ids.add(m[1]);
  for (const m of text.matchAll(/KB-\d{3}(?:\/\d{3})+/g)) {
    for (const part of m[0].split(/KB-|\//)) {
      if (/^\d{3}$/.test(part)) ids.add(part);
    }
  }
  return ids;
};
const named = mentionedIds(corrections);

for (const id of ["158", "187", "190", "193"]) {
  assert.ok(
    named.has(id),
    `corrections file must name KB-${id} as part of the wave-1 P1/P2 label fix`,
  );
}
assert.ok(
  /P1/.test(corrections) && /P2/.test(corrections),
  "corrections file must state the P1/P2 relabel (wave 1 says 'All P1' but " +
    "KB-158/187/190/193 are P2 in the goldmine)",
);
assert.ok(
  /wave[\s-]*1/i.test(corrections),
  "corrections file must tie the label fix to wave 1",
);
for (const id of ["162", "163"]) {
  assert.ok(
    named.has(id),
    `corrections file must name KB-${id} as a wave-2 addition`,
  );
}
assert.ok(
  /wave[\s-]*2/i.test(corrections),
  "corrections file must tie the KB-162/163 additions to wave 2",
);

console.log("quality-goldmine ledger (AUD-15) tests passed");
