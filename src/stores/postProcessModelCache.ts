// Pure helpers for the post-process model-list cache (workstream 2).
// Extracted so the settings store, the provider-state hook, and the
// colocated test share one source of truth:
// - dropdown options hydrate from the persisted settings cache on load, so
//   reopening the panel is instant and works offline
// - a cache hit means no refetch on open (the refresh button stays the
//   explicit way to update a list)
// - a failed fetch never clobbers what is already shown; it only records
//   the classified error for the inline alert
import type { CachedModelList, PostProcessModelError } from "@/bindings";

export type ModelOptionsByProvider = Record<string, string[]>;
// The bindings type the settings store exposes for
// post_process_model_lists is partial (specta renders a serde-default
// HashMap as Partial<Record<...>>), so entries may be undefined.
export type ModelListCache = Record<string, CachedModelList | undefined>;
export type ModelFetchErrors = Record<string, PostProcessModelError | null>;

function hasCachedModels(cached: CachedModelList | undefined): boolean {
  return Array.isArray(cached?.models) && (cached?.models?.length ?? 0) > 0;
}

// A provider has a usable list in the session state only when its entry is
// a non-empty array: the store clears entries by writing [], and a cleared
// entry must not shadow the persisted cache.
export function hasSessionList(
  options: ModelOptionsByProvider,
  providerId: string,
): boolean {
  const entry = options[providerId];
  return Array.isArray(entry) && entry.length > 0;
}

// Merge the persisted cache into the session dropdown options. A list
// fetched this session (non-empty entry) always wins; every other provider
// with a cached list gets its models so the dropdown starts populated.
export function hydrateModelOptions(
  cache: ModelListCache | undefined,
  sessionOptions: ModelOptionsByProvider,
): ModelOptionsByProvider {
  if (!cache) return sessionOptions;
  const merged: ModelOptionsByProvider = { ...sessionOptions };
  for (const [providerId, cached] of Object.entries(cache)) {
    if (hasSessionList(merged, providerId)) continue;
    if (
      !cached ||
      !Array.isArray(cached.models) ||
      cached.models.length === 0
    ) {
      continue;
    }
    merged[providerId] = cached.models;
  }
  return merged;
}

// Whether opening the provider's panel should trigger a fetch: only when
// neither the session nor the persisted cache holds a list (a cache hit is
// exactly the no-refetch case).
export function shouldFetchOnOpen(
  cache: ModelListCache | undefined,
  sessionOptions: ModelOptionsByProvider,
  providerId: string,
): boolean {
  if (hasSessionList(sessionOptions, providerId)) return false;
  if (hasCachedModels(cache?.[providerId])) return false;
  return true;
}

// Record (or clear) the classified fetch error for one provider. The
// options themselves are untouched: a failed refresh never blanks the
// dropdown, it surfaces inline with the failure class.
export function withModelFetchError(
  errors: ModelFetchErrors,
  providerId: string,
  error: PostProcessModelError | null,
): ModelFetchErrors {
  return { ...errors, [providerId]: error };
}
