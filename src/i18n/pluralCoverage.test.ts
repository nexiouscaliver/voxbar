import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import i18next from "i18next";

// Fleet pass 2: the six count-based key families (run, runHint, results,
// charsChanged, listSummary, showAll) shipped only _one/_other suffixes, so
// every locale whose CLDR plural categories include _few/_many/_two/_zero
// resolved a MISSING category and silently fell back to English (ru/uk/pl
// at every count except 1 and 21, cs at 2-4, he at 2, ar across its whole
// category set); Hindi (and Nepali, ICU-version-dependent) additionally
// rendered a hardcoded "1" at count=0 because CLDR hi puts 0 in the 'one'
// category. This test pins both invariants for every shipped locale:
//
// 1. STATIC coverage: for every count an integer UI can produce, the
//    suffix key Intl.PluralRules selects for that locale EXISTS in the
//    locale's own file (i18next v25 resolves through Intl.PluralRules, so
//    this is exactly the resolution the app performs).
// 2. EMPIRICAL no-leak: the project's real i18next instance, initialized
//    with the shipped locale files, resolves every probe count from the
//    locale's own strings - never the English fallback.
// 3. The zero facet: the suffix that renders at count=0 must interpolate
//    {{count}}, never a hardcoded numeral.
//
// Counts are integers by construction (pairs, results, chars, model-list
// sizes); fractional categories exist in some CLDR sets but are
// unreachable from the UI, so the sweep stays integral.
//
// KB-276 (v1.6.4): the sweep missed three count families, so they regressed
// while the six above stayed pinned - the two secureInput families (whose
// SecureInputWarning call site hand-rolled a one/other suffix instead of
// letting i18next resolve, wrong grammar in ru/uk/pl/cs/he/ar) and
// liveLogs.lineCount (a bare "{{count}} lines" key that rendered
// "1 lines" in English). All three now ride the same three invariants.

const LOCALES_DIR = fileURLToPath(new URL("./locales", import.meta.url));
const locales = readdirSync(LOCALES_DIR).filter((name) =>
  readdirSync(join(LOCALES_DIR, name)).includes("translation.json"),
);
assert.ok(
  locales.length >= 26,
  `expected the full locale set, found ${locales.length}`,
);

const KEY_FAMILIES = [
  "settings.studio.run",
  "settings.studio.runHint",
  "settings.studio.results",
  "settings.studio.diff.charsChanged",
  "settings.postProcessing.api.model.listSummary",
  "settings.postProcessing.api.model.showAll",
  // KB-276: the secureInput count families and the live-log line count.
  "secureInput.blockedWithCulprit",
  "secureInput.blockedNoCulprit",
  "settings.debug.liveLogs.lineCount",
];

// The numeral-accuracy facet (check 4) runs on the KB-276 families.
const NUMERAL_FAMILIES = KEY_FAMILIES.slice(-3);

const PROBE_COUNTS = [0, 1, 2, 3, 4, 5, 9, 10, 11, 21, 22, 100, 101, 1000];

function loadLocale(locale: string): Record<string, unknown> {
  return JSON.parse(
    readFileSync(join(LOCALES_DIR, locale, "translation.json"), "utf8"),
  );
}

function lookup(tree: Record<string, unknown>, dotted: string): unknown {
  let node: unknown = tree;
  for (const part of dotted.split(".")) {
    if (node === null || typeof node !== "object") return undefined;
    node = (node as Record<string, unknown>)[part];
  }
  return node;
}

// Plural families live FLAT in the files: `run_one`/`run_other` are
// sibling keys inside their parent node, and i18next resolves the suffix
// from the bare key + count. The static walk therefore inspects the
// parent's `leaf_category` keys.
function parentAndLeaf(
  tree: Record<string, unknown>,
  dotted: string,
): { parent: Record<string, string>; leaf: string } {
  const parts = dotted.split(".");
  const leaf = parts.pop() as string;
  const parent = lookup(tree, parts.join("."));
  assert.ok(
    parent !== null && typeof parent === "object",
    `${dotted}: parent node missing`,
  );
  return { parent: parent as Record<string, string>, leaf };
}

// 1 + 3: static coverage and the zero facet, straight off the files.
for (const locale of locales) {
  const tree = loadLocale(locale);
  const rules = new Intl.PluralRules(locale);
  for (const family of KEY_FAMILIES) {
    const { parent, leaf } = parentAndLeaf(tree, family);
    const categories = new Set(
      PROBE_COUNTS.map((count) => rules.select(count)),
    );
    for (const category of categories) {
      assert.ok(
        `${leaf}_${category}` in parent,
        `${locale}: ${family}_${category} is missing - Intl.PluralRules selects it ` +
          `for an integer count, so i18next would silently fall back to English`,
      );
    }
    // The suffix that renders at count=0 interpolates the count.
    const zeroCategory = rules.select(0);
    const zeroString = parent[`${leaf}_${zeroCategory}`];
    assert.ok(
      typeof zeroString === "string" && zeroString.includes("{{count}}"),
      `${locale}: ${family}_${zeroCategory} renders at count=0 and must ` +
        `interpolate {{count}}, not a hardcoded numeral`,
    );
  }
}

// 2: the empirical no-leak check with the project's own i18next, exactly
// the resolution path the app uses (the same method the fleet reviewer
// used to expose the leak).
const resources: Record<string, { translation: Record<string, unknown> }> = {};
for (const locale of locales) {
  resources[locale] = { translation: loadLocale(locale) };
}
await i18next.init({
  lng: "en",
  fallbackLng: "en",
  resources,
  interpolation: { escapeValue: false },
  returnEmptyString: false,
});
const en = i18next.getFixedT("en");
for (const locale of locales) {
  if (locale === "en") continue;
  const t = i18next.getFixedT(locale);
  // i18next keys use the flat dotted path; force reload per language.
  for (const family of KEY_FAMILIES) {
    for (const count of PROBE_COUNTS) {
      const mine = t(family, { count });
      const english = en(family, { count });
      assert.ok(
        typeof mine === "string" && mine.length > 0,
        `${locale}: ${family} at count=${count} resolved to nothing`,
      );
      assert.notEqual(
        mine,
        english,
        `${locale}: ${family} at count=${count} leaked the English fallback ` +
          `("${english}") - a plural category key is missing or mistranslated`,
      );
    }
  }
}

// 4. NUMERAL accuracy for the KB-276 families: a category that fires at a
//    count OTHER than 1 must never hardcode the numeral 1. Russian and
//    Ukrainian 'one' also fires at 21, 31, 41, ..., 101, 1001 (CLDR: n%10=1
//    && n%100!=11), so a "1 строка"-style _one key renders the WRONG NUMBER
//    at 21 lines in the live-log counter and the secure-input banner - the
//    regression this facet exists to catch. Every digit run in the rendered
//    string must equal the count, checked empirically through the same
//    i18next resolution the app performs. Scoped to the three KB-276
//    families: the six fleet-pass-2 families still carry hardcoded "1" in
//    their ru/uk _one keys (twenty keys, the same wrong-number class at
//    21+); that defect predates this lane - these families already
//    resolved through i18next before it, so nothing here regressed them -
//    and they stay out until a dedicated pass fixes the copy.
for (const locale of locales) {
  const t = i18next.getFixedT(locale);
  for (const family of NUMERAL_FAMILIES) {
    for (const count of PROBE_COUNTS) {
      const rendered = t(family, { count });
      assert.ok(typeof rendered === "string" && rendered.length > 0);
      for (const digitRun of rendered.match(/\d+/g) ?? []) {
        assert.equal(
          digitRun,
          String(count),
          `${locale}: ${family} at count=${count} rendered the numeral ` +
            `"${digitRun}" - a plural category that fires beyond count=1 ` +
            `must interpolate {{count}}, not hardcode a number`,
        );
      }
    }
  }
}

console.log(
  `pluralCoverage: ${locales.length} locales x ${KEY_FAMILIES.length} families x ` +
    `${PROBE_COUNTS.length} counts covered, no English leaks, numerals accurate`,
);
