import assert from "node:assert/strict";
import { noticeMessage, noticeNotificationBody } from "./noticeMessage";

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
// KB-015/KB-122: the memory-gate refusal's own code. The detail is
// unit-separator encoded (model, needed, free - memory_gate_notice_detail
// in transcription.rs); the localized line interpolates all three instead
// of pasting the raw English refusal sentence into the model-name slot.
assert.equal(
  noticeMessage(
    t,
    "model_load_failed_memory_gate",
    "Whisper Tiny\u001f43 MB\u001f1.0 GB",
  ),
  "errors.modelLoadFailedMemoryGate[free=1.0 GB,model=Whisper Tiny,needed=43 MB]",
  "the encoded refusal detail interpolates model, needed, and free",
);
assert.equal(
  noticeMessage(t, "model_load_failed_memory_gate", "Whisper Tiny"),
  "errors.modelLoadFailedMemoryGate[free=,model=Whisper Tiny,needed=]",
  "a bare-model refusal detail interpolates empty amounts, never literals",
);
// Fleet pass 1: the memory-gate notification body keeps the FULLY
// interpolated line. The code is not in the body-stripping set: its three
// slots (model, needed, free) all sit mid-sentence in every one of the 26
// locale templates with text after each, so the empty-detail re-localize
// this test's sibling codes go through left grammatically broken output
// in every locale ("Not enough free memory to load : it needs about ,
// with about free."). The detail is app-manufactured (model name and two
// human-readable amounts, never a raw backend diagnostic), so the full
// line is safe on the system surface - the same decision as the
// companion_session_capped device slot.
assert.equal(
  noticeNotificationBody(
    t,
    "model_load_failed_memory_gate",
    "Whisper Tiny\u001f43 MB\u001f1.0 GB",
  ),
  "errors.modelLoadFailedMemoryGate[free=1.0 GB,model=Whisper Tiny,needed=43 MB]",
  "the memory-gate notification body is the fully interpolated line, never the detail-stripped garble",
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
// Round 7 truth pass: the config-gap fallback (detail names the gap) and
// the no-op guard's unchanged outcome (nothing interpolated).
assert.equal(
  noticeMessage(
    t,
    "post_process_not_configured",
    "no prompt template is selected",
  ),
  "overlay.notice.postProcessNotConfigured[detail=no prompt template is selected]",
);
assert.equal(
  noticeMessage(t, "post_process_not_configured"),
  "overlay.notice.postProcessNotConfigured[detail=]",
  "a missing gap detail interpolates as empty, never as 'undefined'",
);
assert.equal(
  noticeMessage(t, "post_process_unchanged"),
  "overlay.notice.postProcessUnchanged",
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
// v1.6.0 W1-L7: the cycle detail carries name, 1-based position, and total
// (unit-separator encoded by cycle_notice_detail), and the line localizes
// the "2 of 5" phrasing through the position key. A bare-name detail (the
// pre-v1.6.0 shape) keeps the plain key.
assert.equal(
  noticeMessage(t, "post_process_prompt_cycled", "Concise 2 5"),
  "overlay.notice.postProcessPromptCycled[name=Concise 2 5]",
  "a detail without separators is a plain name, never split",
);
assert.equal(
  noticeMessage(t, "post_process_prompt_cycled", "Meeting Notes\u001f2\u001f5"),
  "overlay.notice.postProcessPromptCycledPosition[name=Meeting Notes,position=2,total=5]",
  "the encoded detail interpolates name, position, and total",
);
// The mid-session switching notices: the upgrade names the template, the
// revert interpolates nothing.
assert.equal(
  noticeMessage(t, "post_process_session_upgraded", "Meeting Notes"),
  "overlay.notice.postProcessSessionUpgraded[name=Meeting Notes]",
  "an upgrade names the template that will run",
);
assert.equal(
  noticeMessage(t, "post_process_session_upgraded"),
  "overlay.notice.postProcessSessionUpgraded[name=]",
  "an upgrade with no selected template interpolates empty, never 'undefined'",
);
assert.equal(
  noticeMessage(t, "post_process_session_downgraded"),
  "overlay.notice.postProcessSessionDowngraded",
  "a revert carries no template name",
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
  "overlay.notice.companionSessionCapped[device=]",
);
assert.equal(
  noticeMessage(t, "companion_session_capped", "Pixel 9"),
  "overlay.notice.companionSessionCapped[device=Pixel 9]",
);
assert.equal(
  noticeMessage(t, "command_mode_no_session"),
  "app.commandNoSession",
  "command_mode_no_session reuses the existing key, no locale additions",
);

// v1.6.0 zero-audio fix: the companion session captured nothing; the key
// ships in every locale and interpolates nothing.
assert.equal(
  noticeMessage(t, "companion_zero_audio"),
  "overlay.notice.companionZeroAudio",
);

// Unknown codes return null: the main-window router skips them silently and
// the overlay card falls back to its generic line.
assert.equal(noticeMessage(t, "something_unmapped"), null);
assert.equal(noticeMessage(t, ""), null);

console.log("noticeMessage tests passed");
