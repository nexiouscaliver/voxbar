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

// -- Round 11 (v1.6.4 stabilization) reconciliation pins ------------------
//
// The round 11 pass reconciled the ledger against the code the v1.6.4
// cycle landed on. Three invariants are pinned here:
//
// 1. KB-275's round 9 entry is no longer a blanket OPEN: the fleet
//    commits 81dae89a/520a47e6 closed its lane-machine-reset gap after
//    that section was finalized, so the entry must read PARTIALLY FIXED
//    with the no-re-point half named as the residual. The round 9
//    heading is a MULTI-LINE bold span, which entryStatus's single-line
//    regex cannot see (its first matchable KB-275 span was the round 11
//    flip note, so the first cut of this pin enforced nothing); the
//    check therefore uses a newline-tolerant span reader whose first
//    KB-275 span is the round 9 entry itself (round 10's queue lists
//    ids unbolded). Mutation-checked: rewriting the round 9 slot back
//    to a blanket OPEN trips it.
// 2. The round 11 section records a FIXED marker for every round 10 id
//    the cycle's six commits closed, keeps KB-264/KB-271 as the open
//    leftovers, and carries the six named reconciliations (KB-236 was
//    reconciled open with fresh anchors at round 11 and closed by the
//    v1.6.5 residuals cycle, lane R3; KB-237/254/255/241/245 verified).
//    Scoped to the
//    section's text because rounds 9 and 10 reused these ids with
//    different meanings (the round 9 correction-log convention).
// 3. The round 9 KB-085 residual's shutdown-send anchor must point at
//    the line companion/mod.rs actually carries it on (the v1.6.4
//    companion rewrite moved it; the review round caught the stale
//    citation), so the anchor is checked against the live source file.

const round11Start = goldmine.indexOf("# Round 11");
assert.ok(
  round11Start !== -1,
  "quality-goldmine.md has no Round 11 section — the v1.6.4 cycle's " +
    "closures and reconciliations must be recorded there",
);
const round11 = goldmine.slice(round11Start);

// Same contract as entryStatus, but the bold span may wrap across lines
// (the ledger wraps long headings), so `[\s\S]` replaces `[^\\n]`.
const entryStatusMultiline = (md: string, id: string): string => {
  const span = md.match(
    new RegExp(`\\*\\*KB-${id}(?![0-9])([\\s\\S]*?)\\*\\*`),
  );
  assert.ok(
    span,
    `KB-${id} has no bold entry heading (**KB-${id} …**) in quality-goldmine.md`,
  );
  const segments = (span[1] ?? "").split("·").map((s) => s.trim());
  return segments.find((s) => s.length > 0) ?? "";
};

const kb275Status = entryStatusMultiline(goldmine, "275");
assert.match(
  kb275Status,
  /PARTIALLY FIXED/,
  `KB-275's round 9 entry (the first KB-275 bold span, read across its ` +
    `wrapped lines) must read PARTIALLY FIXED — the post-round-9 fleet ` +
    `commits 81dae89a/520a47e6 closed its lane-machine-reset gap; the ` +
    `no-re-point half is the residual (status slot: ` +
    `"${kb275Status.slice(0, 80)}")`,
);
assert.ok(
  !/^\s*OPEN/.test(kb275Status),
  `KB-275's round 9 status slot must not read as a blanket OPEN any more ` +
    `(status slot: "${kb275Status.slice(0, 80)}")`,
);

// KB-085 residual anchor vs the live source: the round 9 entry cites the
// shutdown send inside stop(), so the cited line must be where mod.rs
// actually sends it today.
const companionModPath = join(
  docsDir,
  "..",
  "src-tauri",
  "src",
  "companion",
  "mod.rs",
);
const companionMod = readFileSync(companionModPath, "utf8");
const shutdownSendLine =
  companionMod
    .split("\n")
    .findIndex((l) => l.includes("shutdown_tx.send(true)")) + 1;
assert.ok(
  shutdownSendLine > 0,
  "companion/mod.rs no longer contains the shutdown send `shutdown_tx.send(true)` — " +
    "update the KB-085 residual anchor and this pin together",
);
const kb085Entry = goldmine.match(/- \*\*KB-085 residual:\*\*[\s\S]{0,700}/);
assert.ok(
  kb085Entry,
  "the round 9 still-open list must carry a bold KB-085 residual entry",
);
assert.ok(
  kb085Entry[0].includes(`companion/mod.rs:${shutdownSendLine}`),
  `KB-085's residual must cite the CURRENT shutdown-send line ` +
    `(companion/mod.rs:${shutdownSendLine}); the v1.6.4 companion rewrite ` +
    `moved it once already`,
);

const ROUND_11_FIXED = [
  "260",
  "261",
  "262",
  "263",
  "265",
  "266",
  "267",
  "268",
  "269",
  "270",
  "272",
  "273",
  "275",
  "276",
  "277",
  "278",
] as const;
for (const id of ROUND_11_FIXED) {
  assert.ok(
    round11.includes(`**KB-${id} · FIXED`),
    `the Round 11 section must record KB-${id} (round 10 meaning) as FIXED ` +
      `with its landing commit`,
  );
}
for (const id of ["264", "271"] as const) {
  assert.ok(
    new RegExp(`\\*\\*KB-${id}\\*\\*`).test(round11),
    `the Round 11 section must keep KB-${id} in its still-open-from-round-10 ` +
      `list with a current anchor`,
  );
}
for (const marker of [
  "KB-236 · CLOSED in the v1.6.5 residuals cycle (lane R3)",
  "KB-237 · FIXED",
  "KB-254 · FIXED",
  "KB-255 · FIXED",
  "KB-241 · FIXED",
  "KB-245 · FIXED",
] as const) {
  assert.ok(
    round11.includes(marker),
    `the Round 11 section must carry the "${marker}" reconciliation verdict`,
  );
}
assert.ok(
  goldmine.includes("Clarified round 11"),
  "the round 10 six-pack tail must carry its round 11 clarification " +
    "(only KB-258 of it was open when the queue was cut)",
);
assert.ok(
  goldmine.includes("handy_keys.rs:421-423"),
  "KB-236's residual must keep its round 11 refreshed anchor " +
    "(the Already-recording refusal seam)",
);

// -- Round 12 / v1.6.6 queue pins (fleet, cycle 15) -----------------------
//
// The v1.6.6 companion-machinery cycle closed the round 12 queue and its
// round 15 section recorded the goal state as a one-time manual sweep:
// all twenty entries (KB-279..KB-298) CLOSED with pins named, KB-261 the
// one non-closed STATUS SLOT (DEFERRED-UPSTREAM). Nothing persisted
// those checks - the round 12 entries are list-form ("- KB-280 CLOSED
// ..."), which the status-slot machinery above never sees - so any
// future reopen or status-word drift would trip nothing. These pins
// persist the sweep: a reopened entry, a drifted CLOSED marker, or a
// lost DEFERRED-UPSTREAM verdict now fails CI.
{
  const queueStart = goldmine.indexOf("## Round 12 queue");
  assert.ok(
    queueStart !== -1,
    "quality-goldmine.md has no Round 12 queue section",
  );
  const queueEnd = goldmine.indexOf("\n## ", queueStart + 1);
  const queue = goldmine.slice(
    queueStart,
    queueEnd === -1 ? undefined : queueEnd,
  );
  for (let id = 279; id <= 298; id++) {
    const padded = String(id);
    assert.ok(
      queue.includes(`- KB-${padded} CLOSED `) ||
        queue.includes(`- KB-${padded} CLOSED\n`),
      `the Round 12 queue entry for KB-${padded} must keep its CLOSED marker - ` +
        `the cycle closed all twenty; a reopen must be a deliberate, visible flip`,
    );
  }
  // KB-261's standing verdict, pinned on the P1 entry's status slot.
  // The FIRST bold KB-261 span in the file is an early summary line, so
  // the pin keys on the entry heading's own shape (id + separator +
  // verdict); later sections reuse the id with other meanings and their
  // own FIXED markers.
  assert.ok(
    goldmine.includes("**KB-261 · DEFERRED-UPSTREAM"),
    "KB-261's P1 entry must stand DEFERRED-UPSTREAM - the ggml " +
      "duplicate-symbol investigation is deferred upstream at the pinned " +
      "crate versions; flipping it to closed or open needs the entry's " +
      "own evidence",
  );
}

// Fleet pass 2 (v1.6.4 review): the ledger cites companion-file
// path:line anchors in every round, and the pass 1 companion rewrite
// moved them wholesale - 13+ citations in the round 10/11 sections alone
// went stale while this suite stayed green (it live-checked exactly ONE
// anchor, KB-085's). The doc's own promise ("every status flip cites the
// commit or path:line it was verified against") needs every CITATION to
// be live-checked. Every companion/mod.rs and companion/server.rs
// citation in the ledger must appear in the table below with the token
// its cited line must carry. Fleet, cycle 15: the regex is WIDENED to
// the other three files the ledger cites with path:line anchors -
// transcription_coordinator.rs (the cycle's most-churned file, +372
// lines, carrying eight new citations the companion-only regex could
// not see), actions.rs, and settings.rs - so a citation drifts stale in
// any of the five files, or a new citation appears without a row, and
// this pin fails. Anchors in files outside these five (audio.rs, utils,
// frontend files - disclosed; the KB-085 check above remains the canary
// for the pattern) are out of this pin's scope.
{
  const readSrc = (rel: string): string[] =>
    readFileSync(join(docsDir, "..", "src-tauri", "src", rel), "utf8").split(
      "\n",
    );
  const sources: Record<string, string[]> = {
    mod: readSrc("companion/mod.rs"),
    server: readSrc("companion/server.rs"),
    transcription_coordinator: readSrc("transcription_coordinator.rs"),
    actions: readSrc("actions.rs"),
    settings: readSrc("settings.rs"),
  };
  // The citation text names these files without their directory prefixes
  // except the companion pair; this maps the regex's stem to both the
  // sources key and the human-readable path in failure messages.
  const citeLabel = (kind: string): string =>
    kind === "mod" || kind === "server" ? `companion/${kind}.rs` : `${kind}.rs`;

  // [file, firstLine, lastLine, token that must appear within the span]
  const anchorTable: [
    fileKind:
      | "mod"
      | "server"
      | "transcription_coordinator"
      | "actions"
      | "settings",
    a: number,
    b: number,
    token: string,
  ][] = [
    ["mod", 1471, 1471, "fn effective_enabled("],
    ["server", 981, 981, "mod tests {"],
    [
      "mod",
      1872,
      1872,
      "fn registrations_release_by_lane_and_generation_never_by_name()",
    ],
    ["mod", 1348, 1348, "fn note_stopping_session"],
    ["mod", 837, 839, "close_remote_stream_or_defer();"],
    ["mod", 825, 825, "shutdown_tx.send(true)"],
    ["mod", 737, 737, "spawn(server::run("],
    ["mod", 528, 528, "fn freeze_session("],
    [
      "mod",
      1617,
      1617,
      "fn session_identity_freezes_per_lane_and_cannot_be_renamed()",
    ],
    [
      "mod",
      2222,
      2222,
      "fn kb261_concurrent_same_platform_registrations_never_share_an_identity()",
    ],
    ["server", 313, 313, "NO pause acknowledgement"],
    [
      "server",
      291,
      291,
      "manager.unregister_connection(device_lane, device_generation);",
    ],
    ["server", 297, 297, "notify_lane_gone(device_lane);"],
    ["server", 364, 364, "finalize_companion_lane(device_lane);"],
    ["server", 436, 436, "chunks admission ACCEPTED"],
    ["mod", 1007, 1026, "fn reset_pairing("],
    ["mod", 1118, 1118, "CONNECT_EDGE_SETTINGS_RMW: Mutex<()>"],
    ["mod", 1141, 1141, "fn connect_edge_settings_rmw("],
    ["mod", 655, 673, "connect_edge_settings_rmw(app, &|settings| {"],
    ["mod", 772, 772, "rm.clear_deferred_remote_close();"],
    [
      "mod",
      2282,
      2282,
      "fn kb261_connect_edge_settings_rmws_serialize_through_one_lock()",
    ],
    ["mod", 487, 487, "fn register_connection_with_identity("],
    ["mod", 487, 522, "fn register_connection_with_identity("],
    ["mod", 1939, 1939, "fn kb261_identities_are_exact_and_never_adopt()"],
    ["mod", 2007, 2050, "kb291_the_known_identity_machinery_is_gone"],
    ["mod", 638, 640, "no longer seeds a known-identity cache"],
    ["mod", 123, 133, "ConnectionRegistration"],
    ["server", 516, 527, "KB-291 (v1.6.6)"],
    ["server", 528, 528, "let (registration, rx) ="],
    ["server", 601, 601, "dropped before its Welcome"],
    ["server", 274, 274, "async fn finalize_disconnected_connection"],
    [
      "server",
      1670,
      1670,
      "fn welcome_send_failure_finalizes_the_lane_instead_of_leaking_it()",
    ],
    [
      "mod",
      1740,
      1740,
      "fn server_stop_leaves_records_for_their_pipelines_in_both_stop_orders()",
    ],
    ["mod", 1816, 1816, "fn stop_leaves_the_session_map_to_its_pipelines()"],
    [
      "mod",
      2445,
      2445,
      "fn kb270_lane_session_records_are_consumed_at_every_session_end()",
    ],
    [
      "server",
      1510,
      1510,
      "fn connection_blocking_seams_run_off_the_async_connection_task()",
    ],
    ["server", 252, 252, "fn spawn_worker("],
    ["server", 543, 642, "spawn_worker("],
    ["server", 806, 851, "KB-285 (v1.6.6)"],
    ["mod", 178, 188, "struct TrayResyncGate {"],
    ["mod", 362, 418, "fn claim_tray_resync("],
    ["mod", 314, 330, "pub fn note_device_connected("],
    ["mod", 350, 351, "pub fn note_device_disconnected("],
    [
      "mod",
      2062,
      2103,
      "fn kb292_n_rapid_edges_schedule_at_most_one_tray_rebuild_per_settled_state()",
    ],
    // Fleet, cycle 15 (KB-296 residual): stop()'s wholesale clear queues
    // a LaneGone per cleared lane, and the pin that holds it.
    ["mod", 846, 867, "coordinator.notify_lane_gone(lane);"],
    [
      "mod",
      3253,
      3295,
      "kb296_server_stop_queues_lane_gone_for_every_lane_it_clears",
    ],
    // Fleet, cycle 15: transcription_coordinator.rs, actions.rs, and
    // settings.rs citations join the live check. Rows below are ordered
    // by first appearance in the ledger.
    ["transcription_coordinator", 1181, 1206, "deferred_retire_lanes"],
    ["transcription_coordinator", 1213, 1219, "deferred.contains(&lane)"],
    [
      "transcription_coordinator",
      1226,
      1231,
      "connection_device(lane).is_none()",
    ],
    ["transcription_coordinator", 1232, 1237, "lane_retirement_ready"],
    [
      "transcription_coordinator",
      1334,
      1381,
      "exactly the cancelled lane's machine",
    ],
    ["transcription_coordinator", 1351, 1375, "retire_dead_lane("],
    [
      "transcription_coordinator",
      1449,
      1462,
      "Command::LaneGone { lane } => {",
    ],
    ["transcription_coordinator", 1709, 1726, "fn send_companion_edge("],
    ["transcription_coordinator", 1896, 1896, "fn route_input("],
    ["transcription_coordinator", 1950, 1968, "fn lane_retirement_ready"],
    [
      "transcription_coordinator",
      2504,
      2504,
      "fn start_success_probe_is_scoped_to_the_starting_session",
    ],
    [
      "transcription_coordinator",
      2527,
      2556,
      "the Start arm hands the effect's lane",
    ],
    [
      "transcription_coordinator",
      4558,
      4558,
      "fn a_lane_grace_expired_release_stops_with_the_lane_tagged()",
    ],
    [
      "transcription_coordinator",
      4607,
      4607,
      "fn expire_due_graces_tags_lane_stop_effects_before_run_effect()",
    ],
    ["actions", 71, 74, "unregister_in_flight_pipeline"],
    ["actions", 121, 138, "lane_pipeline_in_flight"],
    ["actions", 191, 191, "retire the shared recording surfaces"],
    ["actions", 334, 341, "preview_worth_waiting_for"],
    ["actions", 707, 711, "KB-239"],
    ["actions", 1312, 1318, "KB-019"],
    ["actions", 1464, 1474, "spawn_blocking"],
    ["actions", 1794, 1801, "history_retry"],
    ["actions", 1898, 1903, "process singleton"],
    ["actions", 1966, 1966, "on_lane_session_changed(app, lane, true)"],
    ["actions", 2305, 2306, "skipping persistence"],
    ["actions", 2305, 2321, "KB-254"],
    ["actions", 2569, 2571, "Failed to save history entry"],
    ["actions", 2820, 2822, "Failed to save failed history entry"],
    ["settings", 2081, 2085, "active_post_process_provider"],
    ["settings", 2193, 2226, "store.get"],
    ["settings", 2307, 2309, "whats_new_last_seen_version"],
    ["settings", 2432, 2437, "pub fn write_settings"],
    ["settings", 2143, 2181, "fn acquire_settings_store(app: &AppHandle)"],
    ["settings", 2187, 2190, "return get_default_settings();"],
    ["settings", 2437, 2441, "write_settings: settings NOT persisted"],
    ["actions", 2268, 2271, "catch_unwind(std::panic::AssertUnwindSafe"],
    ["actions", 2848, 2851, "retire_session_surfaces(&ah_for_panic, false);"],
  ];

  // 1. Every table row's token really sits in the cited span of the live
  //    source.
  for (const [kind, a, b, token] of anchorTable) {
    const lines = sources[kind];
    assert.ok(
      b <= lines.length,
      `the anchor ${citeLabel(kind)}:${a}-${b} points past the file (only ${lines.length} lines) - refresh the ledger`,
    );
    const span = lines.slice(a - 1, b).join("\n");
    assert.ok(
      span.includes(token),
      `${citeLabel(kind)}:${a}${a === b ? "" : `-${b}`} no longer carries "${token}" - the code moved; refresh the ledger citation`,
    );
  }

  // 2. Every citation in the ledger's five live-checked files is covered
  //    by a table row: no citation may exist that this pin does not
  //    live-check.
  const citationRe =
    /(?:companion\/(mod|server)|(transcription_coordinator|actions|settings))\.rs:(\d+)(?:-(\d+))?/g;
  const cited = new Set<string>();
  for (const m of goldmine.matchAll(citationRe)) {
    const kind = m[1] ?? m[2];
    const a = Number(m[3]);
    const b = m[4] ? Number(m[4]) : a;
    cited.add(`${kind}:${a}-${b}`);
  }
  const covered = new Set(anchorTable.map(([k, a, b]) => `${k}:${a}-${b}`));
  for (const key of cited) {
    assert.ok(
      covered.has(key),
      `the ledger cites ${citeLabel(key.split(":")[0])}:${key.split(":")[1]} but the anchor table does not live-check it - add the row (or refresh the citation)`,
    );
  }
  // And no dead table rows (a refreshed citation leaves its old row
  // behind silently otherwise).
  for (const key of covered) {
    assert.ok(
      cited.has(key),
      `the anchor table checks ${citeLabel(key.split(":")[0])}:${key.split(":")[1]} but the ledger no longer cites it - drop the row`,
    );
  }
}

// KB-287 (v1.6.5 residuals): the KB-277 fix (cdb14c51) rewrote the
// README's streaming-overlap sentence to state the singleton-engine
// truth, but nothing TESTED it - the fix was pinned only by this ledger's
// prose, so a docs revert of that one sentence would sail through CI.
// This content pin reads README.md itself: the companion section must
// keep the singleton-engine phrasing (the first session keeps streaming,
// a concurrently-starting session transcribes at stop, because the
// streaming engine is a singleton), and the pre-fix false claim (both
// sessions "record and transcribe at stop instead of streaming live
// text" - the exact wording cdb14c51 deleted) must not return. The code
// truth it mirrors: actions.rs decides stream ownership on the
// coordinator's serialized thread before any recorder runs - the first
// session to start claims the singleton (actions.rs:1880).
{
  const readme = readFileSync(join(docsDir, "..", "README.md"), "utf8");
  const companionSection = readme.slice(
    readme.indexOf("Each phone runs its own recording independently"),
    readme.indexOf("### Network and listener control"),
  );
  assert.ok(
    companionSection.length > 0,
    "README.md must keep the companion recording section this pin slices",
  );
  assert.ok(
    companionSection.includes("the first keeps streaming live text"),
    "KB-277/KB-287: the README must state that the FIRST session keeps " +
      "streaming when sessions overlap",
  );
  assert.ok(
    companionSection.includes("transcribes at stop"),
    "KB-277/KB-287: the README must state that a concurrently-started " +
      "session transcribes at stop",
  );
  assert.ok(
    companionSection.includes("streaming engine is a singleton"),
    "KB-277/KB-287: the README must name the singleton-engine reason",
  );
  assert.ok(
    !companionSection.includes(
      "both record and transcribe at stop instead of streaming live text",
    ),
    "KB-277/KB-287: the pre-fix sentence (no session streams during an " +
      "overlap) contradicts the singleton design cdb14c51 documented - a " +
      "revert of that README fix must fail here",
  );
}

console.log(
  "quality-goldmine ledger (AUD-15 + round 11 + round 12) tests passed",
);
