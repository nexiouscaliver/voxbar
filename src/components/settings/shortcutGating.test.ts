import assert from "node:assert/strict";
import {
  shortcutMasterToggleField,
  shortcutRowDisabled,
} from "./shortcutGating";
import type { AppSettings } from "@/bindings";

// KB-009: every gated row maps to the same master toggle the backend's
// binding_is_active checks (a mismatch would grey out a row whose key is
// still live, or leave an inert row's recorder armable).
assert.equal(
  shortcutMasterToggleField("delete_last_word"),
  "delete_last_word_enabled",
);
assert.equal(shortcutMasterToggleField("undo"), "undo_enabled");
assert.equal(
  shortcutMasterToggleField("transcribe_commands"),
  "command_mode_enabled",
);
assert.equal(
  shortcutMasterToggleField("transcribe_with_post_process"),
  "post_process_enabled",
);
assert.equal(
  shortcutMasterToggleField("cycle_post_process_prompt"),
  "post_process_enabled",
);

// The main transcribe key and the cancel key have no master toggle: their
// rows never disable.
assert.equal(shortcutMasterToggleField("transcribe"), null);
assert.equal(shortcutMasterToggleField("cancel"), null);

// A row is disabled exactly when its feature's toggle is off.
const settingsOn = {
  delete_last_word_enabled: true,
  undo_enabled: true,
  command_mode_enabled: true,
  post_process_enabled: true,
} as AppSettings;
const settingsOff = {
  delete_last_word_enabled: false,
  undo_enabled: false,
  command_mode_enabled: false,
  post_process_enabled: false,
} as AppSettings;

for (const id of [
  "delete_last_word",
  "undo",
  "transcribe_commands",
  "transcribe_with_post_process",
  "cycle_post_process_prompt",
]) {
  assert.equal(
    shortcutRowDisabled(id, settingsOn),
    false,
    `${id} must stay enabled when its toggle is on`,
  );
  assert.equal(
    shortcutRowDisabled(id, settingsOff),
    true,
    `${id} must disable when its toggle is off`,
  );
}

// Untoggled rows stay interactive whatever the toggles say.
for (const id of ["transcribe", "cancel"]) {
  assert.equal(shortcutRowDisabled(id, settingsOn), false);
  assert.equal(shortcutRowDisabled(id, settingsOff), false);
}

// While settings are still loading (null), nothing greys out - the
// recorder chip only renders once bindings are loaded anyway.
assert.equal(shortcutRowDisabled("delete_last_word", null), false);
assert.equal(shortcutRowDisabled("cycle_post_process_prompt", null), false);

console.log("shortcutGating: all assertions passed");
