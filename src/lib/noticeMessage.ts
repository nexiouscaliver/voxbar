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

// v1.6.0 W1-L7: the cycle notice's detail carries three facts (template
// name, 1-based position, library total) separated by the ASCII unit
// separator (0x1f) - a control character no real template name contains -
// so one plain string can feed localized interpolation. The Rust emitter is
// cycle_notice_detail (shortcut/mod.rs); both sides pin the separator.
const CYCLE_DETAIL_SEPARATOR = "\u001f";

function parseCycleDetail(detail: string): {
  name: string;
  position?: string;
  total?: string;
} {
  const parts = detail.split(CYCLE_DETAIL_SEPARATOR);
  if (parts.length === 3 && parts[1] !== "" && parts[2] !== "") {
    return { name: parts[0], position: parts[1], total: parts[2] };
  }
  return { name: detail };
}

// KB-015/KB-122: the memory-gate refusal's structured detail - model,
// needed, free - unit-separator encoded by memory_gate_notice_detail
// (transcription.rs), the same convention as the cycle detail, so the
// localized line interpolates the three facts instead of pasting the raw
// English refusal sentence into the model-name slot.
function parseMemoryGateDetail(detail: string): {
  model: string;
  needed?: string;
  free?: string;
} {
  const parts = detail.split(CYCLE_DETAIL_SEPARATOR);
  if (parts.length === 3 && parts[1] !== "" && parts[2] !== "") {
    return { model: parts[0], needed: parts[1], free: parts[2] };
  }
  return { model: detail };
}

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
    // KB-015/KB-122: the memory-gate refusal gets its own line so the
    // raw English refusal sentence never interpolates into the generic
    // failure's model-name slot. The detail carries model, needed, and
    // free (unit-separator encoded); a bare-model detail (a defensive
    // shape) interpolates empty amounts rather than the literal
    // "{{needed}}".
    case "model_load_failed_memory_gate": {
      const gate = parseMemoryGateDetail(detail ?? "");
      return t("errors.modelLoadFailedMemoryGate", {
        model: gate.model,
        needed: gate.needed ?? "",
        free: gate.free ?? "",
      });
    }
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
    // Round 7 truth pass: the polish was requested but a config gap (no
    // provider, no model, no usable template) meant no engine ever ran;
    // the raw transcript pasted. Detail carries the specific gap.
    case "post_process_not_configured":
      return t("overlay.notice.postProcessNotConfigured", {
        detail: detail ?? "",
      });
    // Round 7 no-op guard: the engine ran, validated, and changed nothing
    // (a verbatim echo); the pasted text equals the raw transcript because
    // nothing was changed, not because the run failed.
    case "post_process_unchanged":
      return t("overlay.notice.postProcessUnchanged");
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
    case "post_process_prompt_cycled": {
      // v1.6.0 W1-L7: the detail carries name, 1-based position, and total
      // (unit-separator encoded by cycle_notice_detail) so the line can say
      // "2 of 5" in the locale's own grammar; a detail without the
      // separators is the pre-v1.6.0 bare-name shape.
      const cycle = parseCycleDetail(detail ?? "");
      if (cycle.position !== undefined && cycle.total !== undefined) {
        return t("overlay.notice.postProcessPromptCycledPosition", {
          name: cycle.name,
          position: cycle.position,
          total: cycle.total,
        });
      }
      return t("overlay.notice.postProcessPromptCycled", {
        name: cycle.name,
      });
    }
    // v1.6.0 W1-L7 mid-session switching: the pp hotkey upgraded the live
    // session (the template name rides the detail) or reverted it.
    case "post_process_session_upgraded":
      return t("overlay.notice.postProcessSessionUpgraded", {
        name: detail ?? "",
      });
    case "post_process_session_downgraded":
      return t("overlay.notice.postProcessSessionDowngraded");
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
    // v1.6.0: a companion session ended having captured zero audio (the
    // suspended-AudioContext defect class), so nothing was transcribed and
    // no history entry exists; previously this dropped silently. No
    // interpolation: the device detail stays in the log line.
    case "companion_zero_audio":
      return t("overlay.notice.companionZeroAudio");
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
// that pass `detail` into t()'s options, except:
//
// * companion_session_capped: its {{device}} slot is user-facing content
//   (which phone's session capped), sits mid-sentence in most locales, and
//   removing it would break the localized grammar ("wurde auf erreicht."),
//   so the cap's notification body keeps the full line.
// * model_load_failed_memory_gate (fleet pass 1): its detail is not a raw
//   backend diagnostic but the app-manufactured structured triple (model,
//   needed, free - memory_gate_notice_detail), and ALL THREE slots sit
//   mid-sentence in every one of the 26 locale templates with text after
//   each. Re-localizing with an empty detail - the stripping this set
//   applies - left dangling colons, commas, and empty slots in every
//   locale ("Not enough free memory to load : it needs about , with about
//   free."), so the gate's notification body keeps the fully interpolated
//   line, the same decision as the cap's.
const DETAIL_INTERPOLATING_CODES = new Set([
  "recording_failed",
  "model_load_failed",
  "model_fallback",
  "post_process_memory_gate",
  "post_process_cloud_failed",
  "post_process_prompt_cycled",
  "post_process_session_upgraded",
  "companion_server_failed",
  "post_process_not_configured",
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
