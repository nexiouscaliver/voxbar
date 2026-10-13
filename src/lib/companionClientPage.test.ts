import assert from "node:assert/strict";
import fs from "node:fs";
import nodePath from "node:path";
import { fileURLToPath } from "node:url";

// W1-L2 contract tests for the phone web client (the single embedded HTML
// asset the Rust companion server serves). The page has no framework and
// no bun:test module; like its siblings this is an assert-style script
// that pins the committed asset so the Rust-side injection and the
// embedded-strings pipeline keep working:
//
// 1. The STRINGS placeholder stays byte-stable (the server swaps the null
//    for the generated locale blob by exact match, client_page.rs).
// 2. No em or en dash anywhere in the page (the operator's voice rules).
// 3. Every t("key", ...) key the page references exists in the en
//    locale's companion block AND in the generated strings_gen.rs, so the
//    page and the embedded blob cannot drift apart (regenerate with
//    `bun run gen:companion-strings` after locale edits).
// 4. The v1.6.0 client contract: gesture-scoped audio bootstrap, session
//    preferences on every Ptt edge with localStorage persistence, the
//    transcript/recentHistory frame handlers, and the mic retry path.

const __dirname = nodePath.dirname(fileURLToPath(import.meta.url));
const repoRoot = nodePath.join(__dirname, "..", "..");
const clientPath = nodePath.join(
  repoRoot,
  "src-tauri",
  "src",
  "companion",
  "client",
  "index.html",
);
const stringsGenPath = nodePath.join(
  repoRoot,
  "src-tauri",
  "src",
  "companion",
  "strings_gen.rs",
);
const enPath = nodePath.join(
  repoRoot,
  "src",
  "i18n",
  "locales",
  "en",
  "translation.json",
);

const html = fs.readFileSync(clientPath, "utf8");
const stringsGen = fs.readFileSync(stringsGenPath, "utf8");
const enCompanion = JSON.parse(fs.readFileSync(enPath, "utf8")).companion;

// 1. The injection marker contract.
const MARKER = "const STRINGS = /*__COMPANION_STRINGS__*/null;";
assert.ok(
  html.includes(MARKER),
  "the STRINGS placeholder must stay byte-stable for the Rust injection",
);
assert.strictEqual(
  html.indexOf("/*__COMPANION_STRINGS__*/"),
  html.lastIndexOf("/*__COMPANION_STRINGS__*/"),
  "the marker appears exactly once",
);

// 2. The page ships no em or en dash (check-text-style covers the locale
// files; this covers the page's own strings, code, and comments).
for (const [label, ch] of [
  ["em dash", "\u2014"],
  ["en dash", "\u2013"],
] as const) {
  assert.ok(!html.includes(ch), `the client page must not contain an ${label}`);
}

// 3. Key coverage across the page, the en locale, and the generated blob.
const usedKeys = new Set<string>();
for (const m of html.matchAll(/\bt\(\s*"([a-zA-Z]+)"/g)) {
  usedKeys.add(m[1]);
}
assert.ok(
  usedKeys.size >= 20,
  `expected a real key set, found ${usedKeys.size}`,
);
for (const key of usedKeys) {
  assert.ok(
    typeof enCompanion[key] === "string" && enCompanion[key].length > 0,
    `key '${key}' used by the page is missing from en companion.*`,
  );
  assert.ok(
    stringsGen.includes(`"${key}":`),
    `key '${key}' is missing from strings_gen.rs; run: bun run gen:companion-strings`,
  );
}
// The v1.6.0 additions specifically.
for (const key of [
  "targetLabel",
  "targetPc",
  "targetPhone",
  "postProcessLabel",
  "resultTitle",
  "copy",
  "copied",
  "polished",
  "historyTitle",
  "historyEmpty",
  "micRetry",
  "zeroAudio",
]) {
  assert.ok(usedKeys.has(key), `the page must render the new key '${key}'`);
}

// 4. The v1.6.0 client behavior pins (source-level, mirroring the Rust
// companion::client_page tests from the phone's own test side).

// The press gesture owns the audio bootstrap.
const pressStart = sliceBetween(
  html,
  "function pressStart",
  "function pressEnd",
);
assert.ok(pressStart.includes("ensureAudio();"), "pressStart bootstraps audio");
assert.ok(
  pressStart.indexOf("ensureAudio();") < pressStart.indexOf("sendPtt(true);"),
  "the audio bootstrap runs inside the gesture, before the edge",
);

// The welcome handler never touches the audio context (code identifiers,
// not comments: a context created off-gesture starts suspended).
const welcome = sliceBetween(html, 'case "welcome":', "break;");
assert.ok(!welcome.includes("audioCtx"));
assert.ok(!welcome.includes("new Ctx("));
assert.ok(!welcome.includes("setupGraph"));
assert.ok(welcome.includes("openMic();"), "welcome warms only the mic stream");

// Preferences ride every edge and persist.
const sendPtt = sliceBetween(html, "function sendPtt", "ptt.addEventListener");
assert.ok(sendPtt.includes('type: "ptt"'));
assert.ok(sendPtt.includes("target: outputTarget"));
assert.ok(sendPtt.includes("postProcess: postProcess"));
assert.ok(html.includes("localStorage.getItem(TARGET_KEY)"));
assert.ok(html.includes("localStorage.setItem(TARGET_KEY, outputTarget)"));
assert.ok(html.includes("localStorage.getItem(PP_KEY)"));
assert.ok(html.includes("localStorage.setItem(PP_KEY,"));

// v1.6.2 fleet pass 1: the hello carries a persistent device id. The
// UA-derived device name is a platform token every same-platform phone
// shares (two iPhones both say "iPhone", every Chrome-Android phone says
// "Linux"), so per-phone data keyed on it alone collapsed same-platform
// phones into one Recent list - private phone-destined dictations
// included. The id is persisted like the prefs and rides every hello.
assert.ok(
  html.includes('const DEVICE_ID_KEY = "voxbarCompanion.deviceId";'),
  "the device id has a stable storage key",
);
const deviceIdentity = sliceBetween(
  html,
  "function deviceIdentity",
  "function loadPrefs",
);
assert.ok(
  deviceIdentity.includes("localStorage.getItem(DEVICE_ID_KEY)"),
  "the device id is read from storage",
);
assert.ok(
  deviceIdentity.includes("localStorage.setItem(DEVICE_ID_KEY, id)"),
  "a fresh device id is persisted for the next visit",
);
const helloSend = sliceBetween(html, 'type: "hello",', "}),");
assert.ok(
  helloSend.includes("deviceId: deviceIdentity(),"),
  "the hello must carry the persistent device id alongside the platform name",
);

// KB-260 (v1.6.4): the no-crypto fallback id must be high-entropy in the
// digits the server's identity tag READS. The tag is the id's first four
// alphanumeric characters (protocol.rs device_identity_name), and the old
// fallback led with Date.now(): two no-crypto phones pairing in the same
// hour minted timestamp-leading ids whose tags were byte-identical, so the
// same-platform twins collapsed into one Recent list. The fallback now
// leads with Math.random, and this pin EXECUTES the committed fallback
// expression (frozen clock, scripted randomness) and applies the server's
// exact tag rule, so a timestamp-leading regression fails here.
{
  const fallback = sliceBetween(
    html,
    "function fallbackDeviceId",
    "function deviceIdentity",
  );
  const exprMatch = fallback.match(/return \(([\s\S]+?)\);/);
  assert.ok(
    exprMatch,
    "the fallback must keep a single return (...) expression for this pin",
  );
  const expr = exprMatch[1];
  const serverTag = (id: string) =>
    (id.match(/[a-zA-Z0-9]/g) || []).slice(0, 4).join("");
  const makeId = (randomSequence: number[], nowMs: number) =>
    new Function("Math", "Date", `"use strict"; return (${expr});`)(
      {
        random: () => randomSequence.shift() ?? 0,
      } as unknown as Math,
      { now: () => nowMs } as unknown as DateConstructor,
    ) as string;

  // Two no-crypto phones pairing in the SAME millisecond (the collision
  // window the old timestamp-leading ids shared for hours): distinct
  // randomness must yield distinct tags through the server's rule.
  const nowMs = 1760000000000;
  const tags = new Set<string>();
  const DRAWS = 64;
  for (let i = 0; i < DRAWS; i++) {
    const id = makeId([0.01 + i * 0.013, 0.4], nowMs);
    const tag = serverTag(id);
    assert.ok(
      tag.length === 4,
      `the tag must hold four alphanumerics (id ${id})`,
    );
    tags.add(tag);
  }
  assert.strictEqual(
    tags.size,
    DRAWS,
    "64 same-instant no-crypto phones must yield 64 distinct identity tags",
  );

  // The tag must depend on the RANDOM half, not the clock: the same
  // randomness at different instants keeps one identity, and different
  // randomness at one instant differs.
  assert.strictEqual(
    serverTag(makeId([0.37, 0.5], nowMs)),
    serverTag(makeId([0.37, 0.5], nowMs + 60_000)),
    "same randomness keeps the tag stable across instants",
  );
  assert.notEqual(
    serverTag(makeId([0.37, 0.5], nowMs)),
    serverTag(makeId([0.91, 0.5], nowMs)),
    "different randomness at the same instant must differ",
  );
}

// KB-262 (v1.6.4): the zero-audio recovery advice must match the
// activation the Mac applies - "Hold the button" is wrong advice under the
// default tap-to-lock, where the button taps to start and taps again to
// stop (the same mirror of the talk button label the idle hint follows).
// The page routes the notice through zeroAudioHint(), which picks the
// zeroAudioHold / zeroAudioHoldOrToggle / zeroAudioTap key from the
// welcome frame's activation, so three pins hold:
{
  const hint = sliceBetween(html, "function zeroAudioHint", "function vibrate");
  const toggleAt = hint.indexOf('activation === "toggle"');
  const holdOrToggleAt = hint.indexOf('activation === "hold_or_toggle"');
  assert.ok(
    toggleAt >= 0 && hint.indexOf("zeroAudioTap", toggleAt) >= 0,
    "KB-262: toggle mode must advise with the zeroAudioTap copy",
  );
  assert.ok(
    holdOrToggleAt >= 0 &&
      hint.indexOf("zeroAudioHoldOrToggle", holdOrToggleAt) >= 0,
    "KB-262: hold_or_toggle mode must advise with the zeroAudioHoldOrToggle copy",
  );
  assert.ok(
    hint.includes('return t("zeroAudioHold",'),
    "KB-262: push_to_talk (and any unknown activation) must fall back to the zeroAudioHold copy",
  );

  // The notice itself must route through the hint, never the neutral key.
  const zeroAudioNotice = sliceBetween(
    html,
    'msg.code === "companion_zero_audio"',
    "break;",
  );
  assert.ok(
    zeroAudioNotice.includes("showNotice(zeroAudioHint())"),
    "KB-262: the companion_zero_audio notice must render zeroAudioHint()",
  );

  // Every shipped locale carries the three activation variants, so the
  // advice localizes everywhere (strings_gen parity is pinned above).
  const localesDir = nodePath.join(repoRoot, "src", "i18n", "locales");
  const shipped = fs
    .readdirSync(localesDir)
    .filter((name) =>
      fs.existsSync(nodePath.join(localesDir, name, "translation.json")),
    );
  assert.ok(
    shipped.length >= 26,
    `expected the full locale set, found ${shipped.length}`,
  );
  for (const locale of shipped) {
    const companion = JSON.parse(
      fs.readFileSync(
        nodePath.join(localesDir, locale, "translation.json"),
        "utf8",
      ),
    ).companion;
    for (const key of [
      "zeroAudioHold",
      "zeroAudioHoldOrToggle",
      "zeroAudioTap",
    ]) {
      assert.ok(
        typeof companion[key] === "string" && companion[key].length > 0,
        `KB-262: ${locale} must ship companion.${key} for its activation`,
      );
    }
  }

  // The English tap advice must not tell a tap-mode user to hold (the
  // exact activation-blind wording the finding quotes).
  assert.ok(
    !enCompanion.zeroAudioTap.includes("Hold"),
    "KB-262: en zeroAudioTap must advise tapping, not holding",
  );
  assert.ok(
    enCompanion.zeroAudioHold.includes("Hold"),
    "KB-262: en zeroAudioHold must advise holding",
  );
}

// KB-258 (v1.6.5): the Polish-text switch only arms the NEXT dictation -
// per-lane prefs freeze at session start (freeze_session), while the
// keyboard pp hotkey flips the live session - so the toggle's label must
// carry a hint saying when the flip lands. The scoped fix is the label;
// folding mid-session pref updates is the bigger change. Three pins: the
// hint element renders next to the toggle, the page feeds it the
// postProcessHint key, and every locale ships that companion string.
{
  assert.ok(
    html.includes('id="t-pp-hint"'),
    "KB-258: the Polish toggle row must render the hint element",
  );
  const ppRow = sliceBetween(html, 'id="t-pp-label"', "</div>");
  assert.ok(
    ppRow.includes('id="t-pp-hint"') && ppRow.includes('id="ppToggle"'),
    "KB-258: the hint must sit inside the Polish toggle's own row",
  );
  const hintInit = sliceBetween(
    html,
    'document.getElementById("t-pp-hint")',
    ");",
  );
  assert.ok(
    hintInit.includes('t(\n        "postProcessHint",') ||
      hintInit.includes('t("postProcessHint",'),
    "KB-258: the hint must render the postProcessHint companion string",
  );
  assert.ok(
    usedKeys.has("postProcessHint"),
    "KB-258: the page must reference the postProcessHint key",
  );
  const localesDir = nodePath.join(repoRoot, "src", "i18n", "locales");
  const shipped = fs.readdirSync(localesDir);
  for (const locale of shipped) {
    const companion = JSON.parse(
      fs.readFileSync(
        nodePath.join(localesDir, locale, "translation.json"),
        "utf8",
      ),
    ).companion;
    assert.ok(
      typeof companion.postProcessHint === "string" &&
        companion.postProcessHint.length > 0,
      `KB-258: ${locale} must ship companion.postProcessHint`,
    );
  }
}

// The new server frames render.
const transcriptCase = sliceBetween(html, 'case "transcript":', "break;");
assert.ok(transcriptCase.includes("showTranscript("));
const recentCase = sliceBetween(html, 'case "recentHistory":', "break;");
assert.ok(recentCase.includes("RECENT_LIMIT"));
assert.ok(
  /const RECENT_LIMIT = 5;/.test(html),
  "the recent list is five entries",
);
assert.ok(html.includes("navigator.clipboard.writeText"));

// Mic failure: inline error with retry, never a dead button.
assert.ok(html.includes('id="micError"'));
assert.ok(html.includes('id="micRetry"'));
const openMic = sliceBetween(
  html,
  "function openMic",
  "micRetryBtn.addEventListener",
);
assert.ok(openMic.includes("showMicError();"));
assert.ok(!openMic.includes("ptt.disabled = true"));

function sliceBetween(source: string, start: string, end: string): string {
  const at = source.indexOf(start);
  assert.ok(at >= 0, `anchor '${start}' missing from the client page`);
  const until = source.indexOf(end, at);
  assert.ok(
    until > at,
    `closing anchor '${end}' missing after '${start}' in the client page`,
  );
  return source.slice(at, until);
}

console.log(
  `companionClientPage: ${usedKeys.size} string keys verified, v1.6.0 client contract pinned`,
);
