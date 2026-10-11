import assert from "node:assert/strict";
import { POST_PROCESS_BADGE_EVENT, ppBadgeStateFromPayload } from "./ppBadge";

// v1.6.0 W1-L7: the PP badge seam, pinned on this side of the Rust/TS
// boundary. The Rust side pins the same literals (POST_PROCESS_BADGE_EVENT
// and post_process_badge_payload in transcription_coordinator.rs), so a
// rename on either side fails a test instead of silently killing the chip.

// The event name matches the Rust constant byte for byte.
assert.equal(POST_PROCESS_BADGE_EVENT, "post-process-changed");

// The on edge with a named template lights the chip and names it.
assert.deepEqual(
  ppBadgeStateFromPayload({ active: true, template: "Meeting Notes" }),
  {
    active: true,
    template: "Meeting Notes",
  },
);

// The on edge with no selected template (backend sends null) lights the
// bare PP label; a missing field reads the same way.
assert.deepEqual(ppBadgeStateFromPayload({ active: true, template: null }), {
  active: true,
  template: null,
});
assert.deepEqual(ppBadgeStateFromPayload({ active: true }), {
  active: true,
  template: null,
});

// The off edge clears the chip - and never names a template, even if a
// stray one rode along.
assert.deepEqual(ppBadgeStateFromPayload({ active: false, template: null }), {
  active: false,
  template: null,
});
assert.deepEqual(
  ppBadgeStateFromPayload({ active: false, template: "Stale" }),
  { active: false, template: null },
  "an off edge must not name a template",
);

// Unknown shapes (null/undefined payload, missing fields) read as off, so
// a malformed event can never wedge the chip on.
assert.deepEqual(ppBadgeStateFromPayload(null), {
  active: false,
  template: null,
});
assert.deepEqual(ppBadgeStateFromPayload(undefined), {
  active: false,
  template: null,
});
assert.deepEqual(ppBadgeStateFromPayload({}), {
  active: false,
  template: null,
});

console.log("ppBadge seam tests passed");
