import fs from "fs";
import os from "os";
import path from "path";
import { fileURLToPath } from "url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

// Text-style gate for shipped copy (the operator's voice rules): no em dashes
// (U+2014) and no en dashes (U+2013) in anything a user reads. Commas, colons,
// and periods do the same job without the AI-tell punctuation.

// Configuration
const REPO_ROOT = path.join(__dirname, "..");
const LOCALES_DIR = path.join(REPO_ROOT, "src", "i18n", "locales");
const README = path.join(REPO_ROOT, "README.md");
const DOCS_DIR = path.join(REPO_ROOT, "docs");

// The banned characters, named for the report lines.
const BANNED: Array<{ char: string; label: string }> = [
  { char: "\u2014", label: "em dash" },
  { char: "\u2013", label: "en dash" },
];

type Finding = {
  file: string;
  location: string;
  label: string;
};

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

/// Scan one string, returning a finding per banned character present.
function scanString(
  text: string,
  file: string,
  location: string,
): Array<{ label: string; char: string }> {
  const hits: Array<{ label: string; char: string }> = [];
  for (const { char, label } of BANNED) {
    if (text.includes(char)) {
      hits.push({ label, char });
    }
  }
  return hits.map((hit) => ({ ...hit, label: hit.label, char: hit.char }));
}

/// Walk a parsed JSON value, collecting findings with dot-separated key paths.
function scanJsonValue(
  value: unknown,
  file: string,
  prefix: string[],
  findings: Finding[],
): void {
  if (typeof value === "string") {
    for (const hit of scanString(value, file, prefix.join("."))) {
      findings.push({ file, location: prefix.join("."), label: hit.label });
    }
  } else if (Array.isArray(value)) {
    value.forEach((item, index) => {
      scanJsonValue(item, file, prefix.concat(String(index)), findings);
    });
  } else if (typeof value === "object" && value !== null) {
    for (const key of Object.keys(value as Record<string, unknown>)) {
      scanJsonValue(
        (value as Record<string, unknown>)[key],
        file,
        prefix.concat(key),
        findings,
      );
    }
  }
}

/// Scan a JSON translation file, reporting key paths.
function scanJsonFile(absPath: string, findings: Finding[]): boolean {
  let parsed: unknown;
  try {
    parsed = JSON.parse(fs.readFileSync(absPath, "utf8"));
  } catch (error) {
    console.error(
      colorize(
        `✗ Could not parse ${absPath}: ${(error as Error).message}`,
        "red",
      ),
    );
    return false;
  }
  scanJsonValue(parsed, absPath, [], findings);
  return true;
}

/// Scan a plain-text file (markdown, docs), reporting 1-based line numbers.
function scanTextFile(absPath: string, findings: Finding[]): void {
  const lines = fs.readFileSync(absPath, "utf8").split("\n");
  lines.forEach((line, index) => {
    for (const hit of scanString(line, absPath, `line ${index + 1}`)) {
      findings.push({
        file: absPath,
        location: `line ${index + 1}`,
        label: hit.label,
      });
    }
  });
}

function isMarkdown(file: string): boolean {
  return file.endsWith(".md");
}

/// Scan one filesystem path (file or directory) for banned characters.
function scanPath(target: string, findings: Finding[]): boolean {
  const stat = fs.statSync(target);
  if (stat.isFile()) {
    if (isMarkdown(target)) {
      scanTextFile(target, findings);
      return true;
    }
    if (target.endsWith(".json")) {
      return scanJsonFile(target, findings);
    }
    return true;
  }
  let ok = true;
  for (const entry of fs
    .readdirSync(target, { withFileTypes: true })
    .sort((a, b) => a.name.localeCompare(b.name))) {
    ok = scanPath(path.join(target, entry.name), findings) && ok;
  }
  return ok;
}

/// The scanner entry point used by both the real check and the self-test.
export function scanTargets(targets: string[]): {
  findings: Finding[];
  parseFailures: boolean;
} {
  const findings: Finding[] = [];
  let parseFailures = false;
  for (const target of targets) {
    if (!fs.existsSync(target)) {
      continue;
    }
    if (!scanPath(target, findings)) {
      parseFailures = true;
    }
  }
  return { findings, parseFailures };
}

/// Unit self-test: the scanner must flag seeded em and en dashes in JSON key
/// paths and markdown lines, and must pass clean copy. Run with --self-test.
function runSelfTest(): number {
  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "voxbar-text-style-"));
  let failures = 0;
  const expect = (condition: boolean, message: string) => {
    if (!condition) {
      failures += 1;
      console.error(colorize(`✗ self-test: ${message}`, "red"));
    }
  };

  // A locale-shaped JSON fixture with one em dash and one en dash, plus a
  // clean key.
  const localeDir = path.join(tmpDir, "xx");
  fs.mkdirSync(localeDir);
  fs.writeFileSync(
    path.join(localeDir, "translation.json"),
    JSON.stringify({
      overlay: {
        clean: "Commas, colons, and periods.",
        emDash: "before\u2014after",
        nested: { enDash: "1\u20132" },
      },
    }),
  );
  // A markdown fixture with an em dash on a known line.
  const mdPath = path.join(tmpDir, "doc.md");
  fs.writeFileSync(mdPath, "Clean line.\nSecond\u2014with a dash.\n");

  const { findings } = scanTargets([localeDir, mdPath]);

  expect(findings.length === 3, `expected 3 findings, got ${findings.length}`);
  expect(
    findings.some(
      (f) => f.location === "overlay.emDash" && f.label === "em dash",
    ),
    "the em dash inside overlay.emDash must be reported with its key path",
  );
  expect(
    findings.some(
      (f) => f.location === "overlay.nested.enDash" && f.label === "en dash",
    ),
    "the en dash inside the nested key must be reported with its key path",
  );
  expect(
    findings.some((f) => f.location === "line 2" && f.label === "em dash"),
    "the markdown em dash must be reported with its line number",
  );
  expect(
    !findings.some((f) => f.location.includes("clean")),
    "clean copy must not be flagged",
  );

  fs.rmSync(tmpDir, { recursive: true, force: true });
  if (failures === 0) {
    console.log(colorize("✓ text-style scanner self-test passed", "green"));
    return 0;
  }
  return 1;
}

function main(): number {
  const args = process.argv.slice(2);
  if (args.includes("--self-test")) {
    return runSelfTest();
  }

  // Default surfaces are the shipped copy; extra positional arguments scan
  // additional paths (how the RED half of the gate's demo runs).
  const targets = [LOCALES_DIR, README, DOCS_DIR, ...args];
  const { findings, parseFailures } = scanTargets(targets);

  console.log(colorize("\n✍️  Text Style Check (no em/en dashes)\n", "blue"));
  console.log("─".repeat(60));

  if (findings.length === 0 && !parseFailures) {
    console.log(
      colorize("✓ No em dashes or en dashes in shipped text.", "green"),
    );
    return 0;
  }

  for (const finding of findings) {
    console.log(
      colorize(
        `✗ ${finding.label} in ${path.relative(REPO_ROOT, finding.file)} at ${finding.location}`,
        "yellow",
      ),
    );
    if (finding.location.startsWith("line")) {
      console.log(`    fix: replace the dash with a comma, colon, or period`);
    } else {
      console.log(
        `    fix: rewrite the string at ${finding.location} without the dash`,
      );
    }
  }
  console.log("─".repeat(60));
  console.log(
    colorize(
      `\n✗ ${findings.length} banned character${findings.length === 1 ? "" : "s"} found`,
      "red",
    ),
  );
  return 1;
}

process.exit(main());
