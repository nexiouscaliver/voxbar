import catalog from "../src-tauri/src/catalog/catalog.json";
import {
  MODEL_CAPABILITY_LANGUAGES,
  recognitionLanguage,
  supportsLanguageCode,
} from "../src/lib/constants/languages.ts";

interface CatalogModel {
  name: string;
  languages?: string[];
}

const failures: string[] = [];

// These model-code variants intentionally share one persisted UI intent. Keep
// this assertion beside catalog coverage so adding a separate picker entry
// cannot silently break language continuity when users switch model families.
if (
  !supportsLanguageCode(["nb"], "no") ||
  !supportsLanguageCode(["no"], "nb")
) {
  failures.push(
    "Norwegian intent `no` must remain equivalent to model code `nb`",
  );
}

// The Hindi/Hinglish pair is the deliberate script-variant family the loop
// below must accept: one `hi` model code intentionally backs both picker
// rows (Devanagari output vs Roman script), per effectiveLanguage in
// src/components/settings/LanguageSelector.tsx.
if (
  !supportsLanguageCode(["hi"], "hi") ||
  !supportsLanguageCode(["hi"], "hi-Latn")
) {
  failures.push(
    "Hindi model code `hi` must back both `hi` and `hi-Latn` intents",
  );
}

for (const model of catalog.models as CatalogModel[]) {
  for (const modelLanguage of model.languages ?? []) {
    const base = recognitionLanguage(modelLanguage);
    const matchingIntents = MODEL_CAPABILITY_LANGUAGES.filter((language) =>
      supportsLanguageCode([modelLanguage], language.value),
    );

    if (matchingIntents.length === 0) {
      failures.push(
        `${model.name}: ${modelLanguage} (no frontend language intent)`,
      );
      continue;
    }

    // Matching collapses codes, so every match shares `base`. A multi-match
    // is valid only as the base intent plus deliberate script variants
    // (`hi` + `hi-Latn`); a duplicate that shares the base any other way
    // (a stray `nb` entry next to `no`) stays ambiguous.
    const isSingleFamily =
      matchingIntents.length === 1 ||
      matchingIntents.every(
        (language) =>
          language.value === base ||
          new RegExp(`^${base}-[A-Za-z]{4}$`).test(language.value),
      );

    if (!isSingleFamily) {
      failures.push(
        `${model.name}: ${modelLanguage} (ambiguous intents: ${matchingIntents
          .map((language) => language.value)
          .join(", ")})`,
      );
    }
  }
}

if (failures.length > 0) {
  console.error("Model language coverage check failed:\n");
  for (const failure of failures) console.error(`  - ${failure}`);
  process.exit(1);
}

console.log(
  `Model language coverage: all catalog codes map to exactly one of ${MODEL_CAPABILITY_LANGUAGES.length} frontend intents (script variants like hi/hi-Latn count as one family)`,
);
