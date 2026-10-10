import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { noticeMessage } from "./noticeMessage";

// AUD-05: the companion session-cap notice drops the device name and 8
// locales render the companion device two ways inside one notice block.
//
// The Rust side emits the cap event WITH the device name as the notice
// detail (companion/server.rs:411-415 passes Some(device_name.clone());
// managers/transcription.rs:463-467 documents "Detail carries the device
// name"), but noticeMessage's companion_session_capped arm calls
// t("overlay.notice.companionSessionCapped") with no options
// (noticeMessage.ts:93), and no locale's copy has a {{device}} slot, so
// the name never reaches the user — the KB-163 complaint extended.
//
// Separately, the capped string in hi/zh-TW/ru/da/ne/pt/sv coins a second
// ad-hoc rendering of "companion" instead of the term that file already
// established in companionBadge / companionDevices.label / companion.title
// (hi कंपेनियन vs कॉम्पैनियन; zh-TW 隨附裝置 vs 伴侶; ru сопутствующего
// устройства vs -спутник; da companion-enheden vs Følge-; ne कम्प्यानियन vs
// साथी; pt dispositivo companion vs companheiro; sv companionenheten vs
// Kompanion-). And ja ships the file's only mid-string colon without a
// following space (しました:15分の).
//
// Contract for the fix, pinned by the assertions below:
// 1. Every locale's overlay.notice.companionSessionCapped contains a
//    {{device}} interpolation slot (i18next style, like {{error}}/{{name}}
//    in the neighboring keys) and still names the companion with the
//    file's ESTABLISHED term from the table below (case-insensitive, so
//    label-case Følge/Kompanion and sentence-case følge/kompanion both
//    count; CJK/Indic terms have no case).
// 2. The English copy reads exactly "Companion dictation ended: the
//    15-minute session limit was reached on {{device}}." and no locale
//    uses an em or en dash.
// 3. The ja value has no ASCII ':' that is not followed by a space.
// 4. noticeMessage's companion_session_capped arm passes the event detail
//    through as the device option, falling back to "" like its siblings:
//    t("overlay.notice.companionSessionCapped", { device: detail ?? "" }).

const here = path.dirname(fileURLToPath(import.meta.url));
const localesDir = path.join(here, "../i18n/locales");

// Established companion term per locale. The seven AUD-05 drift locales
// come from the audit evidence; every other entry was verified in this
// repo against the file's own companionBadge / companionDevices.label /
// companion.title (e.g. da "Følgeomikrofon"/"Følgeenheder", ru
// "Микрофон-спутник"/"Устройства-спутники", ne "साथी माइक्रोफोन"). Stems
// are chosen to survive inflection (cs doprovodný→doprovodného,
// pl towarzyszący→towarzyszącego, uk супутник→супутника).
const ESTABLISHED_COMPANION_TERM: Record<string, string> = {
  ar: "مرافق",
  bg: "придружаващ",
  ca: "acompanyant",
  cs: "doprovodn",
  da: "følge",
  de: "begleit",
  en: "companion",
  es: "acompañante",
  fr: "compagnon",
  he: "מלווה",
  hi: "कॉम्पैनियन",
  id: "pendamping",
  it: "companion",
  ja: "コンパニオン",
  ko: "컴패니언",
  ne: "साथी",
  nl: "companion",
  pl: "towarzysząc",
  pt: "companheiro",
  ru: "спутник",
  sv: "kompanion",
  tr: "eşlik",
  uk: "супутник",
  vi: "đồng hành",
  zh: "伴侣",
  "zh-TW": "伴侶",
};

// Guard: the hard-coded table must cover exactly the locale directories on
// disk, so a locale added later cannot silently skip the term check (and
// the loop below can never vacuously pass).
const onDisk = fs.readdirSync(localesDir).sort();
assert.deepEqual(
  onDisk,
  Object.keys(ESTABLISHED_COMPANION_TERM).sort(),
  "the established-companion-term table must cover exactly the locales in src/i18n/locales",
);

// i18next-style dotted-key lookup against a parsed locale object — the
// same resolution t("overlay.notice.companionSessionCapped") performs.
// JSON.parse also fails the test outright if a locale file is invalid JSON.
const resolveKey = (root: unknown, key: string): string | undefined => {
  let node: unknown = root;
  for (const part of key.split(".")) {
    if (node == null || typeof node !== "object") return undefined;
    node = (node as Record<string, unknown>)[part];
  }
  return typeof node === "string" ? node : undefined;
};

const readLocale = (locale: string): unknown =>
  JSON.parse(
    fs.readFileSync(path.join(localesDir, locale, "translation.json"), "utf8"),
  );

// 1-3: every locale's capped copy carries the device slot, keeps the
// established companion term, and (ja) spaces its colons.
for (const [locale, term] of Object.entries(ESTABLISHED_COMPANION_TERM)) {
  const value = resolveKey(
    readLocale(locale),
    "overlay.notice.companionSessionCapped",
  );
  assert.ok(
    typeof value === "string" && value.length > 0,
    `${locale}: overlay.notice.companionSessionCapped must exist as a non-empty string`,
  );
  const copy = value as string;

  assert.ok(
    copy.includes("{{device}}"),
    `${locale}: overlay.notice.companionSessionCapped must carry a {{device}} interpolation slot — the Rust cap event sends the device name as the notice detail (server.rs:411-415) but this copy has nowhere to put it`,
  );

  assert.ok(
    copy.toLowerCase().includes(term.toLowerCase()),
    `${locale}: overlay.notice.companionSessionCapped must name the companion with the file's established term "${term}" (per companionBadge/companionDevices.label), not a second ad-hoc rendering`,
  );

  assert.ok(
    !copy.includes("—") && !copy.includes("–"),
    `${locale}: overlay.notice.companionSessionCapped must not use em or en dashes`,
  );

  if (locale === "ja") {
    assert.ok(
      !/:(?! )/.test(copy),
      "ja: overlay.notice.companionSessionCapped must not contain a ':' that is not followed by a space (しました:15分の)",
    );
  }
}

// 2: the intended English copy, pinned verbatim.
assert.equal(
  resolveKey(readLocale("en"), "overlay.notice.companionSessionCapped"),
  "Companion dictation ended: the 15-minute session limit was reached on {{device}}.",
  "en copy must read exactly: Companion dictation ended: the 15-minute session limit was reached on {{device}}.",
);

// 4: noticeMessage forwards the event detail as the device option. The
// translate fn echoes the key plus sorted options, same pinning style as
// noticeMessage.test.ts.
const t = (key: string, options?: Record<string, unknown>): string => {
  if (!options || Object.keys(options).length === 0) return key;
  const parts = Object.keys(options)
    .sort()
    .map((name) => `${name}=${String(options[name])}`);
  return `${key}[${parts.join(",")}]`;
};

assert.equal(
  noticeMessage(t, "companion_session_capped", "Pixel 9"),
  "overlay.notice.companionSessionCapped[device=Pixel 9]",
  "companion_session_capped must interpolate the device name from its notice detail",
);
assert.equal(
  noticeMessage(t, "companion_session_capped"),
  "overlay.notice.companionSessionCapped[device=]",
  "companion_session_capped without detail must pass an empty device, never 'undefined'",
);

console.log("companionSessionCapped notice device-slot tests passed");
