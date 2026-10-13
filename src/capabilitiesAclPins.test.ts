import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";

/* KB-282 pin: the default capability used to bundle opener:default, which
 * includes allow-reveal-item-in-dir and (through the set) an unscoped
 * arbitrary-path surface reachable from webview JS. The swap keeps exactly
 * what the frontend uses - openUrl for the default URL schemes
 * (https/http/mailto/tel: the help links, release links, and whats-new
 * markdown links are all https) - via allow-open-url plus the default-urls
 * scope, and nothing else. The Rust-side OpenerExt calls (open the
 * recordings folder / log dir) bypass the ACL entirely and are untouched.
 *
 * Static source assertion in the repo's assert-script style (same
 * technique as appUxPins.test.ts): the capability file is declarative
 * config, so its committed content IS the behavior. */

const here = import.meta.dirname;
const capabilityPath = path.join(
  here,
  "..",
  "src-tauri",
  "capabilities",
  "default.json",
);
const capability: {
  permissions: Array<string | { identifier: string }>;
} = JSON.parse(fs.readFileSync(capabilityPath, "utf8"));

const permissionIds = capability.permissions.map((entry) =>
  typeof entry === "string" ? entry : entry.identifier,
);

assert.ok(
  permissionIds.includes("opener:allow-open-url"),
  "KB-282: the openUrl command the frontend calls must stay allowed",
);
assert.ok(
  permissionIds.includes("opener:allow-default-urls"),
  "KB-282: the default URL schemes (https/http/mailto/tel) must stay in scope for openUrl",
);
assert.ok(
  !permissionIds.includes("opener:default"),
  "KB-282: the opener:default set must be gone - it bundles reveal-item-in-dir (an arbitrary-path Finder reveal from webview JS)",
);
assert.ok(
  !permissionIds.includes("opener:allow-reveal-item-in-dir"),
  "KB-282: no arbitrary-path reveal permission may be webview-reachable",
);
assert.ok(
  !permissionIds.includes("opener:allow-open-path"),
  "KB-282: no arbitrary-path open permission may be webview-reachable (KB-275 already removed it; this pin keeps it out)",
);
// Belt and braces: the bundle name must not reappear anywhere in the file
// (a comment or description string reintroducing it would hide a future
// regression from the array checks above).
const raw = fs.readFileSync(capabilityPath, "utf8");
assert.ok(
  !raw.includes("reveal-item-in-dir"),
  "KB-282: reveal-item-in-dir must not appear anywhere in the capability",
);

console.log("capabilities ACL pins (KB-282) passed");
