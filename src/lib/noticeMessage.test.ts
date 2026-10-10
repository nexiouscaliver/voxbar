import assert from "node:assert/strict";
import { noticeMessage } from "./noticeMessage";

// A translate fn that echoes the key plus sorted interpolation options, so
// the assertions pin the exact key AND detail each code passes to i18n.
const t = (key: string, options?: Record<string, unknown>): string => {
  if (!options || Object.keys(options).length === 0) return key;
  const parts = Object.keys(options)
    .sort()
    .map((name) => `${name}=${String(options[name])}`);
  return `${key}[${parts.join(",")}]`;
};

// Every code must land on its key; the detail-interpolating codes must pass
// the event's detail through (these are the arms whose interpolation the
// extraction had to preserve exactly for the overlay card).
assert.equal(noticeMessage(t, "no_model_selected"), "errors.noModelSelected");
assert.equal(
  noticeMessage(t, "microphone_permission_denied"),
  "errors.micPermissionDenied.generic",
);
assert.equal(noticeMessage(t, "no_input_device"), "errors.noInputDevice");
assert.equal(
  noticeMessage(t, "recording_failed", "mic busy"),
  "errors.recordingFailed[error=mic busy]",
  "recording_failed interpolates its detail",
);
assert.equal(
  noticeMessage(t, "recording_failed"),
  "errors.recordingFailed[error=]",
  "a missing detail interpolates as empty, never as 'undefined'",
);
assert.equal(
  noticeMessage(t, "transcription_failed"),
  "overlay.notice.transcriptionFailed",
);
assert.equal(noticeMessage(t, "paste_failed"), "errors.pasteFailed");
assert.equal(
  noticeMessage(t, "model_load_failed", "Whisper Large"),
  "errors.modelLoadFailed[model=Whisper Large]",
);
assert.equal(
  noticeMessage(t, "model_load_failed"),
  "errors.modelLoadFailed[model=errors.modelLoadFailedUnknown]",
  "model_load_failed without detail falls back to the unknown-model key",
);
assert.equal(
  noticeMessage(t, "model_fallback", "whisper-tiny"),
  "errors.modelFallback[model=whisper-tiny]",
);
assert.equal(
  noticeMessage(t, "post_process_memory_gate", "need 4 GB, have 2 GB"),
  "toast.postProcessSkip.memoryGate[detail=need 4 GB, have 2 GB]",
);
assert.equal(
  noticeMessage(t, "post_process_download_missing"),
  "toast.postProcessSkip.downloadMissing",
);
assert.equal(
  noticeMessage(t, "post_process_engine_failed"),
  "toast.postProcessSkip.engineFailed",
);
assert.equal(
  noticeMessage(t, "post_process_timeout"),
  "toast.postProcessSkip.timeout",
);
assert.equal(
  noticeMessage(t, "post_process_length_guard"),
  "toast.postProcessSkip.lengthGuard",
);
assert.equal(
  noticeMessage(t, "post_process_too_long"),
  "toast.postProcessSkip.tooLong",
);
assert.equal(
  noticeMessage(t, "post_process_output_invalid"),
  "overlay.notice.postProcessOutputInvalid",
);
assert.equal(
  noticeMessage(t, "post_process_cloud_failed", "openai/gpt-4o-mini: 401"),
  "overlay.notice.postProcessCloudFailed[detail=openai/gpt-4o-mini: 401]",
);
assert.equal(
  noticeMessage(t, "delete_last_word_no_session"),
  "overlay.notice.deleteLastWordNoSession",
);
assert.equal(
  noticeMessage(t, "delete_last_word_no_buffer"),
  "overlay.notice.deleteLastWordNoBuffer",
);
assert.equal(
  noticeMessage(t, "undo_no_session"),
  "overlay.notice.undoNoSession",
);
assert.equal(noticeMessage(t, "undo_no_buffer"), "overlay.notice.undoNoBuffer");
assert.equal(noticeMessage(t, "binding_busy"), "overlay.notice.bindingBusy");
assert.equal(
  noticeMessage(t, "post_process_prompt_cycled", "Concise"),
  "overlay.notice.postProcessPromptCycled[name=Concise]",
  "prompt cycled interpolates the template name",
);
assert.equal(
  noticeMessage(t, "wayland_tauri_hotkeys"),
  "overlay.notice.waylandTauriHotkeys",
);
assert.equal(
  noticeMessage(t, "gnome_overlay_fallback"),
  "overlay.notice.gnomeOverlayFallback",
);
assert.equal(
  noticeMessage(t, "companion_disconnected_finalized"),
  "overlay.notice.companionDisconnected",
);
assert.equal(
  noticeMessage(t, "companion_server_failed", "bind 0.0.0.0:8443: EADDRINUSE"),
  "overlay.notice.companionServerFailed[error=bind 0.0.0.0:8443: EADDRINUSE]",
);

// The two new routing codes (KB-016 / KB-038).
assert.equal(
  noticeMessage(t, "companion_session_capped"),
  "overlay.notice.companionSessionCapped",
);
assert.equal(
  noticeMessage(t, "command_mode_no_session"),
  "app.commandNoSession",
  "command_mode_no_session reuses the existing key, no locale additions",
);

// Unknown codes return null: the main-window router skips them silently and
// the overlay card falls back to its generic line.
assert.equal(noticeMessage(t, "something_unmapped"), null);
assert.equal(noticeMessage(t, ""), null);

console.log("noticeMessage tests passed");
