// v1.6.0 W1-L7: the post-process badge event seam. The Rust coordinator
// emits this event to the recording_overlay window with the payload shape
// below (emit_post_process_badge / post_process_badge_payload in
// transcription_coordinator.rs). Both sides pin the same literals in their
// tests (POST_PROCESS_BADGE_EVENT there asserts "post-process-changed";
// ppBadge.test.ts asserts it here), so the seam cannot drift silently.

// The event name the coordinator emits and this overlay listens for.
export const POST_PROCESS_BADGE_EVENT = "post-process-changed";

// One backend badge edge as it arrives over the Tauri event channel:
// `active` gates the chip, `template` names it. Fields are optional on the
// wire so an unknown shape reads as off, never crashes the listener.
export interface PpBadgePayload {
  active?: boolean;
  template?: string | null;
}

// The chip's render state derived from one payload.
export interface PpBadgeState {
  active: boolean;
  template: string | null;
}

// Map one backend payload to the chip's state. Unknown or missing fields
// read as off/unnamed, and an off edge never names a template (even if a
// stray one rode along) - the render branches on truthy values only.
export function ppBadgeStateFromPayload(
  payload: PpBadgePayload | null | undefined,
): PpBadgeState {
  const active = Boolean(payload?.active);
  return {
    active,
    template: active ? (payload?.template ?? null) : null,
  };
}
