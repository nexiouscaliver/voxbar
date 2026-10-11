// Shared notice-code -> localized-message mapping for the overlay notice
// channel. Codes deliberately reuse the strings the main-window toasts
// already ship (one voice for the same failure on both surfaces); the keys
// below exist in every locale. The post-process skips reuse the toast copy
// for the same reason.
//
// Returns null for unknown codes so each surface can pick its fallback: the
// overlay card renders its generic line (RecordingOverlay), while the
// main-window router skips the code silently (App.tsx).

// The translate function react-i18next's useTranslation hands to components;
// typed structurally (as the other shared modules take it) so both React
// callers (App and the overlay component) can pass theirs.
export type NoticeTranslateFn = (
  key: string,
  options?: Record<string, unknown>,
) => string;

// Localize one notice code. `detail` is the event's optional diagnostic
// (error text, model names); the codes that interpolate it fall back to
// empty strings, except model_load_failed whose fallback names the unknown
// model explicitly.
export function noticeMessage(
  t: NoticeTranslateFn,
  code: string,
  detail?: string | null,
): string | null {
  switch (code) {
    case "no_model_selected":
      return t("errors.noModelSelected");
    case "microphone_permission_denied":
      return t("errors.micPermissionDenied.generic");
    case "no_input_device":
      return t("errors.noInputDevice");
    case "recording_failed":
      return t("errors.recordingFailed", { error: detail ?? "" });
    case "transcription_failed":
      return t("overlay.notice.transcriptionFailed");
    case "paste_failed":
      return t("errors.pasteFailed");
    case "model_load_failed":
      return t("errors.modelLoadFailed", {
        model: detail ?? t("errors.modelLoadFailedUnknown"),
      });
    case "model_fallback":
      return t("errors.modelFallback", { model: detail ?? "" });
    case "post_process_memory_gate":
      return t("toast.postProcessSkip.memoryGate", {
        detail: detail ?? "",
      });
    case "post_process_download_missing":
      return t("toast.postProcessSkip.downloadMissing");
    case "post_process_engine_failed":
      return t("toast.postProcessSkip.engineFailed");
    case "post_process_timeout":
      return t("toast.postProcessSkip.timeout");
    case "post_process_length_guard":
      return t("toast.postProcessSkip.lengthGuard");
    case "post_process_too_long":
      return t("toast.postProcessSkip.tooLong");
    case "post_process_output_invalid":
      return t("overlay.notice.postProcessOutputInvalid");
    case "post_process_cloud_failed":
      return t("overlay.notice.postProcessCloudFailed", {
        detail: detail ?? "",
      });
    case "delete_last_word_no_session":
      return t("overlay.notice.deleteLastWordNoSession");
    case "delete_last_word_no_buffer":
      return t("overlay.notice.deleteLastWordNoBuffer");
    case "undo_no_session":
      return t("overlay.notice.undoNoSession");
    case "undo_no_buffer":
      return t("overlay.notice.undoNoBuffer");
    case "binding_busy":
      return t("overlay.notice.bindingBusy");
    case "post_process_prompt_cycled":
      return t("overlay.notice.postProcessPromptCycled", {
        name: detail ?? "",
      });
    case "wayland_tauri_hotkeys":
      return t("overlay.notice.waylandTauriHotkeys");
    case "gnome_overlay_fallback":
      return t("overlay.notice.gnomeOverlayFallback");
    case "companion_disconnected_finalized":
      return t("overlay.notice.companionDisconnected");
    case "companion_server_failed":
      return t("overlay.notice.companionServerFailed", {
        error: detail ?? "",
      });
    // A companion phone dictation hit the 15-minute session cap (KB-016);
    // the key ships in every locale and interpolates the device name the
    // cap event carries as its detail.
    case "companion_session_capped":
      return t("overlay.notice.companionSessionCapped", {
        device: detail ?? "",
      });
    // The command-mode modifier engaged with no live dictation (KB-038);
    // reuses the key the former dedicated toast used, so no new locale
    // strings ride along.
    case "command_mode_no_session":
      return t("app.commandNoSession");
    default:
      return null;
  }
}

// The codes whose localized line ends in (or carries) a raw diagnostic
// detail that must not leave the app's own surfaces — the same arms above
// that pass `detail` into t()'s options, except companion_session_capped:
// its {{device}} slot is user-facing content (which phone's session
// capped), sits mid-sentence in most locales, and removing it would break
// the localized grammar ("wurde auf erreicht."), so the cap's notification
// body keeps the full line.
const DETAIL_INTERPOLATING_CODES = new Set([
  "recording_failed",
  "model_load_failed",
  "model_fallback",
  "post_process_memory_gate",
  "post_process_cloud_failed",
  "post_process_prompt_cycled",
  "companion_server_failed",
]);

// Clean up what a removed {{detail}} interpolation leaves behind: the
// separator punctuation the slot orphaned (": " and friends, also at line
// start for RTL locales), the double space a mid-sentence slot leaves, and
// any placeholder that still survived — i18next leaves an options variable
// it does not have as the literal "{{var}}" (skipOnVariables defaults to
// true), so it is dropped rather than shown.
function stripInterpolationArtifacts(line: string): string {
  return line
    .replace(/\{\{\s*\w+\s*\}\}/g, "")
    .replace(/[ \t]{2,}/g, " ")
    .replace(/[\s:：\-–—]+$/u, "")
    .replace(/^[\s:：\-–—]+/u, "")
    .trim();
}

// AUD-03: the body the OS notification shows for one notice code. Same
// code space and null discipline as noticeMessage, but detail-free — the
// raw backend diagnostic (KB-037 class on a brand-new system surface)
// stays in the toast, which keeps the interpolated noticeMessage output.
// Interpolating codes re-localize with an empty detail (an options
// variable that IS present interpolates its value even when empty) and the
// orphaned separator is then stripped. If the stripped line somehow came
// out empty, null skips the notification rather than showing a blank body.
export function noticeNotificationBody(
  t: NoticeTranslateFn,
  code: string,
  detail?: string | null,
): string | null {
  const message = noticeMessage(t, code, detail);
  if (message === null) return null;
  if (!DETAIL_INTERPOLATING_CODES.has(code)) return message;
  const stripped = stripInterpolationArtifacts(
    noticeMessage(t, code, "") ?? "",
  );
  return stripped.length > 0 ? stripped : null;
}
