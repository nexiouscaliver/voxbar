import type { SkipReason } from "@/bindings";

// Pure helpers for the post-process skip toast (plan Phase B, T31). The
// App-level listener keeps a module-level Set of already-toasted reasons
// and toasts each reason at most once per app session (spec 7.3); the raw
// transcript is always pasted, the toast only explains why polishing was
// skipped.

// Returns true the first time a reason is seen, false afterwards. Mutates
// the injected set; deterministic and side-effect free otherwise.
export function shouldToast<T extends string>(
  seen: Set<T>,
  reason: T,
): boolean {
  if (seen.has(reason)) return false;
  seen.add(reason);
  return true;
}

// Maps the wire reason (snake_case) to the i18n key segment under
// toast.postProcessSkip (camelCase).
export function skipToastKey(reason: SkipReason): string {
  switch (reason) {
    case "memory_gate":
      return "memoryGate";
    case "download_missing":
      return "downloadMissing";
    case "engine_failed":
      return "engineFailed";
    case "timeout":
      return "timeout";
    case "length_guard":
      return "lengthGuard";
    case "too_long":
      return "tooLong";
  }
}
