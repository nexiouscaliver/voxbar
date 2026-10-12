import fs from "fs";
import path from "path";
import { fileURLToPath } from "url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

// Configuration
const LOCALES_DIR = path.join(__dirname, "..", "src", "i18n", "locales");
const REFERENCE_LANG = "en";

type TranslationData = Record<string, unknown>;

interface ValidationResult {
  valid: boolean;
  missing: string[][];
  extra: string[][];
}

function getLanguages(): string[] {
  const entries = fs.readdirSync(LOCALES_DIR, { withFileTypes: true });
  return entries
    .filter((entry) => entry.isDirectory() && entry.name !== REFERENCE_LANG)
    .map((entry) => entry.name)
    .sort();
}

const LANGUAGES = getLanguages();

// Colors for terminal output
const colors: Record<string, string> = {
  reset: "\x1b[0m",
  red: "\x1b[31m",
  green: "\x1b[32m",
  yellow: "\x1b[33m",
  blue: "\x1b[34m",
};

function colorize(text: string, color: string): string {
  return `${colors[color]}${text}${colors.reset}`;
}

// i18next count-plural suffixes: a locale legitimately carries ONLY the
// categories its own CLDR plural rules select (ru needs _few/_many, ar
// needs _zero/_two/_few/_many, en needs just _one/_other), so suffix sets
// DIFFER between locales by design. Parity is therefore compared at the
// FAMILY level: a group of >= 2 sibling keys sharing a base name and
// differing only in a plural suffix collapses to the one family path.
// Everything else (all plain keys, and the presence of every family)
// stays an exact-set comparison, so this is not a loosening: a missing
// plain key or family, or an invented key or family, still fails.
const PLURAL_SUFFIXES = new Set(["zero", "one", "two", "few", "many", "other"]);

function pluralFamilyOf(
  key: string,
  siblingKeys: string[],
): { base: string; suffix: string } | null {
  const lastUnderscore = key.lastIndexOf("_");
  if (lastUnderscore <= 0) return null;
  const base = key.slice(0, lastUnderscore);
  const suffix = key.slice(lastUnderscore + 1);
  if (!PLURAL_SUFFIXES.has(suffix)) return null;
  // A lone key that merely ENDS in a plural-suffix word (say a plain
  // "no_one") is not a plural family: require at least one sibling of the
  // same base with a DIFFERENT plural suffix in the same node.
  const hasPluralSibling = siblingKeys.some(
    (sibling) =>
      sibling !== key &&
      sibling.startsWith(`${base}_`) &&
      PLURAL_SUFFIXES.has(sibling.slice(base.length + 1)),
  );
  return hasPluralSibling ? { base, suffix } : null;
}

function getNormalizedKeyPaths(
  obj: TranslationData,
  prefix: string[] = [],
): string[][] {
  let paths: string[][] = [];
  const keys = Object.keys(obj);
  const collapsedFamilies = new Set<string>();
  for (const key of keys) {
    if (!Object.hasOwn(obj, key)) continue;

    const value = obj[key];
    const family = pluralFamilyOf(key, keys);
    if (family) {
      // One path per family, emitted from its first suffix in key order.
      if (!collapsedFamilies.has(family.base)) {
        collapsedFamilies.add(family.base);
        paths.push(prefix.concat([family.base]));
      }
      continue;
    }

    if (typeof value === "object" && value !== null && !Array.isArray(value)) {
      paths = paths.concat(
        getNormalizedKeyPaths(value as TranslationData, prefix.concat([key])),
      );
    } else {
      paths.push(prefix.concat([key]));
    }
  }
  return paths;
}

function loadTranslationFile(lang: string): TranslationData | null {
  const filePath = path.join(LOCALES_DIR, lang, "translation.json");

  try {
    const content = fs.readFileSync(filePath, "utf8");
    return JSON.parse(content) as TranslationData;
  } catch (error) {
    console.error(colorize(`✗ Error loading ${lang}/translation.json:`, "red"));
    console.error(`  ${(error as Error).message}`);
    return null;
  }
}

function validateTranslations(): void {
  console.log(colorize("\n🌍 Translation Consistency Check\n", "blue"));

  // Load reference file
  console.log(`Loading reference language: ${REFERENCE_LANG}`);
  const referenceData = loadTranslationFile(REFERENCE_LANG);

  if (!referenceData) {
    console.error(
      colorize(`\n✗ Failed to load reference file (${REFERENCE_LANG})`, "red"),
    );
    process.exit(1);
  }

  // Get all key paths from reference (plural families collapsed to one)
  const referenceKeyPaths = getNormalizedKeyPaths(referenceData);
  console.log(
    `Reference has ${referenceKeyPaths.length} keys (plural families counted once)\n`,
  );

  // Track validation results
  let hasErrors = false;
  const results: Record<string, ValidationResult> = {};

  // Validate each language
  for (const lang of LANGUAGES) {
    const langData = loadTranslationFile(lang);

    if (!langData) {
      hasErrors = true;
      results[lang] = { valid: false, missing: [], extra: [] };
      continue;
    }

    // Find missing keys (families collapsed, so a locale satisfies a
    // reference plural family by carrying its own category set)
    const langKeyPaths = getNormalizedKeyPaths(langData);
    const langKeySet = new Set(langKeyPaths.map((path) => path.join(".")));
    const missing = referenceKeyPaths.filter(
      (keyPath) => !langKeySet.has(keyPath.join(".")),
    );

    // Find extra keys (keys or families in the language but not in the
    // reference: inventing either still fails)
    const referenceKeySet = new Set(
      referenceKeyPaths.map((path) => path.join(".")),
    );
    const extra = langKeyPaths.filter(
      (keyPath) => !referenceKeySet.has(keyPath.join(".")),
    );

    results[lang] = {
      valid: missing.length === 0 && extra.length === 0,
      missing,
      extra,
    };

    if (missing.length > 0 || extra.length > 0) {
      hasErrors = true;
    }
  }

  // Print results
  console.log(colorize("Results:", "blue"));
  console.log("─".repeat(60));

  for (const lang of LANGUAGES) {
    const result = results[lang];

    if (result.valid) {
      console.log(
        colorize(`✓ ${lang.toUpperCase()}: All keys present`, "green"),
      );
    } else {
      console.log(colorize(`✗ ${lang.toUpperCase()}: Issues found`, "red"));

      if (result.missing.length > 0) {
        console.log(
          colorize(`  Missing ${result.missing.length} keys:`, "yellow"),
        );
        result.missing.slice(0, 10).forEach((keyPath) => {
          console.log(`    - ${keyPath.join(".")}`);
        });
        if (result.missing.length > 10) {
          console.log(
            colorize(
              `    ... and ${result.missing.length - 10} more`,
              "yellow",
            ),
          );
        }
      }

      if (result.extra.length > 0) {
        console.log(
          colorize(
            `  Extra ${result.extra.length} keys (not in reference):`,
            "yellow",
          ),
        );
        result.extra.slice(0, 10).forEach((keyPath) => {
          console.log(`    - ${keyPath.join(".")}`);
        });
        if (result.extra.length > 10) {
          console.log(
            colorize(`    ... and ${result.extra.length - 10} more`, "yellow"),
          );
        }
      }

      console.log("");
    }
  }

  console.log("─".repeat(60));

  // Summary
  const validCount = Object.values(results).filter((r) => r.valid).length;
  const totalCount = LANGUAGES.length;

  if (hasErrors) {
    console.log(
      colorize(
        `\n✗ Validation failed: ${validCount}/${totalCount} languages passed`,
        "red",
      ),
    );
    process.exit(1);
  } else {
    console.log(
      colorize(
        `\n✓ All ${totalCount} languages have complete translations!`,
        "green",
      ),
    );
    process.exit(0);
  }
}

// Run validation
validateTranslations();
