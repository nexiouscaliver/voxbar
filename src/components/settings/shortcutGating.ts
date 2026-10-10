import type { AppSettings } from "@/bindings";

/// The settings field holding the feature master toggle that gates a
/// shortcut row, mirroring the backend's `binding_is_active` match arms
/// (src-tauri/src/shortcut/mod.rs): a toggle-off binding holds no global
/// registration, so its row greys out and its recorder must not arm
/// (KB-009). Rows without a master toggle - the main transcribe key and
/// the cancel key - return null and stay interactive always.
export function shortcutMasterToggleField(
  shortcutId: string,
): keyof AppSettings | null {
  switch (shortcutId) {
    case "delete_last_word":
      return "delete_last_word_enabled";
    case "undo":
      return "undo_enabled";
    case "transcribe_commands":
      return "command_mode_enabled";
    case "transcribe_with_post_process":
    case "cycle_post_process_prompt":
      return "post_process_enabled";
    default:
      return null;
  }
}

/// Whether a shortcut row must render disabled: its feature's master
/// toggle (when it has one) is off. Only an explicit `false` disables -
/// while settings are still loading the rows render enabled, matching
/// the General tab rows' `?? true` fallbacks (the recorder chip itself
/// only renders once bindings are loaded).
export function shortcutRowDisabled(
  shortcutId: string,
  settings: AppSettings | null,
): boolean {
  const field = shortcutMasterToggleField(shortcutId);
  if (!field) return false;
  return settings?.[field] === false;
}
