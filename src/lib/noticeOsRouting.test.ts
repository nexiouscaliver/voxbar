import assert from "node:assert/strict";

// v1.6.0 W1-L4 contract tests: the persistent OS-notification system.
//
// Four pinned contracts, one per brief:
// 1. Routing classification exhaustiveness (the companion/mod.rs
//    phone_visibility pattern): every NoticeCode has exactly one explicit
//    overlay-only / notification / both decision in NOTICE_CHANNEL_TABLE,
//    cross-pinned against NOTICE_ROUTE_POLICIES and the notice universe, so
//    a new Rust notice code FAILS this test until it joins a group here.
// 2. Click-through: the classification's target table is total (every
//    notifying code navigates somewhere known; overlay-only codes never
//    do), the click bus delivers the extra payload to subscribers
//    (noticeNotificationSurface.test.ts), and the App leg's navigation
//    seam noticeClickSection maps every click, degenerate ones included,
//    onto a section.
// 3. Toggle-off parity: with the master toggle off, planNoticeDispatch
//    degrades to EXACTLY the pre-v1.6.0 dispatch for every code and card
//    state (toast + hidden-window offer per routerDecision alone, no
//    persistent post, no suppression) AND no log write: the emit path in
//    App.tsx writes the log purely off plan.log, so the off path adds no
//    durable state the pre-v1.6.0 app did not write.
// 4. Log cap: the notification log keeps the last 50 entries, newest
//    first, and clears completely.

// ---------------------------------------------------------------------------
// Part 0: module contracts
// ---------------------------------------------------------------------------

interface ChannelEntry {
  channel: string;
  group?: string;
  target?: string;
}

interface RoutingModule {
  NOTICE_ROUTE_POLICIES: Record<string, string>;
  NOTICE_CHANNEL_TABLE: Record<string, ChannelEntry>;
  noticeChannelClass(code: string): string;
  noticeClickTarget(code: string): string | null;
  noticeClickSection(click: { code?: string; target?: string }): string;
  osNoticeDecision(
    code: string,
    prefs: { enabled: boolean; outcomes: boolean },
  ): { persistent: boolean };
  planNoticeDispatch(
    code: string,
    cardVisible: boolean,
    prefs: { enabled: boolean; outcomes: boolean },
  ): {
    toast: boolean;
    persistentOs: boolean;
    legacyHiddenOsOffer: boolean;
    log: boolean;
  };
  DEFAULT_OS_NOTIFICATION_PREFS: { enabled: boolean; outcomes: boolean };
  routerDecision(
    code: string,
    cardVisible: boolean,
  ): {
    toast: boolean;
    notify: boolean;
  };
}

let routing: RoutingModule | null = null;
let loadError = "";
try {
  routing = (await import("./noticeRouting")) as unknown as RoutingModule;
} catch (error) {
  loadError = error instanceof Error ? error.message : String(error);
}
if (routing === null) {
  assert.fail(
    "src/lib/noticeRouting.ts must exist and export the v1.6.0 additions " +
      "NOTICE_CHANNEL_TABLE, noticeChannelClass, noticeClickTarget, " +
      "osNoticeDecision, planNoticeDispatch, DEFAULT_OS_NOTIFICATION_PREFS " +
      "(import failed: " +
      loadError +
      ")",
  );
}
const {
  NOTICE_ROUTE_POLICIES,
  NOTICE_CHANNEL_TABLE,
  noticeChannelClass,
  noticeClickTarget,
  noticeClickSection,
  osNoticeDecision,
  planNoticeDispatch,
  DEFAULT_OS_NOTIFICATION_PREFS,
  routerDecision,
} = routing;

assert.deepEqual(
  DEFAULT_OS_NOTIFICATION_PREFS,
  { enabled: true, outcomes: true },
  "the persistent OS-notification surface defaults ON with outcomes included",
);

// ---------------------------------------------------------------------------
// Part 1: classification exhaustiveness (the phone_visibility pattern)
//
// One classification list per group below; the union must equal the notice
// universe exactly (mirrors ALL_NOTICE_CODES in noticeRouting.test.ts, the
// NoticeCode::as_str universe), and NOTICE_CHANNEL_TABLE must carry exactly
// those keys - a new Rust notice code fails here until it is deliberately
// classified.
// ---------------------------------------------------------------------------

const ALL_NOTICE_CODES = [
  "no_model_selected",
  "microphone_permission_denied",
  "no_input_device",
  "recording_failed",
  "transcription_failed",
  "paste_failed",
  "model_load_failed",
  "model_load_failed_memory_gate",
  "model_fallback",
  "post_process_memory_gate",
  "post_process_download_missing",
  "post_process_engine_failed",
  "post_process_timeout",
  "post_process_length_guard",
  "post_process_too_long",
  "post_process_output_invalid",
  "post_process_cloud_failed",
  "post_process_not_configured",
  "post_process_unchanged",
  "delete_last_word_no_session",
  "delete_last_word_no_buffer",
  "undo_no_session",
  "undo_no_buffer",
  "binding_busy",
  "wayland_tauri_hotkeys",
  "gnome_overlay_fallback",
  "post_process_prompt_cycled",
  "post_process_session_upgraded",
  "post_process_session_downgraded",
  "companion_disconnected_finalized",
  "companion_server_failed",
  "companion_session_capped",
  "companion_zero_audio",
  "command_mode_no_session",
] as const;

// Error group: notify whenever the master toggle is on; clicks land on the
// in-app notifications log. companion_zero_audio deliberately targets the
// log (not history) because a zero-audio session leaves NO history entry -
// the log row is the only durable record of what happened.
const ERROR_GROUP = [
  "no_model_selected",
  "microphone_permission_denied",
  "no_input_device",
  "recording_failed",
  "transcription_failed",
  "paste_failed",
  "model_load_failed",
  "model_load_failed_memory_gate",
  "post_process_download_missing",
  "post_process_memory_gate",
  "post_process_engine_failed",
  "post_process_timeout",
  "post_process_length_guard",
  "post_process_output_invalid",
  "post_process_cloud_failed",
  // Round 7: a requested-but-unusable polish is a persistent
  // misconfiguration in the download_missing class.
  "post_process_not_configured",
  "companion_server_failed",
  "companion_zero_audio",
] as const;

// Outcome group: final companion-session results; the finalized transcript
// lives in history, so clicks navigate there.
const OUTCOME_GROUP = [
  "companion_session_capped",
  "companion_disconnected_finalized",
] as const;

// Overlay-only: transient per-dictation states and recurring advisories.
// Each keeps exactly the transient surfaces its NOTICE_ROUTE_POLICIES entry
// already gives it; the persistent surface never fires.
const OVERLAY_ONLY = [
  "model_fallback",
  "post_process_too_long",
  // Round 7: the engine ran and changed nothing - transient per-dictation
  // state; the run record is the durable surface.
  "post_process_unchanged",
  "delete_last_word_no_session",
  "delete_last_word_no_buffer",
  "undo_no_session",
  "undo_no_buffer",
  "binding_busy",
  "post_process_prompt_cycled",
  // v1.6.0 W1-L7: transient mid-session mode toggles; the badge carries
  // the durable state, nothing to persist.
  "post_process_session_upgraded",
  "post_process_session_downgraded",
  "wayland_tauri_hotkeys",
  "gnome_overlay_fallback",
  "command_mode_no_session",
] as const;

// Test self-check: the groups partition the notice universe exactly.
const grouped = [...ERROR_GROUP, ...OUTCOME_GROUP, ...OVERLAY_ONLY];
assert.deepEqual(
  [...grouped].sort(),
  [...ALL_NOTICE_CODES].sort(),
  "test self-check: the classification groups must cover the whole notice universe",
);
assert.equal(
  new Set(grouped).size,
  ALL_NOTICE_CODES.length,
  "test self-check: no notice code may sit in two classification groups",
);

// The exhaustive pin: NOTICE_CHANNEL_TABLE carries exactly one entry per
// notice code. A new Rust notice code (a new NOTICE_ROUTE_POLICIES arm, a
// new noticeMessage arm) fails BOTH equality checks until it is classified.
assert.deepEqual(
  Object.keys(NOTICE_CHANNEL_TABLE).sort(),
  [...ALL_NOTICE_CODES].sort(),
  "NOTICE_CHANNEL_TABLE must classify exactly the notice universe - a new notice code needs an explicit overlay-only/notification/both decision",
);
assert.deepEqual(
  Object.keys(NOTICE_CHANNEL_TABLE).sort(),
  Object.keys(NOTICE_ROUTE_POLICIES).sort(),
  "NOTICE_CHANNEL_TABLE and NOTICE_ROUTE_POLICIES must cover the same codes - the two hand tables cannot drift",
);

// Entry shape: channel is one of the three declared classes; entries carry
// group/target if and only if the channel may notify.
const CHANNEL_CLASSES = ["overlay-only", "notification", "both"];
for (const [code, entry] of Object.entries(NOTICE_CHANNEL_TABLE)) {
  assert.ok(
    CHANNEL_CLASSES.includes(entry.channel),
    `${code}: channel must be one of ${CHANNEL_CLASSES.join("/")}, got ${entry.channel}`,
  );
  if (entry.channel === "overlay-only") {
    assert.equal(
      entry.group,
      undefined,
      `${code}: an overlay-only code must not carry a notification group`,
    );
    assert.equal(
      entry.target,
      undefined,
      `${code}: an overlay-only code must not carry a click target`,
    );
  } else {
    assert.ok(
      entry.group === "error" || entry.group === "outcome",
      `${code}: a notifying code must carry group error|outcome`,
    );
    assert.ok(
      entry.target === "notifications" || entry.target === "history",
      `${code}: a notifying code must carry target notifications|history`,
    );
  }
}

// The declared groups agree with the table entries.
for (const code of ERROR_GROUP) {
  assert.equal(NOTICE_CHANNEL_TABLE[code].group, "error", `${code} group`);
  assert.equal(
    NOTICE_CHANNEL_TABLE[code].target,
    "notifications",
    `${code} click target`,
  );
}
for (const code of OUTCOME_GROUP) {
  assert.equal(NOTICE_CHANNEL_TABLE[code].group, "outcome", `${code} group`);
  assert.equal(
    NOTICE_CHANNEL_TABLE[code].target,
    "history",
    `${code} click target`,
  );
}
for (const code of OVERLAY_ONLY) {
  assert.equal(
    NOTICE_CHANNEL_TABLE[code].channel,
    "overlay-only",
    `${code} must stay overlay-only`,
  );
}

// noticeChannelClass falls back to overlay-only for unknown/empty codes
// (the same silent-skip discipline as routerDecision).
assert.equal(noticeChannelClass("something_unmapped"), "overlay-only");
assert.equal(noticeChannelClass(""), "overlay-only");

// ---------------------------------------------------------------------------
// Part 2: runtime agreement (classification vs decision functions)
//
// Every code, every card state, every prefs combination: the runtime
// decision must agree with the explicit classification. This is the loop
// half of the phone_visibility pattern - the table cannot say one thing
// while the functions do another.
// ---------------------------------------------------------------------------

const PREFS_MATRIX = [
  { enabled: true, outcomes: true },
  { enabled: true, outcomes: false },
  { enabled: false, outcomes: true },
  { enabled: false, outcomes: false },
] as const;

const CARD_STATES: readonly boolean[] = [true, false];

for (const code of ALL_NOTICE_CODES) {
  const entry = NOTICE_CHANNEL_TABLE[code];
  for (const prefs of PREFS_MATRIX) {
    const expectedPersistent =
      entry.channel === "overlay-only"
        ? false
        : entry.group === "outcome"
          ? prefs.enabled && prefs.outcomes
          : prefs.enabled;
    assert.equal(
      osNoticeDecision(code, prefs).persistent,
      expectedPersistent,
      `${code} persistent decision under ${JSON.stringify(prefs)}`,
    );
    for (const cardVisible of CARD_STATES) {
      const plan = planNoticeDispatch(code, cardVisible, prefs);
      assert.equal(
        plan.persistentOs,
        expectedPersistent,
        `${code} plan persistentOs (cardVisible=${cardVisible}) under ${JSON.stringify(prefs)}`,
      );
      // The two OS surfaces never double-post: the persistent notification
      // replaces the legacy hidden-window offer exactly when it fires.
      assert.equal(
        plan.legacyHiddenOsOffer,
        routerDecision(code, cardVisible).notify && !expectedPersistent,
        `${code} legacy offer is today's offer, suppressed only by a persistent post`,
      );
      // The toast leg is untouched by the new surface in every state.
      assert.equal(
        plan.toast,
        routerDecision(code, cardVisible).toast,
        `${code} toast leg unchanged by the OS classification`,
      );
      // The log leg is gated on the master toggle alone (not the outcomes
      // sub-toggle, not the card state, not whether the OS post fired).
      assert.equal(
        plan.log,
        prefs.enabled && entry.channel !== "overlay-only",
        `${code} log leg (cardVisible=${cardVisible}) under ${JSON.stringify(prefs)}`,
      );
    }
  }
}

// Unknown codes plan inert under every prefs combination.
for (const prefs of PREFS_MATRIX) {
  for (const cardVisible of CARD_STATES) {
    const plan = planNoticeDispatch("something_unmapped", cardVisible, prefs);
    assert.deepEqual(
      plan,
      {
        toast: false,
        persistentOs: false,
        legacyHiddenOsOffer: false,
        log: false,
      },
      "unknown codes stay inert on every surface",
    );
  }
}

// ---------------------------------------------------------------------------
// Part 3: toggle-off parity
//
// The off path must restore today's behavior EXACTLY: for every code and
// card state, the plan is the pre-v1.6.0 dispatch (routerDecision alone)
// with persistentOs always false, no suppression of the legacy offer, and
// NO log write - the log is new durable state v1.6.0 must not add while
// the notification system is disabled, so the emit path (App.tsx writes
// the log purely off plan.log) is covered by this same assertion.
// ---------------------------------------------------------------------------

for (const code of ALL_NOTICE_CODES) {
  for (const cardVisible of CARD_STATES) {
    const legacy = routerDecision(code, cardVisible);
    for (const outcomes of [true, false]) {
      const plan = planNoticeDispatch(code, cardVisible, {
        enabled: false,
        outcomes,
      });
      assert.deepEqual(
        plan,
        {
          toast: legacy.toast,
          persistentOs: false,
          legacyHiddenOsOffer: legacy.notify,
          log: false,
        },
        `toggle OFF must restore the exact pre-v1.6.0 dispatch for ${code} (cardVisible=${cardVisible}, outcomes=${outcomes})`,
      );
    }
  }
}

// The outcomes sub-toggle only ever widens or narrows outcome-group codes:
// error-group codes are immune to it (errors-only mode still notifies them).
for (const code of ERROR_GROUP) {
  assert.equal(
    osNoticeDecision(code, { enabled: true, outcomes: false }).persistent,
    true,
    `${code} (error group) must notify in errors-only mode`,
  );
}
for (const code of OUTCOME_GROUP) {
  assert.equal(
    osNoticeDecision(code, { enabled: true, outcomes: false }).persistent,
    false,
    `${code} (outcome group) must go quiet in errors-only mode`,
  );
}
// Errors-only mode still LOGS outcome codes: the sub-toggle governs the OS
// surface only; the in-app log keeps the full error+outcome record while
// the master toggle is on.
for (const code of [...ERROR_GROUP, ...OUTCOME_GROUP]) {
  assert.equal(
    planNoticeDispatch(code, false, { enabled: true, outcomes: false }).log,
    true,
    `${code} must still be logged in errors-only mode while the master toggle is on`,
  );
}

// ---------------------------------------------------------------------------
// Part 4: click-through mapping
//
// Target totality: every code that can post a persistent notification
// navigates somewhere known; overlay-only and unknown codes never do.
// noticeClickSection is the App-level navigation seam (App.tsx feeds it
// whatever the click bus delivered and setCurrentSection's its result), so
// its totality is what makes the App leg regression-safe: garbage or
// missing targets can never strand a click or crash the handler.
// ---------------------------------------------------------------------------

for (const code of [...ERROR_GROUP, ...OUTCOME_GROUP]) {
  const target = noticeClickTarget(code);
  assert.ok(
    target === "notifications" || target === "history",
    `${code} must have a concrete click target`,
  );
}
for (const code of OVERLAY_ONLY) {
  assert.equal(
    noticeClickTarget(code),
    null,
    `${code} never posts a persistent notification, so it has no click target`,
  );
}
assert.equal(noticeClickTarget("something_unmapped"), null);
assert.equal(noticeClickTarget(""), null);

// The App leg's navigation decision, end to end over the real table: the
// section a click lands on always equals the classified target.
for (const code of [...ERROR_GROUP, ...OUTCOME_GROUP]) {
  assert.equal(
    noticeClickSection({ target: noticeClickTarget(code) ?? undefined }),
    NOTICE_CHANNEL_TABLE[code].target,
    `${code} click must navigate to its classified target section`,
  );
}
// Degenerate clicks fall back to the log section, never a throw.
assert.equal(noticeClickSection({}), "notifications");
assert.equal(noticeClickSection({ code: "x" }), "notifications");
assert.equal(noticeClickSection({ target: "garbage" }), "notifications");
assert.equal(noticeClickSection({ target: "history" }), "history");

console.log("noticeOsRouting classification tests passed");
