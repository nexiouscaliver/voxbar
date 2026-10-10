import assert from "node:assert/strict";
import {
  OVERLAY_LANGUAGE_CACHE_KEY,
  OVERLAY_PLACEMENT_CACHE_KEY,
  normalizePlacement,
  readCachedLanguage,
  readCachedPlacement,
  writeCachedLanguage,
  writeCachedPlacement,
  type OverlayCacheStorage,
} from "./overlayBootstrap";

// An in-memory storage double so the cache helpers run without a DOM.
class MemoryStorage implements OverlayCacheStorage {
  private map = new Map<string, string>();
  getItem(key: string): string | null {
    return this.map.has(key) ? this.map.get(key)! : null;
  }
  setItem(key: string, value: string): void {
    this.map.set(key, value);
  }
}

// Placement normalization: only "top" is top, everything else (including
// junk from an older cache format) falls back to "bottom".
assert.equal(normalizePlacement("top"), "top");
assert.equal(normalizePlacement("bottom"), "bottom");
assert.equal(normalizePlacement(undefined), "bottom");
assert.equal(normalizePlacement("Top"), "bottom");
assert.equal(normalizePlacement("diagonal"), "bottom");

// Language + placement round-trip through a storage.
const storage = new MemoryStorage();
assert.equal(readCachedLanguage(storage), null, "empty cache reads as null");
assert.equal(
  readCachedPlacement(storage),
  "bottom",
  "empty cache defaults to bottom placement",
);

writeCachedLanguage(storage, "de");
writeCachedPlacement(storage, "top");
assert.equal(readCachedLanguage(storage), "de");
assert.equal(readCachedPlacement(storage), "top");

// Overwriting works: the cache holds the latest values only.
writeCachedLanguage(storage, "ja");
writeCachedPlacement(storage, "bottom");
assert.equal(readCachedLanguage(storage), "ja");
assert.equal(readCachedPlacement(storage), "bottom");

// The helpers touch only their own keys.
assert.equal(storage.getItem(OVERLAY_LANGUAGE_CACHE_KEY), "ja");
assert.equal(storage.getItem(OVERLAY_PLACEMENT_CACHE_KEY), "bottom");

// A corrupted placement value normalizes instead of crashing the boot.
writeCachedPlacement(storage, "nonsense" as never);
assert.equal(readCachedPlacement(storage), "bottom");

// A null storage (localStorage unavailable) never throws and yields
// defaults, so a locked-down webview still boots.
assert.equal(readCachedLanguage(null), null);
assert.equal(readCachedPlacement(null), "bottom");
writeCachedLanguage(null, "de");
writeCachedPlacement(null, "top");

console.log("overlayBootstrap: all assertions passed");
