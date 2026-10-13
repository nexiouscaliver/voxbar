import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";

/* KB-262 pin (v1.6.5): the AI-unavailable copy could not distinguish a
 * build-lacking-AI Mac from an OS-lacking one because the availability
 * probe was a single bool - a stub-linked build (toolchain without
 * FoundationModels) rendered the same "requires macOS Tahoe" message the
 * OS-lacks case gets, and the stub's own distinct string never reached
 * the availability path. The probe is now a tri-state
 * (available / os_unsupported / build_lacks), one new locale string
 * (appleIntelligence.buildLacks) ships across every locale, and BOTH
 * surfaces (post-process settings card + studio engine row) render it.
 *
 * No React render harness exists in this repo, so this pin holds the
 * wired-together halves as source assertions in the repo's assert-script
 * style: the typed binding, the two render surfaces' branch on
 * build_lacks, and the locale coverage of the new string. The Rust half
 * (the probe itself + the build-script cfg that flags the stub arm) is
 * pinned by availability_probe_reports_the_tri_state_kb262 and
 * build_script_flags_the_stub_arm_for_the_probe_kb262 in
 * src-tauri/src/apple_intelligence.rs. */

const here = import.meta.dirname;
const repoRoot = path.join(here, "..", "..", "..", "..");

function read(rel: string): string {
  return fs.readFileSync(path.join(repoRoot, rel), "utf8");
}

// 1. The generated binding answers with the tri-state type, not a bool.
{
  const bindings = read("src/bindings.ts");
  assert.ok(
    /async checkAppleIntelligenceAvailable\(\) : Promise<AppleIntelligenceStatus>/.test(
      bindings,
    ),
    "KB-262: checkAppleIntelligenceAvailable must return the tri-state status",
  );
  assert.ok(
    !/async checkAppleIntelligenceAvailable\(\) : Promise<boolean>/.test(
      bindings,
    ),
    "KB-262: the bool probe must not survive in the bindings",
  );
  const typeMatch = bindings.match(
    /export type AppleIntelligenceStatus = [^;]+;/,
  );
  assert.ok(typeMatch, "KB-262: the status type must be exported");
  for (const variant of ['"available"', '"os_unsupported"', '"build_lacks"']) {
    assert.ok(
      typeMatch[0].includes(variant),
      `KB-262: the status type must carry the ${variant} variant`,
    );
  }
}

// 2. The settings surface words the two unavailable verdicts differently:
//    build_lacks gets its own copy, os_unsupported keeps the OS copy, and
//    the available verdict renders nothing.
{
  const settings = read(
    "src/components/settings/post-processing/PostProcessingSettings.tsx",
  );
  const appleBranch = settings.slice(
    settings.indexOf("state.isAppleProvider ?"),
    settings.indexOf("state.selectedProvider?.id === \"custom\""),
  );
  assert.ok(
    appleBranch.includes('=== "build_lacks"') &&
      appleBranch.includes("settings.postProcessing.api.appleIntelligence.buildLacks"),
    "KB-262: the settings card must render the buildLacks copy for a build lacking AI",
  );
  assert.ok(
    appleBranch.includes('=== "os_unsupported"') &&
      appleBranch.includes("settings.postProcessing.api.appleIntelligence.unavailable"),
    "KB-262: the settings card must keep the OS-requirements copy for os_unsupported",
  );
}

// 3. The studio engine row picks the buildLacks note for build_lacks and
//    never shows it for an available engine.
{
  const studio = read("src/components/settings/studio/PromptStudio.tsx");
  assert.ok(
    studio.includes('appleStatus === "build_lacks"') &&
      studio.includes("settings.postProcessing.api.appleIntelligence.buildLacks"),
    "KB-262: the studio engine row must render the buildLacks note",
  );
  assert.ok(
    studio.includes('appleStatus !== "available"'),
    "KB-262: an available Apple Intelligence engine must show its model, not a note",
  );
  // The probe feeds the status straight through, no bool collapse.
  assert.ok(
    studio.includes(".then((status) => {") &&
      studio.includes("setAppleStatus(status)"),
    "KB-262: the studio probe handler must keep the tri-state verdict",
  );
}

// 4. The hook behind the settings surface keeps the tri-state too.
{
  const hook = read(
    "src/components/settings/PostProcessingSettingsApi/usePostProcessProviderState.ts",
  );
  assert.ok(
    hook.includes('setAppleIntelligenceStatus(status)') &&
      hook.includes('if (status !== "available")'),
    "KB-262: the provider-select hook must branch on the tri-state verdict",
  );
}

// 5. The build-lacks copy ships in every locale (26 + zh-TW set).
{
  const localesDir = path.join(repoRoot, "src", "i18n", "locales");
  const shipped = fs.readdirSync(localesDir);
  assert.ok(shipped.length >= 26, `expected the full locale set`);
  for (const locale of shipped) {
    const ai = JSON.parse(
      fs.readFileSync(
        path.join(localesDir, locale, "translation.json"),
        "utf8",
      ),
    ).settings.postProcessing.api.appleIntelligence;
    assert.ok(
      typeof ai.buildLacks === "string" && ai.buildLacks.length > 0,
      `KB-262: ${locale} must ship settings.postProcessing.api.appleIntelligence.buildLacks`,
    );
    assert.ok(
      typeof ai.unavailable === "string" && ai.unavailable.length > 0,
      `KB-262: ${locale} must keep the os_unsupported copy (unavailable)`,
    );
  }
}

console.log(
  "appleAvailabilityPins (KB-262): tri-state probe wired through bindings, settings, studio, and every locale",
);
