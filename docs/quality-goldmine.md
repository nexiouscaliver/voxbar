# The quality goldmine

What three rounds of parallel feature review found in the v1.4.0 cycle, folded into one
knowledge base. Each round put twelve reviewers on every feature and every toggle in the app;
a consolidation pass deduplicated the findings, verified every fix claim against the code, and
carried the result into the next round so later rounds checked earlier claims instead of
re-finding them. Line numbers cite the tree at commit c8b0f890 on
cycle2/v1.4.0-postprocessing-and-companion; the tree moves, so treat every citation as a
starting point, not a promise.

How to use this file:

- Before picking up a bug or a stability task, search here first. Fixed items name their
  commits and their tests so you do not re-file what is already closed.
- The cross-cutting themes at the bottom are the highest-leverage entry points: each one names
  a fix pattern that would close five to twenty items at once.
- Items marked OPEN were left unfixed on purpose (cycle scope, not dismissal). Most were
  re-verified by at least one review round; the evidence tags say which.

Evidence tags: [R1], [R2], [R3] mean a reviewer of that round read or ran the evidence;
a checkmark on the tag means the consolidator repeated it in the final pass. "carried" means
nobody looked again in later rounds: verify before fixing.

Status at v1.4.0: no P0 open (the last one, KB-150, was fixed after round 3, see below).
The open backlog is 38 P1 and roughly 104 P2 items. Cycle 3 (audit-fix waves,
commits a9e1159e..a6997aec) has since closed 17 of them - 12 P1 and 5 P2 -
with the entries updated in place below; five are partial (KB-020/033/148/154/190)
and keep a residual line. (That 17 was a mid-cycle count covering waves 1-2
only; by cycle 3's end waves 1-6 closed 30 items - the round 4 standing
verdict said 29 and missed KB-112, corrected in the round 6 reconciliation
below.)

## P0: the one that got away, now fixed

**KB-150 · FIXED · switching the selected microphone while a recording was live silently killed and discarded that recording.** As found: `update_selected_device` (audio.rs:1129-1139 ✓R4) invalidated the cache and, when the stream was open (always true mid-recording), did `stop_microphone_stream()` + `start_microphone_stream()`: the stop **discarded the samples** (`let _ = rec.stop()` at audio.rs:887-894 [R3]) and zeroed `is_recording` while the state machine still said Recording; on hotkey release the empty buffer hit the silent skip branch "Recording produced no audio samples; skipping persistence" (actions.rs:1433-1439 [R3]); overlay hidden, tray Idle, zero feedback; streaming sessions lost everything after the switch. The sibling `update_selected_channel` locked state and rejected exactly this with a comment explaining why [R3]: the asymmetry was visible in the same read ✓R4. UI unguarded: MicrophoneSelector.tsx:60-64 disables only for isUpdating/isLoading/empty list [R3]. No test coverage at the time (R3 ran the full audio-module filter → 47 passed, none on this path).

Fixed after round 3 in commit c8b0f890 ("fix(review round 3): switching the microphone mid-recording silently discarded the dictation"): `update_selected_device` now takes the state lock and rejects the switch while Recording or Stopping through the shared `capture_restart_forbidden` predicate (managers/audio.rs:1152), the same rule as `update_selected_channel`; the predicate is pinned by test over all three recording states (managers/audio.rs:1457-1467). Verified in code for this document.

No P0 remains open at v1.4.0.

---

## Fixed and verified in code

- **KB-002 · FIXED · cancel leaks the forced system mute.** `cancel_recording` now calls `self.remove_mute()` in **both** the Recording and Stopping branches, with a comment naming KB-002 (audio.rs:1319-1325, 1353-1360 ✓R4); covers the local always-on path, the remote branch (close_remote_stream has no restore), and the lazy-close deferral. R3 ran `cargo test --lib mute_restore` → 1 passed, `managers::audio` → 3 passed. **Follow-up: KB-151** (the quit path still strands the mute) - closed in cycle 3: a9e1159e makes `RunEvent::Exit` call `remove_mute()`.
- **KB-019 · FIXED · local post-process gate reads the SELECTED model.** `selected_llm_model_id(app)` now feeds the gate, with a comment naming KB-019 (actions.rs:859-871 ✓R4); same resolution as the swap runner. R3 ran `local_availability_follows_the_selected_model(_not_the_pinned_one)` → 1 passed; actions::tests 27; local_llm::manager 24. Do not re-report.
- **KB-103 · FIXED · always_on boot panic + poisoned setting.** Constructor's AlwaysOn open is best-effort (`if let Err(e) = … error!`, audio.rs:556-572 ✓R4); `update_microphone_mode` applies the runtime flip (on spawn_blocking) BEFORE persisting, with the rationale comment (commands/audio.rs:181-208 ✓R4); the store updater throws on error so the toggle rolls back (settingsStore.ts:106-114 ✓R4). **Follow-up: KB-104 residual + KB-179** (same boot-panic class, history side).
- **KB-104 · FIXED · both setters now apply before persisting (device residual closed in cycle 3, fcb3e742).** `update_microphone_mode` is fixed (above); the device setter no longer persists-first either: `set_selected_microphone` (commands/audio.rs:246-273) applies through `update_selected_device` BEFORE anything is written and propagates its error, and `update_selected_device` (managers/audio.rs:1186-1226) rejects the switch while a recording is live, persists only after the reopen resolves its device from settings, and on a failed reopen undoes the write and brings the previous device's stream back (the KB-104 comment block). Round 4 verified this fix (its list names "KB-104 residual") but the entry kept the stale residual sentence until the round 6 pass flipped it.
- **KB-005 · still FIXED, but its guard test is unwired:** historyLimitInput.test.ts registers 0 tests under `bun test`, no package.json script, not in CI (R3 ran both invocations); third instance of the KB-136 class.

Prior FIXED (unchanged, do not re-report): KB-001, KB-003, KB-004, KB-006, KB-007, KB-041-core, KB-096 (residual closed in v1.6.2, 6b054cd1: the version is injected at build time from package.json and the fallback literal is pinned to it, src/lib/utils/appVersion.ts).

**Narrowed in the final round:** KB-011 residual (no-model path only: re-verified [R3 ×2]); KB-013 residuals (show wipes notice + Info-tone fallback: re-verified [R3 ×2]); KB-083 residual folded into KB-085 [R3]; KB-025 **correction** (see below).

---

## P1, MEDIUM, OPEN (38 at v1.4.0 - 12 since fixed in cycle 3, entries updated in place)

**KB-008 · FIXED (371f2b41, roster pinned 6670025b; round 4 verified, round 9 fleet re-verified).** Was: post-process toggle off unregistered only `transcribe_with_post_process`; cycle key stayed registered + system-swallowed. Now binding_is_active gates `cycle_post_process_prompt` and the toggle drives both keys through POST_PROCESS_TOGGLE_BINDINGS. Do not re-report.

**KB-009 · FIXED (backend 371f2b41, frontend ab25b849; round 4 verified, round 9 fleet re-verified).** Was: `change_binding` registered without `binding_is_active`; disabled was opacity-only. Now the command registers only post-update-active bindings, restore_registration is gated the same way, and the recorder chips hard-guard arming.

**KB-010 · FIXED (v1.6.2, fd63e677).** Was: no lid watcher; AlwaysOn never switched on lid close; `set_clamshell_microphone` never restarted. Now an IOKit lid watcher (kIOGeneralInterest on IOPMrootDomain, runloop thread + deduped offload) re-resolves the desired microphone on every real lid transition with the guarded restart, a mid-recording flip parks a pending flag the stop and cancel pipelines honor, and the setter applies before persisting. Pinned by kb010_lid_watcher_registers_an_iokit_interest_and_dedups plus a runtime-probe test.

**KB-012 · FIXED (script half dbd2e1b8; the CI line landed in v1.6.2 fd63e677, ci.yml:190).** The hi/hi-Latn script-variant family rule passes read-only runs, and `check:model-languages` now runs in CI beside check:translations. Round 9 fleet re-verified both halves.

**KB-014 · FIXED (v1.6.2, 58a920ff).** Preview burns the full 1.2 s with overlay_style=none (actions.rs:1655-1686 gates only on the toggle; show_final_preview_overlay no-ops on None, overlay.rs:644-648). New copy angle: the row renders in the Output tab far from the overlay-style control; OFF is what the copy already promises [R3]. Fixed: the preview wait is skipped when overlay_style is None (both preview branches are no-ops there), pinned by kb014_preview_wait_runs_only_when_something_can_show and the stop-pipeline twin.

**KB-015 · FIXED (v1.6.2, 6b054cd1, with the KB-122 umbrella).** Was: model_load_failed interpolates raw English into the {{model}} slot; full memory-gate refusal sentence lands in the slot (transcription.rs:1953-1984, 234-268 [R3]). Fixed: the memory-gate refusal gets its own notice code with a structured model/needed/free detail, and the raw-English toast descriptions (transcription-error, model-state-changed) show localized copy. Pinned by memory_gate_notice_detail_carries_model_needed_free_kb015_kb122.

**KB-016 · FIXED · companion codes reach the main window.** As found [R3]: companion codes had no main-window surface; `companion_session_capped` had **no NoticeCode variant at all** (enum transcription.rs:394-459); broadcast to phones only; disconnected is Info (no sound). Fixed in cycle 3: d540241b adds the `CompanionSessionCapped` NoticeCode (Info, detail = device name) and emits it Mac-side from the server cap site; a6997aec routes it through the App.tsx notice router (toast, plus a macOS notification when the window is hidden and the card could not render) with `overlay.notice.companionSessionCapped` copy in all 26 locales; the shared code→message mapping is pinned over the full enum by src/lib/noticeMessage.test.ts. Device-name detail on disconnect remains KB-163 (open).

**KB-017 · FIXED (05ef0cf9, the KB-223 fix; round 9 fleet).** A failed reliable-paste chord now settles the waiter, restores the clipboard, and surfaces as a real paste failure (stable prefix, PasteFailed notice) on both platforms; the auto-submit Enter is a pinned not-owed after chord failure rather than a silent drop.

**KB-018 · FIXED (v1.6.2, fd63e677).** Was (static, all rounds: no dotool on this host): multi-line transcript written as one `type` line; later lines execute as dotool commands (clipboard.rs:691-692). Fixed: multi-line transcripts reach dotool as per-line type commands plus an explicit key enter per newline (dotool_script_for_text), pinned as a pure function by dotool_script_types_each_line_and_presses_enter_per_newline.

**KB-020 · PARTIALLY FIXED · card-less notices now have surfaces.** As found [R3]: Info-tone notices fired while the card is hidden were invisible on every surface; cycle-prompt was the worst case (pressed between sessions by design) (transcription.rs:458-476, 543-562; RecordingOverlay.tsx:441). Fixed in cycle 3: d540241b computes `card_visible` at emit time on OverlayNoticeEvent; a6997aec turns the App.tsx listener into a router - card-less notices toast (error tone for Error kind) and fire a macOS notification when the main window is hidden - via the shared mapping in src/lib/noticeMessage.ts, pinned over the full enum. Residual, three info codes routed hidden-only: kept. Residual, the fade-window TOCTOU (`card_visible` probed at emit time): CLOSED in v1.6.2 fd63e677 - notices now probe the show and hide generations (card_durably_visible, pending-hide marker) instead of raw is_visible.

**KB-021 · FIXED (v1.6.2, 58a920ff).** Was (all 26 locales re-swept by R3 python script: start_over=True, scratch_everything=False everywhere). Fixed: every locale's voice-deletion description advertises the live scratch everything phrase instead of the dead start over, cross-pinned against the recognizer's ClearAll defaults.

**KB-022 · FIXED (v1.6.2, 8b94fdc1).** Was: commands-tab add/remove before defaults resolve (or after silent `.catch(() => {})`) persists ~29 zero-phrase commands (CommandsSettings.tsx:212-251; backend accepts, command_matrix.rs:378-424) [R3]; resetRow IS guarded: the odd one out. Fixed: the Commands tab passes the loaded defaults into every add, remove and undo table build and disables add/remove while defaults are unresolved.

**KB-023 · FIXED, with KB-240 (v1.6.2, 8b94fdc1).** Was, sharpened: the retry refresh is HALF-done - `update_transcription` rewrites post_process_prompt but none of the five summary columns, so one row half-reports two different runs (managers/history.rs:404-418 [R3]). Fixed: update_transcription rewrites the five pp summary columns atomically with the text and NULLs them when the leg did not run; the retry command threads the fresh summary it used to drop.

**KB-025 · FIXED (v1.6.2, 8b94fdc1).** Was + CORRECTION: cleanup emits no event (ghost rows; save-before-emit order at history.rs:296-307 [R3]); the in-tab HistoryLimit/retention controls make ghost rows manifest inside the same view; the R1/R2 "pinned by test" claim was wrong (no cleanup test existed). Fixed: delete_entries_and_files emits per-id Deleted events (the retention cleanups included), the History tab listener handles deleted by filtering the id, and the previously missing cleanup tests exist (count and time selection seams).

**KB-027 · FIXED · companion toggle applies before persisting and rolls back.** As found [R3]: `change_companion_devices_setting` persisted+Ok'd unconditionally (mod.rs:845-851) while the always_on updater directly above (landed in 5bf5b893) threw and rolled back. Fixed in a9e1159e: `companion::apply_enabled` returns Err on a failed start (logged, stored on the manager, noticed) and the command applies it BEFORE persisting, so the settings store rolls the toggle back instead of persisting an enabled state with no server behind it; the startup path stays best-effort.

**KB-028 · FIXED (v1.6.5 residuals, lane R1).** Was: disable-during-local-recording leaked the pre-warmed remote recorder; stop() skipped the close while anything recorded and nothing re-triggered it after the session ended (mod.rs:285-289 at find time; prewarm refuses only while live, audio.rs:1049-1056) [R3]. Fixed: the teardown close routes through `AudioRecordingManager::close_remote_stream_or_defer` - it parks the skipped close in `remote_close_pending` and every session-end fold (local stop, local cancel, lane stop, lane cancel) runs it once nothing records anywhere, so a disable during a LOCAL recording no longer strands the pre-warmed idle recorders (companion/mod.rs:758-763). Pinned by kb028_stop_defers_the_lane_recorder_close_instead_of_silently_skipping_it (companion tests) and kb028_the_lane_recorder_close_parks_and_runs_at_every_session_end (managers/audio.rs tests, which also pins the four fold sites).

**KB-029 · FIXED (v1.6.5 residuals, lane R1).** Was, sharper: reset-pairing failures console.error only AND the restart leg calls manager.start() directly so status.error was never set: the panel rendered a clean "Stopped" [R3]. Fixed: the `reset_companion_pairing` command routes failures through the exact surface `restart_companion_server` uses - `set_last_error` (the panel's server-error line) plus the CompanionServerFailed notice, cleared on success - and the panel's resetPairing handler checks `result.status === "error"` (the bindings resolve a Rust Err as a result, never a throw) and toasts with the shared settingNotSaved key (shortcut/mod.rs + CompanionDevices.tsx). Pinned by kb029_reset_pairing_failures_surface_on_the_restart_error_path (shortcut tests, comparing both commands' arms) and companionResetPairingPins.test.ts (executes the committed handler against all three resolutions).

**KB-031 · FIXED · change_theme_setting syncs the tray.** As found [R3]: the theme setter never rebuilt the tray, so the menu bar kept the previous theme's icon (mod.rs:940-959). Fixed in a9e1159e: `change_theme_setting` calls `tray::update_tray_menu` after `apply_window_theme` (which the icon lookup reads the window theme from), so the tray re-syncs on every theme change.

**KB-032 · FIXED (v1.6.2, 58a920ff).** Was (impact-sized): six text-white-on-accent sites; accent.ts ships light fills that made white labels near-invisible. Fixed: the six sites use text-accent-on, the pair that flips with a non-default accent, pinned by accent.test.ts.

**KB-033 · PARTIALLY FIXED · update checks fail closed while the lock is unknown.** As found [R3]: About button ungated, silent no-op (AboutSettings.tsx:49-55; updaterFlow.ts:332 exits before reveal :364); `updateChecksAllowed` failed OPEN while `updateChecksLocked` was still null - a locked install could run a real network check during the loading window. Fixed in 14369da8: the gate now fails CLOSED (`updateChecksLocked !== false` → not allowed) and the About check button mirrors it exactly (disabled while not known-allowed, locked/disabled tooltip). Residual: the inFlight drop and the tray lock-null window are now handled as well (the update-check gate module, this cycle) - recheck under AUD-10.

**KB-034 · FIXED · update_checks toggle rebuilds the tray.** As found [R3 ×2]: the toggle never re-synced the tray's "Check for Updates" enablement (mod.rs:1161-1181; en description promises exactly the tray behavior). Fixed in a9e1159e: `change_update_checks_setting` calls `tray::update_tray_menu` right after persisting, so the tray reflects the toggle immediately instead of after the next restart.

**KB-035 · FIXED (by v1.6.0; round 9 fleet).** Release notes ship through per-version files (up to 1.6.0.md at the time of verification) and the blank last-seen migration resolves to the newest applicable note, so pre-key migrants see the current release's notes, not stale ones.

**KB-036 · FIXED (371f2b41; round 4 verified, round 9 fleet re-verified).** Was: keyboard-impl switch mid-recording deterministically killed the cancel key; the promised reconcile was never scheduled. Now the switch rearms the live cancel key at both success exits and the KB-199 error exit.

**KB-045 · FIXED, with KB-047 (v1.6.2, 8b94fdc1).** Was, sharpened: no concurrent-start guard on either download path; cancel_download always returns Ok and clears is_downloading even when no token exists; a third click starts a third transfer [R3]. Fixed: both paths claim a per-model slot atomically (duplicate starts no-op instead of overwriting the live token) and cancel with no token is a no-op (no state clear, no cancelled event).

**KB-047 · FIXED, with KB-045 (v1.6.2, 8b94fdc1).** Was: DownloadCleanup::drop removes the token without ownership check (model.rs:603-616) [R3 ×2]. Fixed: the DownloadCleanup guard removes only its own token.

**KB-105 · FIXED (v1.6.2, 58a920ff).** Was (mechanics fully pinned by R3): delayed hide-after-error (2.6 s) hides the NEXT session's card: hide snapshots generation at CALL time (after the sleep), so a show during the window is overwritten; armed at four sites. Fixed: hide-after-error snapshots the show generation at ERROR time, before the read delay, pinned by kb105_error_hide_snapshots_the_generation_before_the_read_delay.

**KB-106 · FIXED (754c0fc5, v1.6.0's notification lane; round 9 fleet).** paste_failed now routes through the notice channel to a persistent OS notification regardless of window or card visibility.

**KB-107 · FIXED · api-key change drops the stale cached model list.** As found [R3]: the api-key handler re-hydrated the OLD key's cached model list (`[]` treated as absent; base-URL handler clears the cache, api-key handler didn't: mod.rs:1487-1499 contrast :1441-1470). Fixed in a9e1159e: `change_post_process_api_key_setting` removes the provider's `post_process_model_lists` entry before persisting, so the dropdown refetches with the new key.

**KB-108 · FIXED (v1.6.2, 8b94fdc1).** Was (re-verified 2×): built-in template delete not durable: seed loop pushes missing ids on every read, resurrecting at the END of the catalog after selection was reassigned [R3]. Fixed: deleting a built-in records a persisted tombstone the seed loop honors (user templates tombstone nothing), pinned by deleted_builtin_prompt_tombstones_and_never_resurrects and the tombstone round-trip tests.

**KB-109 · FIXED (master-toggle half b746d17a, cycle 3; the provider-pinning half v1.6.2, 8b94fdc1).** Was (re-verified 3×): retry ignores the post-process master toggle and sends the stored transcript to the CURRENT provider/model/prompt. Fixed: the retry's post-process leg resolves provider, model and prompt from the entry's recorded summary first (a model_override pin beside the provider_override), falling back to current settings only for fields the entry never recorded; a recorded prompt that no longer exists skips the leg with a notice.

**KB-110 · FIXED by construction (v1.6.2, f2014843).** Was: second phone's press contaminates/cuts the shared session (per-connection trackers press unconditionally; one shared ring; shared binding) [R3]. Fixed: each authenticated connection owns its recorder, ring, coordinator session, delivery target and recents; see round 9.

**KB-111 · FIXED by construction (v1.6.2, f2014843).** Was: either phone dropping finalizes the shared session while the other streams. Fixed: per-lane sessions stop independently; one phone's stop or disconnect never touches the other's session (two_lanes_each_hold_a_live_session_with_independent_stops).

**KB-112 · FIXED · autostart failures propagate and roll the toggle back (cycle 3 wave 4, fcb3e742).** Was [R3]: `apply_autostart` returned (), failures were warn-only, the command Ok'd unconditionally. Fixed: `apply_autostart` returns `Result` and its doc comment names KB-112 (autostart.rs:25-46); `change_autostart_setting` applies the login item BEFORE persisting and propagates failures so the settings store rolls the toggle back (shortcut/mod.rs:1313-1331); the startup call stays deliberately best-effort (lib.rs:510-513, the companion-init rule: the preference is re-applied every launch so a transient failure self-heals). Missed by the round 4 verified list although it shipped in the same wave 4 commit as KB-104/145/183/188; corrected in the round 6 pass (which also adjusts the round 4 closed count from 29 to 30).

**KB-113 · FIXED (obsolete; round 4 ruled closure by removal, round 9 fleet re-confirmed).** No loading toast exists on any model-download path anymore, so the stuck-toast defect has no trigger surface.

**KB-144 · FIXED (v1.6.2, 58a920ff).** Was, upgraded to medium: search indexes experimental-gated rows (jump targets don't render) AND Companion Devices has no search entry at all [R3 ×3]. Fixed: the experimental-only entries carry requiresExperimental and hide while the flag is off, and Companion Devices joins the index gated the same way. Residual (KB-244): the post-processing row keys remain unindexed.

**KB-148 · PARTIALLY FIXED · update-flow prompts notify while the window is hidden.** As found [R3]: the auto-check 'ask' card + install-policy restart prompts lived only in the hidden window; no reveal, no tray badge. Fixed in a6997aec: the ask card, the download/install failure toast and the restart prompt each also fire a macOS notification (`notifyDesktop`) when `document.visibilityState !== "visible"`. Residual: the auto-check-failure path - runUpdateCheck's catch toasts only when `trigger === "manual"`, so a failed AUTO check stays console-only with the window hidden (addressed separately by this cycle's check-failure notify work).

**KB-151 · FIXED · exit restores the system mute.** Was [R3]: quitting during an active mute_while_recording session stranded the system-wide mute - `RunEvent::Exit` did tray::stop_ram_refresh + companion::shutdown only, no `remove_mute` (lib.rs:1202-1208 ✓R4); the tray Quit path reaches it via app.exit(0). Same user impact as KB-002 via a narrower window. Fixed in a9e1159e: Exit now calls `recording.remove_mute()` (best-effort; warn-only when the manager is gone) before the shutdown legs.

**KB-154 · PARTIALLY FIXED · arch allowlists are disjoint again.** Was [R3]: "granite" sat in BOTH KNOWN_ARCHES and LLM_ARCHES, breaking the documented disjointness both add-from-HF gates rely on (model_capabilities.rs:43 and :61 ✓R4): a granite LLM text GGUF registers as an ASR model; a granite ASR GGUF passes the post-process gate. Fixed in a9e1159e: bare `granite` removed from KNOWN_ARCHES (llama.cpp's LLM arch lives in LLM_ARCHES alone; transcribe-cpp's granite ASR family is the suffixed variants), with disjointness now pinned by tests in model_capabilities.rs and catalog/llm.rs. Residual: granite_nar/cohere arch-string mismatches remain (pre-existing catalog-metadata drift, not introduced by the fix).

**KB-155 · FIXED (v1.6.5 residuals cycle, lane R2) · model delete and download-cancel failures are invisible in the settings UI.** Was [R3]: both handlers console.error'd only and ignored the store's recorded error (modelStore sets error + returns false), so a swap-guard refusal or fs error left the user with zero feedback after confirming a native dialog. Fixed: every failure resolution routes through the row alert - the delete handler's non-refusal branch and its transport catch render settings.models.deleteFailed, and the cancel handler (which had ignored cancelDownload's false result entirely) reads the store error and renders settings.models.cancelFailed, both interpolating {{error}} across all 26 locales (ModelsSettings.tsx:174-256); the live-session refusal keeps its dedicated KB-117/220 copy. Pinned by modelActionErrorPins.test.ts, which executes the committed handler bodies against stubbed false-result/refusal/rejected/ok resolutions (mutation-checked both halves: delete back to console-only and cancel back to result-ignoring each trip it).

**KB-159 · FIXED (371f2b41; round 4 verified via KB-194's twin, round 9 fleet re-verified).** Rebinding cancel onto a chord another active binding holds is rejected up front by binding_conflicts_with_active_binding, in both the cancel and the generic branch.

**KB-160 · FIXED (backend 371f2b41, frontend ab25b849, static pins b2e36c72; round 9 fleet).** Closing the settings window while a recorder is armed now emits main-window-hidden exactly once and both recorders cancel and release the binding suspension.

**KB-162 · FIXED · the batch final preview keeps the session's tail notice.** Was [R3]: the show listener's reset covered 'preview' (setNotice(null) + noticeTimer cleared), so a post-process skip emitted during transcribing was cut off before its designed 5 s read window - an error-sound skip (memory gate) showed nothing on the card exactly when the user was looking at the previewed text; the streaming branch kept the notice. Fixed in a6997aec: the reset now skips the notice (and its timer) when `overlayState === "preview"`; fresh sessions (recording/streaming) still start notice-clean and hide-overlay still clears it.

**KB-176 · FIXED (v1.6.2, 6b054cd1).** Was: the whisper initial prompt is unbounded and puts custom words at the HEAD, so HF's tail-keeping truncation sheds custom words first for every non-Latin output language. Fixed: the prompt is bounded and custom words ride the tail, pinned by whisper_initial_prompt_bounds_and_keeps_custom_words_last_kb176.

**KB-179 · FIXED (v1.6.2, 8b94fdc1, operator decision D3).** Was: a corrupted or unwritable history.db panics the app at every launch (`HistoryManager::new(...).expect(...)` in setup) [R3]. Fixed: new() quarantines the unreadable file as history.db.corrupt (timestamped suffix when taken, stale rollback journal dropped), starts a fresh database at the same path, and records the recovery; the frontend posts one persistent notice naming the quarantined file (settings.history.dbRecoveredNotice across all locales). Pinned by corrupt_db_is_quarantined_and_a_fresh_one_starts, second_quarantine_takes_the_timestamped_suffix, quarantine_drops_the_stale_rollback_journal, init_failure_quarantines_and_retries_instead_of_failing.

**KB-183 · FIXED · Experimental off is now a companion kill switch (cycle 3 wave 4 fcb3e742; hardened in the v1.5.4 cycle 69a7820c + da66553a).** Was [R3]: the LAN TLS server kept running with its only control unmounted (AdvancedSettings gates rendering only; companion::init armed purely on companion_devices_enabled), re-armed every boot, findable by neither search nor tray. Fixed: `effective_enabled` requires BOTH toggles with a 4-combination test (companion/mod.rs:1397 and the test at :2868, fleet pass 2 re-read); `init` arms on the conjunction (mod.rs:1382); the toggle command applies the conjunction, never the raw flag - including for direct IPC callers the UI unmount cannot gate (shortcut/mod.rs:955-990, `companion_toggle_runtime_value`, this cycle); the Experimental switch itself stops the server (shortcut/mod.rs:1689). The persist re-reads settings AFTER apply so the pairing token `start()` regenerates mid-enable survives the full-store write (da66553a). Round 4's list already verified the wave 4 half; the entry above was the stale part.

**KB-185 · FIXED · tray "Unload After → Custom…" switches mode before focusing.** Was [R3]: a silent no-op whenever the setting was a preset - the custom input rendered only when the stored value was already Custom (`customActive = isCustom(storedValue)` ✓R4 at ModelUnloadTimeout.tsx:41), so the promised focus landed on a null ref and nothing switched modes. Fixed in 14369da8: the tray-focus effect first switches the stored setting to custom (backend command + optimistic store update, seeding 90 s like the dropdown does) when a preset is stored, so the field has rendered by the time the await resolves and the ref is attached.

**KB-186 · FIXED (0b0e3323; round 4's bracketed correction already said so, round 9 fleet re-verified).** The onboarding model step renders an error card with Retry and Skip when the catalog fetch fails; the bundled-catalog caveat (an IPC-failure surface, not an offline one) stands.

---

## P2, LOW / OPPORTUNITY / COHESION, OPEN

_Re-verified in round 3 (tag `[R3]`) unless marked `carried`:_

- **KB-024** history copies raw only vs tray/paste processed [R3]. **KB-026** limit no-op unless PreserveLimit + star exemption undocumented (de/zh/hi/ja spot-checked) [R3]. **KB-029** see P1. **KB-030** companion_port dead control [R3].
- **KB-037** raw backend English in toasts (25 locales); _carried_ (referenced by R3's KB-039 evidence; not directly re-read). **KB-038 · FIXED · command-mode no-session rides the notice channel.** Was a bare `command-mode-no-session` emit only the main window heard (re-verified ×2 [R3]); d540241b migrates it to `emit_overlay_notice(CommandModeNoSession)` - log + event + card visibility like every other no-session feedback - and a6997aec's router toasts/notifies it card-less, reusing the existing `app.commandNoSession` copy. **KB-039** reset toast drops the list [R3].
- **KB-040** custom sound theme silent; error-cue Stop fallback also unchecked (audio*feedback.rs full read [R3]). **KB-042** preview tooltip literal [R3]. **KB-043** 0.5 vs 1.0 flash [R3]. **KB-044** is_downloading wrongly cleared: \_carried* (R2-narrowed). **KB-046** 16 #[ignore] supervisor tests (R3 re-ran: 11 passed/16 ignored) [R3].
- **KB-048** word-correction f64 unclamped; consumed raw where a huge threshold rewrites ordinary words (mod.rs:1236-1241) [R3]. **KB-049** AccelerationSelector display mismatch [R3]. **KB-050** style→none never hides live card + frontend reconcile handles only language/placement [R3]. **KB-051** streaming preview indistinguishable from working: exact mechanism pinned (only two emit_stream_working sites; caret suppression) [R3]. **KB-052** arming affordances + aria 'cancel' (localized Cancel keys exist to lift) [R3]. **KB-053** Windows preview-size misclassification (static, all rounds) [R3]. **KB-054** dropdown flashes 'live' [R3].
- **KB-055** stale debug-gated claim [R3]. **KB-056** delay sliders inert with reliable paste [R3]. **KB-057** auto-submit × paste-method None [R3]. **KB-058** Dropdown English placeholder: instances now also: stale custom sound theme (SoundPicker builds Custom only when wavs exist) [R3]. **KB-059** send_edit_action dead + still-false doc (grep exit=1) [R3]. **KB-060** Linux probe forking (static) [R3]. **KB-061** generic paste toast + enigo init console.warn only [R3]. **KB-062** main-thread paste sleeps >10 s worst case [R3]. **KB-063** TypingTool probe staleness [R3].
- **KB-064** global timeout dead + wired command [R3]. **KB-065** verdict detail English (26 locales) [R3]. **KB-066** tray staleness: now THREE surfaces: prompt submenu (all five settings-side mutations silent), local-LLM submenu (post-process-model-changed has zero listeners), AND voice Models submenu (download-complete/model-deleted never rebuild; only model-state-changed does: lib.rs:468-478) [R3 ×4]. **KB-067** runs table binding column: _carried_. **KB-068** renamed built-in masked [R3]. **KB-069** reseed drops NAME; no rename-only test case (re-ran, confirms gap) [R3]. **KB-070** evicted-live run loses outcome line + starts leak [R3]. **KB-071** ${output} never validated; legacy path sends no transcript [R3]. **KB-072** timestamp-ms ids + false comment + unguarded Duplicate [R3]. **KB-073** versionLabel dead key (re-grepped: 26 locale files, 0 refs) [R3 ×2].
- **KB-074** spoken undo/paste inert vs copy in 26 locales [R3]. **KB-075** mid-session snapshot vs fresh finalize (fresh line evidence) [R3]. **KB-076 · FIXED (v1.6.2, 6b054cd1)** (was: ASCII terminal punctuation on CJK/Hindi; the marks are script aware now, with KB-135/KB-178). **KB-077** filler \b never fires between Han [R3: bites only once a writer exists, cf KB-078]. **KB-078** custom_filler_words dead setting [R3]. **KB-079** lazy retention, three call sites [R3].
- **KB-080 · FIXED (v1.6.2, 58a920ff):** was (upgraded to medium [R3]) silent play of missing WAV, reachable in one step from the "Open Recordings Folder" button; custom cues now resolve through an existence check that falls back to the default theme's same cue. **KB-081 · FIXED (v1.6.2, 58a920ff)** (was: star toggle race + blind revert; now per-row pending, revert-to-pre-click, failure toast). **KB-082** cert SAN freezes first LAN IP (load_persisted ignores current IP, cert.rs:31-33) [R3; round 9 fleet re-verified open]. **KB-085 residual** rapid off/on port race (pre-auth deadline half fixed in 9655d8fc; KB-083 residual folded here) [R3; round 9 fleet re-verified]. **KB-086 residual** two dead protocol fields (the zero-tests half closed in v1.6.2: companion/server.rs:952 now carries a tests module; fleet pass 2 re-read). **KB-087 · FIXED (v1.6.2, 58a920ff)** (was: copy-last silent on all paths; both tray copy actions now report every terminal path). **KB-088 · FIXED (v1.6.2, 58a920ff)** (was: debug-key auto-repeat; the chord ignores repeats). **KB-089 · FIXED (82e8700d)** ghost Debug section (sharpened: neither renderer checks debug_mode) [R3; round 9 fleet re-verified]. **KB-090 · FIXED (v1.6.2, 58a920ff)** (was: dark: follows OS + log-surface override media-query-only; a custom dark variant now follows the app theme). **KB-091 · FIXED (v1.6.2, 58a920ff)** (was: verify-state mislabel + light colors + no feedback on still-denied; Check Again answers current state, paired emerald shades). **KB-092 · FIXED (v1.6.2, 58a920ff)** (was: AppData+LogDirectory light colors; skeletons and errors on tokens). **KB-093 · FIXED (v1.6.2, 58a920ff)** (was: no ErrorBoundary on settings tree; the scroll content sits inside one).
- **KB-094** failed install still shows "installed" prompt [R3 ×2]. **KB-095** confirm-card stacking [R3]. **KB-097** attribution literals [R3]. **KB-098** tray empty-string hole (re-measured clean today; gate counts "" as present) [R3 ×2]. **KB-099** translocation toast: mount-only, 15 s, expires unseen for start_hidden; comment's promise wrong; English race [R3 ×2]. **KB-100 · CLOSED (test:updater half 2bdd6049 per round 6 item 5; check:text-style half v1.6.2 fd63e677, ci.yml:187; round 9 fleet re-verified both).** **KB-101** up-to-date path logs nothing [R3]. **KB-149** text-style omits release-notes + tsx literals (ran clean today) [R3].
- **KB-114** command-mode latch (re-verified ×2) [R3]. **KB-115** unclamped u64 family: now also paste*delay_ms/paste_delay_after_ms (five setters total; R3 re-read all bodies) [R3 ×2]. **KB-116** margin no upper bound [R3]. **KB-117** delete_model no in-flight guard, both layers (re-verified ×2) [R3]. **KB-118** keyboard-impl dropdown literals: instance family grows: **LogLevelSelector.tsx:8-13 same class** [R3]. **KB-119** SoundPicker no disabled prop [R3]. **KB-120** per-drag full-store writes: family now: volume + **all five Output-tab sliders + the two new post-process sliders (timeout/keep-warm) + word-correction slider** [R3 ×3]. **KB-121** position/style/language no settings-changed emit → inverted card (fix shape: one emit per command) [R3]. **KB-122 · FIXED (v1.6.2, 6b054cd1, with KB-015)** (was: raw-English interpolation umbrella: instances re-confirmed across overlay notices, channel toast, companion panel+notice, verdicts; the memory-gate refusal and the raw toast descriptions now localize). **KB-123** Shift+F4 on macOS + setter accepts hidden methods [R3]. **KB-124** toggle-off no-op no feedback (re-verified ×2) [R3]. **KB-125** external script unvalidated [R3]. **KB-126** Windows auto-submit try_lock skip: \_carried*. **KB-127** final clipboard-write failure inverts signal [R3]. **KB-128** isNotAPostProcessModelError dead [R3]. **KB-129** allow_base_url_edit decorative (three hardcoded gates) [R3]. **KB-130** off-path grid flash [R3]. **KB-131** Apple chars_out bytes (changed_ratio ~3× for CJK) [R3 ×2]. **KB-132** prompt_test consumes toast budgets: extended: also the skip-reason budget AND arms the overlay "Polishing" chip during a Test [R3 ×2]. **KB-133** shared test state/panel [R3 ×2]. **KB-134** everyday-gate English-only [R3]. **KB-135 · FIXED (v1.6.2, 6b054cd1)** (was: CJK ASCII punctuation pinned by test; terminal punctuation is script aware now, with KB-076/KB-178). **KB-136** assert-script tests never run in CI: now THREE instances: commandGroups, skipToastDedupe, historyLimitInput (R3 ran both invocations for two of them) [R3 ×2]. **KB-137** `|| 5` coercion [R3]. **KB-138** AudioPlayer aria [R3]. **KB-139** retry toast drops detail [R3]. **KB-140** no-confirm delete + page-1 reload [R3]. **KB-141** stale synthesized-release comments (one now factually wrong about stop()) [R3]. **KB-142** sync-command main-thread block: second instance: **change_keyboard_implementation_setting, up to 10 s on first handy_keys switch** [R3]. **KB-143** tray submenu sync rejections log-only [R3]. **KB-145** experimental-off hides but doesn't deactivate [R3]. **KB-146** degenerate submenu label [R3 ×2]. **KB-147** sidebar mouse-only [R3].

_New in round 3 (KB-152, 153, 156-158, 161, 163-175, 177, 178, 180-182, 184, 187-193):_

- **KB-152 ·** failed channel change in AlwaysOn leaves the stream closed until the next press (rollback restores the channel value, not the stream; contrast VAD rollback which restarts) (audio.rs:1164-1171) [R3].
- **KB-153 ·** play_test_sound is an async command that blocks a worker for the full sound + device enumeration, violating the spawn_blocking convention documented beside it (commands/audio.rs:322-334) [R3].
- **KB-156 ·** cancelling during post-download sha256 verify stops nothing (hash loop never observes the token) yet the UI reports cancelled; the verify finishes, model marked downloaded, auto-select can switch models after the user cancelled (download.rs:88-116; model.rs:3006-3038) [R3].
- **KB-157 ·** Footer ModelSelector auto-selects on every model-download-complete without engine filtering; LLM downloads funnel through the shared emit → guaranteed-refused setActiveModel + LLM-name flash + surfaced-nowhere error (ModelSelector.tsx:98-119) [R3].
- **KB-158 · FIXED.** Was: getAvailableAccelerators had .then but no .catch and the backend .expected on JoinError → any rejection permanently emptied both accelerator dropdowns (AccelerationSelector.tsx:65; mod.rs:2285-2295) [R3]. Fixed across a9e1159e (backend: the spawn_blocking JoinError is answered with empty lists + error! instead of .expect) and 14369da8 (frontend: the probe promise gets a .catch; the dropdowns keep their empty fallback).
- **KB-161 ·** lone-modifier transcribe binding under handy_keys: hold measured from the gate-fired press (400 ms activation gate); a 600 ms hold with a 300 ms threshold classifies as a tap-lock; sub-400 ms taps never activate (handy_keys.rs:24-42, 264; coordinator :514-536) [R3].
- **KB-163 ·** companion_disconnected notice drops its device-name detail: the only companion mapping that ignores notice.detail, so with multiple phones the user can't tell which dropped (RecordingOverlay.tsx:111-112 vs transcription.rs:452-455) [R3 ×2].
- **KB-164 ·** polishing chip uses locale-invariant '1.8s' decimal in every locale; Intl pattern exists in dateFormat.ts (RecordingOverlay.tsx:590, 614) [R3].
- **KB-165 ·** clipboard test external_script_does_not_wait_for_inherited_stdio flaky under parallel load (1 s recv_timeout no headroom; R3 observed 1 failed then passes) [R3].
- **KB-166 ·** clipboard_handling=dont_modify silently degrades a rich clipboard (HTML/RTF) to plain text on restore on macOS (text/image snapshot only) while the copy promises preservation; Windows reliable path snapshots all formats (clipboard.rs:197-206; macos.rs:270-277 vs windows.rs:298-352) [R3].
- **KB-167 ·** auto-submit Enter failure on reliable paths discarded without even a log (`let _ = send_return_key`), unlike the legacy path's warn (macos.rs:144-153; windows.rs:106-113) [R3].
- **KB-168 ·** every slider Reset is a silent no-op if loadDefaultSettings failed (errors console-only; resetSetting returns when defaults null) (settingsStore.ts:437-444, 733-745) [R3].
- **KB-169 ·** Test Connection verdict race across provider switch: no staleness guard: previous provider's verdict renders under the new panel; .catch console-only (usePostProcessProviderState.ts:246-268) [R3].
- **KB-170 ·** custom-provider base URL validated nowhere at set time (no scheme check; typo fails only at next use) (BaseUrlField.tsx:27-33; mod.rs:1441-1470); KB-125 family, distinct component [R3].
- **KB-171 · FIXED.** LLM catalog per-file trust anchors were dead for non-default quants: expected_sha256_for resolved only the default file's id, so alternate quants downloaded unverified and skipped the load-time check though the catalog pinned them (catalog/llm.rs:198-219 at the time; every_file_is_fully_pinned test asserts the anchors exist) [R3]. The alternate-quant half landed in ca098651: `find_with_file` resolves every catalog quant's id - default and alternate alike - so `expected_sha256_for`/`expected_size_bytes_for` return the file's own anchor (catalog/llm.rs:221-246, pinned by `alternate_quant_ids_resolve_their_own_trust_anchor` at catalog/llm.rs:443). The user-added half (Add-from-HuggingFace downloads verified NOTHING: expected_sha256 returned None for user repos, ModelManager::verify_sha256 skipped on None, the add flows probed only the GGUF architecture) closed in the v1.5.4 integrity cycle: `download_hf_model` - the single funnel every HF download goes through - fetches the Hub tree listing's LFS sha256 for the exact pinned revision (hf_add.rs `hf_file_sha256`, endpoint derived from hf-hub's own resolve URL so listing and download can never disagree) and verifies every user-added download against it; on mismatch the artifact (snapshot pointer plus blob) is deleted, the existing corruption notice fires, and the add flows refuse registration (download.rs `verify_user_added_download` + model.rs gate; tests: user_added_digest_mismatch_refuses_and_deletes_the_artifact and siblings). Disclosed remainder, deliberately unverified: files the Hub lists without an LFS digest (non-LFS, under the Hub's LFS size threshold), digest listings that themselves fail (metadata outage must not break an anchor-less download), artifacts already complete on disk before the call (no bytes fetched; the entry short circuit returns first), and voice-catalog models' primary HF path (the catalog anchor exists but the primary path still does not app-side hash it - only the mirror fallback does; a pre-existing gap outside this fix's scope).
- **KB-172 ·** BaseUrlField's disabled tooltip is hardcoded English AND wrong for its only reachable state ("managed by the selected provider": it renders only for custom) (BaseUrlField.tsx:20-22) [R3].
- **KB-173 ·** the four prompt CRUD handlers drop backend error statuses (specta resolves {status:"error"}, handlers never check); a refused add/update/delete/duplicate leaves the UI silently unsaved; worst on Update where the draft keeps showing the edit that never persisted (PostProcessingSettings.tsx:584-632) [R3].
- **KB-174 ·** user templates can never carry language/register/description (add hardcodes auto/General/empty; update takes only name+prompt; no editor controls); badges are read-only decoration and the script guard can never sanction a custom script-switch template (mod.rs:1538-1547) [R3].
- **KB-175 ·** the dictation-cancel sweep concludes live prompt_test runs as failed:cancelled (only history_retry exempted), dropping the test's real outcome from the runs table (post_process_runs.rs:566-573) [R3].
- **KB-177 ·** "Reset to default" on an unedited matrix (stored=null) silently converts the store to Some(snapshot); freezing the user out of future default-phrase additions; only Reset All restores (CommandsSettings.tsx:273-289) [R3].
- **KB-178 · FIXED (v1.6.2, 6b054cd1).** Was: Spoken Numbers only runs for English/Hindi-Hinglish output while the dropdown promised digit conversion in all 26 locales with no scope caveat. Now the copy states the scope in every locale (and the pp validator's digit-script guard keeps scripts consistent).
- **KB-180 ·** failed first-page history load renders the "No transcriptions yet" empty state (catch console-only; no error state/retry) (HistorySettings.tsx:113-114, 255-259) [R3].
- **KB-181 ·** "Open Recordings Folder" reveals-not-opens on macOS (NSWorkspace activateFileViewerSelectingURLs selects the folder in its parent); deliberate ACL workaround, residual is label-vs-behavior (commands/mod.rs:92-107; plugin source read; static) [R3].
- **KB-182 ·** a failed history save during dictation is invisible on every surface (both call sites log-only; text pastes normally but never appears in History) (actions.rs:1601-1612, 1812-1813) [R3].
- **KB-184 ·** the phone vibrates for every forwarded overlay notice but can display only three codes: any other code buzzes with no visible message (client/index.html:400-407; ~25 codes exist) [R3].
- **KB-187 · FIXED.** Was: onboarding permission cards used bg-white/5 - invisible fill in light theme (AccessibilityOnboarding.tsx:423, 482) [R3]. Fixed in 14369da8: both permission cards fill with `bg-background`, theme-safe in light and dark.
- **KB-188 ·** tray Post-process Prompt submenu offered unconditionally: with pp off, picking a template silently moves a checkmark that affects nothing (tray.rs:710-726) [R3].
- **KB-189 ·** nine dead footer updater keys (downloading, installing, preparing, updateAvailableShort, five portableUpdate\*) referenced by zero components but forced into all 26 locales by the parity gate [R3].
- **KB-190 · PARTIALLY FIXED.** Was: App Language and Update Policy rows missing from settings search while every other About row was indexed (SettingsSearch.tsx:168-176) [R3]. Fixed in 14369da8: both rows added to the static index, in AboutSettings tab order. Residual note: the gap was wider than the KB text - the Theme and Accent Color rows are now indexed as well (this cycle's search-index module), so the About index is complete; future About rows must be added on arrival.
- **KB-191 ·** re-enabling update checks mid-session fires an immediate silent auto check at toggle time (hasAutoChecked only latches when the guard previously passed) (UpdateChecker.tsx:38-49) [R3].
- **KB-192 ·** app language applies i18n.changeLanguage before persist; failed persist leaves webview in the new language while store/tray keep the old until relaunch: KB-104 family (AppLanguageSelector.tsx:30-33) [R3].
- **KB-193 · FIXED.** Was: About version row rendered a bare "v" while the version IPC was in flight (state starts empty; span ungated) (AboutSettings.tsx:22, 47) [R3]. Fixed in 14369da8: the span renders only once `version !== ""` - no flash.

---

## Verified solid (do not re-litigate)

[R3 positive verifications:] the secure-input subsystem + recording lifecycle end-to-end (monitor/fallback/tray badge, per-binding hold windows, coordinator parity, cancel-during-Processing); KB-019's fix resolution shared by all four surfaces (gate/Test Connection/tray/delete); history slice: show_history_model off path, retention=Never deletes nothing, failed settings writes roll back+toast, outcome tokens always resolve, retry can't hang on a stuck load, dates localized, asset protocol scoped; tray i18n scan clean across 26 locales; ThemeSelector/AccentColorSelector optimistic-apply solid; prompt-library test batch green (13 required templates, keep-language rule, one-time migration). R3 also ran green: actions::tests 27, local_llm::manager 24, companion:: 24 + finalize_companion 5, tray/autostart/portable/legacy_migration 59, managers::history 7, managers::audio 3, mute_restore 1, post_process_runs 13, prompt 11, engine_supervisor 11+16 ignored, clipboard 7 (one flaky: KB-165), check:translations, test:updater, historyLimitInput + localLlmRouting + postProcessModelCache direct scripts.

## Cross-cutting themes (updated)

1. **Hidden-window / hidden-card feedback gap**: still the largest class (KB-011 residual, 013 residuals, 029, 061, 087, 106, 124, 139, 143, 155, 180, 182, 184 + device-name KB-163). Cycle 3 landed the routing decision itself - `card_visible` + the App.tsx router with toasts and macOS notifications (d540241b + a6997aec) - closing 016/020/038/148 and 162 (020/148 keep residuals); retiring the legacy duplicate failure listeners is the remaining leverage here.
2. **Hotkey activity gating init-only**: KB-008, 009, 036, 114, 124 (+ new KB-159/160 on the rebind/window-close edges).
3. **Lying toggles / no-delivery settings**: 030, 078, 104 residual, 112, 145, 183, 188, 191 (KB-027 fixed in cycle 3, a9e1159e).
4. **Raw English into localized copy**: KB-122 umbrella (037, 042, 052, 058, 065, 097, 118+LogLevel, 138, 163, 172, 189).
5. **Persist-before-apply / apply-order**: KB-104 residual, 192 (+ the fixed 103 as template).
6. **Unclamped numeric setters**: KB-048, 115 (five setters), 116.
7. **Tray staleness**: 066 (three submenus), 146 (+ 143 log-only rejections); KB-031/034 fixed in cycle 3 (a9e1159e) and KB-185 in 14369da8.
8. **Script/number awareness**: KB-076, 077, 134, 135, 178 (+ whisper-prompt bias KB-176).
9. **Assert-script tests invisible to `bun test`/CI**: KB-136 family, now three files, including a fix's own guard test (KB-005).
10. **Per-drag full-store writes**: KB-120 family, now ~9 sliders.

## Dedup and correction log (final round)

- Merged re-reports: KB-008 ×3, KB-020 ×3 (slices 1/4/7), KB-016 ×3, KB-014 ×2, KB-011 ×2, KB-015 ×2, KB-109 ×3, KB-066 ×4 (voice-submenu instance folded in), KB-122 ×3, KB-124 ×2, KB-115 ×2, KB-114 ×2, KB-038 ×2, KB-033/034/035/113/148/094/099/092/098/144/073/108/117/131/132/133/146 ×2, KB-163 ×2 (slices 4+10), KB-045/047/022/021/025/026/023/024/079-081/137-140 ×2.
- Folded instances: LogLevelSelector → KB-118; five Output sliders + two pp sliders + word-correction → KB-120; skipToastDedupe + historyLimitInput → KB-136; sound-theme placeholder → KB-058; keyboard-impl switch → KB-142; base URL → KB-125 family (kept as KB-170, cross-ref); KB-083 residual → KB-085.
- **KB-025 corrected:** the "history_limit=0 pinned by test" claim (carried since R1) is false in this tree: no cleanup test exists [R3 grep + test run].
- **KB-104 split:** mode setter FIXED (verified ✓R4), device setter residual OPEN (verified ✓R4).
- Severity moves: KB-080 → medium [R3], KB-144 → medium [R3].

## Verification ledger (this consolidation round)

- `git -C voxbar-c2 log --oneline -4` + `git show --stat 5bf5b893`: commit confirmed; file list matches all four claimed fixes.
- Reads: audio.rs 1305-1365 (remove_mute in both cancel branches, KB-002 comment), 550-575 (best-effort AlwaysOn open), 1129-1155 (update_selected_device unguarded vs channel sibling); actions.rs 856-875 (selected_llm_model_id gate, KB-019 comment); commands/audio.rs 180-210 (apply-then-persist) + 244-266 (set_selected_microphone still persist-first); settingsStore.ts 106-115 (throwing updater); lib.rs 1200-1212 (Exit: no remove_mute); model_capabilities.rs granite grep (:43 + :61); shortcut/mod.rs 346-380 cancel-branch shape; ModelUnloadTimeout.tsx:41 customActive gating.
- Ran: `nice -n 15 bun run check:model-languages` → still fails (hi/hi-Latn, exit 1) ✓R4.
- Not run by me (carried as [R3], who ran them against 5bf5b893): all cargo/bun test invocations listed above, locale sweeps, sonner/transcribe-cpp-sys/tauri-plugin-opener source reads; static-only items never executed live on this macOS host: KB-018, 053, 123, 126, 166, 176 (truncation), 181.

**Bottom line at the end of the review:** the loop converged on the hard classes: every fix commit landed exactly what the KB named, all three round-3 P0s verified FIXED, and the round-3 P0 (KB-150, mic-switch kills live recording) was fixed right after consolidation in c8b0f890. Highest-leverage next moves: the notice-routing decision (theme 1), generalizing hotkey gating (theme 2), and the lying-toggle family (theme 3); each closes 5-20 items with one pattern.

---

# Round 4 (cycle 3 fleet, 2026-10-10)

Twelve reviewers, one per feature area, over cycle3/v1.5.0-quality-waves at
ee983388. Every cycle-3 fix claim was re-verified in code: all held.

## Fix-verified this round (do not re-file)

KB-008, KB-009 (both halves), KB-016, KB-019 (held), KB-020, KB-027, KB-031,
KB-033, KB-034, KB-036 (edge: KB-199), KB-038, KB-104 residual, KB-107,
KB-145/183 (effective_enabled, 4-combination test), KB-148 (caveat: KB-195),
KB-151, KB-154 (pinned twice), KB-158, KB-159, KB-160 (both recorders),
KB-162, KB-185 (end to end), KB-187, KB-188, KB-190 (residual: KB-206),
KB-191 (edge: KB-211), KB-192, KB-193.

**KB-113 resolved by removal**: no toast.loading exists on any model-download
path anymore; the stuck-toast defect has no trigger surface. Mark closed.

## New findings (P1)

- **KB-194 · generic rebind does not check chord conflicts up front, and while
  the recorder is armed cannot catch them at register time either** (suspend
  unregisters everything, register sees no conflict, resume's failure is
  debug-level): one of the duplicate pair dies silently, chosen by map
  iteration order. Fix shape: run binding_conflicts_with_active_binding in
  the generic branch too, pre-unregister (mod.rs:395 has it for cancel only).
- **KB-195 · every hidden-window notification gates on
  document.visibilityState, which WKWebView may not flip for an ordered-out
  Tauri window** (App.tsx:416, updaterFlow.ts:157/334/431): if it stays
  "visible", all KB-148/KB-020 notifications silently no-op. Two reviewers
  converged. Fix shape: drive the gate from the backend main-window-hidden /
  window-visibility truth, not page visibility.
- **KB-109 re-confirmed by two reviewers, upgraded**: retry ignores the pp
  master toggle AND provider switch, so stored transcripts can ship to the
  CURRENT (possibly paid cloud) provider/model/prompt with pp off
  (commands/history.rs:100-107, actions.rs:1098-1137, settings.rs:1851-1855).
- **KB-198 · every toggle-ON hotkey registration discards failure with
  `let _ =`** (mod.rs:1497/1511/2244/2267/2290): a chord duplicated onto a
  toggle-off binding makes toggle-ON hit "already in use" silently; toggle
  reads on, key dead. Fix shape: surface or resolve at commit time.

## New findings (P2)

- **KB-196** failed channel-switch reopen never retries; always-on stream
  left cold until next session (audio.rs:1212-1245; device/VAD siblings retry).
- **KB-197** any phone's hello clobbers session_device mid-session; capped/
  disconnected notices blame the wrong phone (server.rs:316-318).
- **KB-199** KB-036's error exit (failed handy-keys init rollback) skips the
  cancel rearm; mid-recording switch with failing init leaves cancel dead
  (mod.rs:624 short-circuits before both rearm sites).
- **KB-200 · FIXED (cycle 6, df44255c).** unload-timeout settings writes (incl. the tray Custom flow's own
  seed) never rebuilt the tray submenu; now the timeout rides MenuInputs so
  every write rebuilds (commands/transcription.rs:25/110, KB-200 comments).
- **KB-201 · FIXED (cycle 6, df44255c).** set_post_process_provider never
  rebuilt; the provider flip now drives post_process_local_active and the pp
  Model submenu through MenuInputs (shortcut/mod.rs:1807; the template CRUD
  legs at :1843/1865/2022 carry the KB-201/KB-066 surface 1 comments, and
  this cycle's 69a7820c added the add/delete equality pins in tray.rs
  tests - see KB-249).
- **KB-202** history setters are the inverse KB-104 shape: persist succeeds,
  cleanup fails, Err rolls the UI back while disk keeps the value
  (commands/history.rs:122-163).
- **KB-203** updateSetting failure rollback restores a wholesale stale
  settings snapshot, silently reverting unrelated keys (settingsStore.ts:414).
- **KB-204** notification-permission denial is silent and latched forever;
  no surface hints that update notices will never arrive (desktopNotify.ts).
- **KB-205** manual check click during an in-flight silent auto-check is a
  dead click for up to ~21 minutes (runUpdateCheck early-returns on
  inFlight with no feedback; updaterFlow.ts:345).
- **KB-206 · FIXED (cycle 3 wave 5, b63cd215).** Theme + Accent Color rows
  were still missing from settings search; both are indexed now
  (settingsSearchIndex.ts:152-158, the AUD-11 comment: "the Theme and
  Accent Color rows (theme.title, theme.accentColor.label) are reachable by
  search"). Round 5 verified this; the entry above was the stale part.
- **KB-207 · FIXED (v1.6.2, fd63e677)** 300ms hide-unmap race: card_visible=true
  while the overlay already returned null; an info notice in the window is
  invisible on every surface (overlay.rs:731-742 vs RecordingOverlay.tsx:385).
  Notices now probe the show and hide generations (card_durably_visible with
  a pending-hide marker), so the fade window no longer routes card-only.

## New findings (P3)

- **KB-208 · HALF FIXED (capped half, cycle 3 wave 2, a6997aec).** (folds
  into KB-163) companion capped + disconnected both dropped the device-name
  detail in the shared mapping. The capped half now interpolates it:
  `companion_session_capped` passes `device: detail` into
  `overlay.notice.companionSessionCapped` (src/lib/noticeMessage.ts:97-101;
  en copy: "...the 15-minute session limit was reached on {{device}}."). The
  disconnected half still drops the detail (noticeMessage.ts:86-87) - that
  is the remaining KB-163 residual.
- **KB-209** prompt ids can collide on the same millisecond
  (mod.rs:1658/1687; the "random component" comment is not implemented).
- **KB-210** a relaunch() failure after a successful install shows the
  generic update-failure toast (updaterFlow.ts:299-306).
- **KB-211** KB-191 race: fast OFF→ON toggle can skip latch consumption if
  both writes land before the refetch renders (updaterAutoCheck.ts:43).
- **KB-212** restore_registration still re-registers toggle-off chords on
  failure exits (mod.rs:493-503); narrow reach after KB-009.
- **KB-213** GlobalShortcutInput has no unmount-time resume, unlike its
  HandyKeys twin (only listener removal in cleanup).
- **KB-214 · FIXED by mechanism.** KB-136 family grows: shortcutGating.test.ts
  was unwired; CI never ran test:updater. Both halves closed: the "Run bun
  unit tests" step runs `bun run test:updater` (which chains both updater
  test files, package.json:23; the step entered CI in 2bdd6049), and the
  "Test frontend sweep" step (`bun run test:frontend` ->
  scripts/run-frontend-tests.ts, from 4ebe1d4f) executes EVERY git-tracked
  `src/**/*.test.ts` - shortcutGating.test.ts included - so a file with no
  script of its own still runs; scripts/ciTestCoverage.test.ts then fails
  the build when a test is added without an invocation. The same mechanism
  retires round 5's KB-245 note (searchKeyboardNav unwired) and KB-100's
  test:updater half; check:text-style is still absent from CI, so KB-100's
  other half stays open.
- **KB-215** silent value coercion: theme/sound-theme/overlay-position/style
  setters warn + substitute defaults yet return Ok (mod.rs:986-1123).
- **KB-216** write_settings is unobservable (no error surface); persist
  honesty stops at the apply leg (settings.rs:2127-2134).
- **KB-015 reach grew**: model_load_failed raw-English detail now also lands
  in the desktop-notification body via noticeMessage.

## Standing verdict

[Round 6 correction: the paragraph below was true at round 4 but was
superseded within the same cycle - every named "open" item was fixed by
waves 5-6 and verified by round 5: KB-194/198/199/212 and KB-109's
master-toggle half in b746d17a (with tests; spot re-verified in the round 6
pass at shortcut/mod.rs:116/400/445 for KB-194 and :1604/2424/2460/2493 for
KB-198, and commands/history.rs:62-75 for KB-109), KB-195/205/206 in
b63cd215, KB-186 in 0b0e3323 (bundled-catalog reachability caveat stands).
Still open from that queue: KB-109's provider-pinning half (the doc comment
at commands/history.rs:69-71 names it "a separate, larger fix") and KB-195's
minimized-window edge (KB-224). The closed count below also missed KB-112,
which shipped in the same wave 4 commit (fcb3e742) as three counted items:
cycle 3 closed 30, not 29.]

Cycle 3 closed 29 goldmine items with zero false fix claims. The open P1
queue is now: KB-194, KB-195, KB-109, KB-198, KB-186 (offline onboarding dead
end, re-confirmed HIGH by the UI reviewer). Highest-leverage wave-5 pattern:
honest hotkey commits (KB-194 + KB-198 + KB-199 + KB-212 share one file) and
the notification-gate rework (KB-195 unblocks the whole KB-148 surface).

---

# Round 5 (20-agent fleet, 2026-10-11, main @ a10ae0ee + merge ff77be71)

Twenty reviewers over twenty areas (eight never reviewed before: streaming
buffer, command mode, memory gate, engine supervisor, paste pipeline, secure
input, onboarding, release tooling). All 20 delivered (4 after rate-limit
re-dispatch). Every wave-5/6 fix claim re-verified; none false.

## Verified this round (additions to the solid map)

All cycle-3 fixes held, incl. KB-194/198/199/212 (hotkey honesty, with tests),
KB-195 (notification gate on real window visibility, CI-wired test),
KB-205/206/033/191/211-closed-by-mechanism, KB-109 master-toggle half (test),
KB-186 end-to-end (note: the catalog is bundled, so the card is an
IPC-failure surface, not an offline one - reachability caveat when closing).
NEVER-BEFORE-MAPPED SOLID: the four v1.2.3 command-mode miss paths, the GGUF
menu fallback chain down to the crate, render-queue lossless ordering,
never-lose-the-transcript across every engine, LLM swap lifecycle (no leaks),
supervisor invariants (no restart loops, exact worker accounting), secure-
input toggle consistency (error legs skip persist), HF add-flow validation.

## New findings (the next cycle's queue)

CRITICAL: KB-217 smoke-macos.sh can never pass (pgrep -x "VoxBar" vs lowercase
voxbar; unconditional release blocker) - FIXED in ff77be71+ (name + trap).
HIGH: KB-218 voice gate forecasts from the outgoing model's stale un-keyed
measured_asr_rss (refuses small models, phantom fallbacks); KB-219 fallback
resolver's fit test 1.5x looser than the load gate (terminal refusal blames
the fallback; two parity comments now false); KB-022 confirmed both layers.
MEDIUM: KB-220 tray Unload mid-dictation kills the whole dictation (one
shared refuse-while-live pattern also closes KB-117); KB-221 unobserved
command holds silently dictation (worst on menu-fallback models; the latch's
own motivating failure one layer down); KB-222 latched finalize discards
post-release speech; KB-223 reliable-paste chord failure fully silent AND
invisible to paste_failed (Windows: check-then-act clipboard restore race +
structural auto-submit lock loss); KB-224 minimized window defeats the KB-195
gate; KB-225 VPN interface wins the companion bind (10/8 beats home Wi-Fi);
KB-226 CJK command aliases unanchored/ungated (换行 fires inside prose);
KB-227 CMD badge lies on non-streaming models; KB-228 Nepali ~33 untranslated
strings - FIXED in ff77be71+; KB-229 1.5.0.md overclaimed KB-210 - FIXED
(code fixed instead of weakening the note); KB-230 delete/cancel during
transfer emits completion for the deleted model (footer auto-select lies);
KB-231 secure-input unregister failure leaks a double-firing Carbon shadow;
KB-232 release notes hardcoded in release-macos.sh jq arg; KB-233 nothing in
CI gates the release scripts (bash -n would have caught KB-217); KB-234
1.5.0.md + 1.3.0/1.4.0 notes are English-only in all locales (scope call).
LOW tail: KB-207 fixed in v1.6.2 (fd63e677, card_durably_visible); KB-208 companion detail dropped; KB-209
prompt-id collision + false comment; KB-212r register-alias conflicts
bypass the string-equality pre-check; KB-235 Tauri chord-keyed unregister
kills an active sibling's registration (gate on binding_is_active);
KB-236 handy-keys recorder bricked by impl-switch-while-armed (residual
closed in the v1.6.5 residuals cycle, lane R3); KB-237 fixed in v1.6.2
(strand closed by the
per-lane split f2014843, pinned as mode_flip_may_close_local_stream in
fd63e677); KB-238 doomed-mic-preference broadcast before restart outcome
(not re-verified by the round 9 fleet); KB-239 fixed in v1.6.2
(6b054cd1, blocking pool + chars); KB-240 fixed in v1.6.2 (8b94fdc1,
with KB-023); KB-241 fixed in v1.6.2 (removed, 6b054cd1); KB-242 fixed
in v1.6.2 (6b054cd1); KB-243 fixed in v1.6.2 (6b054cd1);
KB-244 search index micro-gaps (pp models/timeout/keep-warm, debug runs);
KB-245 KB-136 family third instance (searchKeyboardNav unwired); KB-100
half-closed (test:updater still not in CI); run-frontend-tests misses
uncommitted files; BUILD.md runbook lacks the smoke step.

## Merge note

The parallel session's 123-commit voxbar branch merged as ff77be71: its Rust
series was already cherry-picked into main across earlier cycles, so main's
fleet-verified versions won every same-intent overlap; content delta is the
bun.lock/.nix correction (plain main failed frozen-lockfile) plus the locale
union of both sides' keys. Full gates green post-merge (809/0 Rust, 19/19
frontend files, translations, text-style, lint, tsc).

---

# Round 6 (v1.5.4 integrity cycle, 2026-10-11, branch cycle7/v1.5.4-integrity)

A truth pass over the ledger itself, plus the model-integrity fix that
closed KB-171's user-added half. Every correction below was made against
the code at cycle7/v1.5.4-integrity (base 623cbc51 + the cycle's three
commits); each entry names the path or commit it was verified against.

## Round 6 reconciliation (every correction made this pass)

1. **KB-112 OPEN -> FIXED** (fcb3e742, cycle 3 wave 4). The round 4 fleet
   verified three siblings from the same commit (KB-104/145/183/188) but
   missed this one, and the entry above still read OPEN. Verified:
   autostart.rs:25-46 (Result + KB-112 doc), shortcut/mod.rs:1313-1331
   (apply before persist, `?` propagation), lib.rs:510-513 (startup stays
   best-effort by design).
2. **KB-104 residual -> FIXED** (fcb3e742). Round 4's verified list names
   "KB-104 residual" but the entry above kept the stale persist-first
   residual sentence. Verified: commands/audio.rs:246-273 (apply-first,
   error propagation, no command-side write) and managers/audio.rs:1186-1226
   (live-recording refusal + rollback of the persisted preference on a
   failed reopen, KB-104 comment).
3. **KB-183 OPEN -> FIXED** (fcb3e742 + this cycle 69a7820c/da66553a).
   Round 4's list names KB-145/183 as verified; the entry above was never
   flipped. Verified: companion/mod.rs:1397 (effective_enabled
   conjunction, fleet pass 2 re-read) + :1382 (init) + :2868 (4-combination test);
   shortcut/mod.rs:955-990 (IPC gate) and :1689 (Experimental kill switch).
4. **KB-206 -> FIXED** (b63cd215, cycle 3 wave 5; round 5 verified). The
   entry above was never flipped. Verified: settingsSearchIndex.ts:152-158
   indexes theme.title + theme.accentColor.label.
5. **KB-214 -> FIXED by mechanism.** test:updater now runs in CI
   (ci.yml "Run bun unit tests"; the step entered CI in 2bdd6049) and the
   git-tracked sweep (scripts/run-frontend-tests.ts, from 4ebe1d4f) runs
   shortcutGating.test.ts and every other tracked `*.test.ts`;
   scripts/ciTestCoverage.test.ts fails CI on any future unwired test. This
   also retires round 5's KB-245 note and KB-100's test:updater half;
   check:text-style remains out of CI (KB-100's other half still open).
6. **KB-208 capped-half -> FIXED** (a6997aec). companion_session_capped
   interpolates the device name (noticeMessage.ts:97-101, en copy
   "...reached on {{device}}."). The disconnected half still drops the
   detail (noticeMessage.ts:86-87); that is the live KB-163 residual.
7. **Round 4 standing verdict corrected in place** (bracketed note added):
   the "open P1 queue" it names (KB-194/195/109/198/186) was closed by
   waves 5-6 (b746d17a, b63cd215, 0b0e3323) and verified by round 5. Spot
   re-verified this pass: KB-194 at shortcut/mod.rs:116/400/445, KB-198 at
   :1604/2424/2460/2493, KB-109's master-toggle half at
   commands/history.rs:62-75 with its provider-pinning half still open (the
   code's own doc comment calls it "a separate, larger fix").
8. **Round 4 closed-count 29 -> 30** (and the intro's mid-cycle 17
   annotated): the miss was KB-112 (see 1). Counting method: the round 4
   verified list has 30 distinct ids, KB-019 was a hold (fixed before
   cycle 3, not a closure), KB-113 closed by removal, so the list itself
   supports 29 - plus the unlisted KB-112 makes 30.
9. **KB-171 -> FIXED** (alternate quants: ca098651, verified at
   catalog/llm.rs:221-246 + the pin test at :443; user-added downloads:
   this cycle, see the entry). The entry above was still describing the
   pre-fix state in full.
10. **KB-200/201 (round 4 new findings) -> FIXED** (df44255c, cycle 6).
    Verified: commands/transcription.rs:25/110 (KB-200),
    shortcut/mod.rs:1807/1843/1865/2022 (KB-201 + KB-066 surface 1).

Spot-verification count this pass: 9 entries against live code (KB-104,
112, 171, 183, 194, 198, 206, 208, 214, plus the KB-109 gate read; KB-200/
201 verified by comment-cited paths) - all fix claims held; the only false
statements found were the ledger's own stale statuses, corrected above.

## New fixes this cycle (new ledger entries)

- **KB-246 · release AI-integrity gate (57e36528).** release-macos.sh tees
  the build to release-build.log and, after the bundle-exists check and
  before signing/smoke, dies unless `otool -L` on VoxBar.app/Contents/MacOS/
  voxbar lists FoundationModels.framework AND the build log shows neither
  stub warning ("Building Apple Intelligence with stubs" / "Apple
  Intelligence SDK not found"): a CLT-only toolchain can no longer ship a
  stub-linked AI build silently. ci.yml greps the release script for the
  three gate strings so a refactor cannot drop the gate (KB-217/233
  family). Same commit: vite.config.ts documents the 2000 kB chunk warning
  limit (the app loads bundles from disk).
- **KB-247 · AUD-01 phone-forwarder denylist completion (69a7820c).** The
  denylist grew from one Mac-only code to four (command_mode_no_session,
  delete_last_word_no_session, undo_no_session, post_process_prompt_cycled)
  and classification is now exhaustive by construction:
  every_notice_code_has_a_deliberate_phone_visibility_classification
  matches EVERY NoticeCode variant with no wildcard arm (a new variant
  breaks compilation until it is consciously classified), pins that the
  runtime denylist agrees with the classification for every emittable code
  string, and refuses denylist entries no variant backs (companion/mod.rs,
  MAC_ONLY_NOTICE_CODES + the test). The buffer variants stay forwarded,
  scoped and documented inside the test.
- **KB-248 · settings hydration guard on the companion toggle persist
  (da66553a).** change_companion_devices_setting keeps the KB-183
  both-toggles gate but re-reads settings AFTER apply and persists that
  snapshot: CompanionManager::start regenerates a stale pairing token (and
  subnet) mid-apply and persists it, so persisting the pre-apply snapshot
  reverted the token the running server just armed - first enable: the QR
  never renders; subnet change: the QR advertises a dead token. The
  load-bearing read ordering is documented on the command.
- **KB-249 · tray refresh pins for the prompt-CRUD legs (69a7820c).** The
  rebuild calls themselves landed in cycle 6 (df44255c, KB-200/201); this
  cycle pins the missing leg as tests: MenuInputs equality now drives a
  rebuild when a template is added or deleted (tray.rs tests, the
  KB-066/201 comment), so the prompt submenu cannot silently go stale
  again. KB-066's other surfaces remain open: the local-LLM submenu still
  has no post-process-model-changed listener (the event is emitted at
  commands/local_llm.rs:357 with zero listeners; only model-state-changed
  rebuilds, lib.rs:496), and the voice Models submenu still rebuilds only
  on model-state-changed.
- **KB-250 · dead-code cleanup (57e36528).** Deleted the orphaned
  record_launched_exe no-op (its cfg(any(not(unix), test)) variant was
  dead everywhere it compiled), the never-called FakeHost::slot_is_free/
  skips_contain, and the RecordingReadness::wait_timeout test helpers.

- **KB-251 · FIXED · CI exit-semantics and discovery nits (df8e936c plus
  the collect-every-failure commit that flipped this entry).** Was
  recorded OPEN at fe971bd5 because no fix existed to cite then; three
  nits, all since landed on this branch. The pwsh exit-semantics and
  discovery halves are df8e936c: every cmake in the Windows
  SPIRV-Headers step guards with `exit $LASTEXITCODE`, the bun unit
  tests and the release scripts syntax gate run as single && chains
  (the cross-shell form that propagates any failure on bash -e and
  pwsh alike), and the sweep's per-glob `git ls-files` adds --cached
  --others --exclude-standard so untracked \*.test.ts files run in local
  sweeps exactly as CI's next fresh checkout would see them
  (run-frontend-tests.ts). BUILD.md's release-script paragraph documents
  the `scripts/smoke-macos.sh` smoke step (the docs half, 332888c7). The first nit - the sweep exiting on the FIRST
  failing file, masking the state of the rest of the suite - is fixed
  in this entry's own commit: the loop records each failure and keeps
  running every remaining file, prints an "N of M frontend test files
  failed" summary listing every one, and exits nonzero on any failure.

## Round 7: the post-processing truth pass (the operator's "no effect" report)

The trigger: the operator reported post-processing "has no effect, ever", and
his report arrived with a live-data note saying his one post_processed_text
row looked IDENTICAL to its raw transcription. The forge-time audit (whose
facts the lane was told to trust) measured otherwise, and this pass re-verified
the dispute directly, read-only, against the live history.db: row 122 holds
transcription_text of 2353 chars and post_processed_text of 2354 chars, and
the texts diverge at exactly one spot - the raw's bare newline in "resolve
them as well\nunattended workflow run" became "as well. Unattended workflow
run" (period, space, capital; +1 char). So the row is a one-character
near-echo, not byte-identical: apparent identity, the auditor's verdict, and
now a repeatable sqlite3 check all agree. (The repo carries no audit artifact;
the evidence lives in the operator's history.db. Nothing else in this entry
rests on it - the guard is pinned behaviorally either way.)

The verdict: the pipeline WORKS. His one completed run (row 122, local
Qwen3-0.6B, outcome=applied, 10.9s) was pasted and saved; the one-character
change was imperceptible in the pasted wall of text; his two later attempts
(rows 124/125) were refused by the memory gate with a once-per-session info
toast; and the History UI shows only the raw transcription_text anyway, so
the processed text is unreachable anywhere in the app. The jq nulls that
suggested "not configured" were wrong key names at the wrong nesting: the real
keys (.settings.post_process_enabled=true, .settings.post_process_provider_id,
.settings.post_process_models, .settings.post_process_selected_prompt_id) were
all set. "I see no change" was the expected perception of a working pipeline
with three visibility defects and one genuine product defect - all four now
fixed on this branch.

- **KB-252 · FIXED · the validator had no identity guard: a verbatim echo
  recorded as `applied` (round 7).** `validate_post_process_output` checked
  non-empty, collapse, expansion, and script - never output-vs-input - so a
  weak local model that near-echoes the transcript (the 0.6B on row 122)
  sailed through and the outcome column could not distinguish "cleaned" from
  "did nothing". Fix: the exact identity guard `is_no_op_output` (trimmed
  byte equality, deliberately NOT looser: punctuation and capitalization
  fixes are the feature's core value, and any normalization past trim would
  flag a legitimate minimal cleanup of already-clean text) plus the
  `PostProcessOutcome::Unchanged` token. The single success seam
  `RunsRegistry::finish_applied` (post_process_runs.rs) records `unchanged`
  with the no-op fallback reason and is now the one finish every engine's
  validated-Ok arm calls (cloud's handle_completion, Apple Intelligence's Ok
  arm, the local runner's WarmHold and terminal arms); near-echoes (one
  character in thousands) stay `applied` with their changed ratio on the
  outcome line, which is the honest limit of a one-run guard. Pinned by:
  no_op_guard_is_exact_identity_after_trim (which includes a one-character
  near-echo as NOT a no-op),
  `finish_applied_distinguishes_changed_from_unchanged`,
  `verbatim_echo_records_unchanged_not_applied` (local runner fakes), and verbatim_echo_cloud_run_records_unchanged_and_notices (cloud
  seam, which also pins the info notice).
- **KB-253 · FIXED · a requested-but-unstartable polish was completely
  silent (round 7).** The dictation seam's pre-run config guards (no provider
  resolves, the provider has no model, no prompt selected, the selection
  dangles, the template body is empty) only debug!-logged and returned raw -
  the exact "post-processing has no effect" machine: the operator presses the
  pp binding, raw text lands, nothing anywhere says why. And the default
  `selected_prompt_id` is None, so "toggle on, never pick a template" walks
  straight into it. Fix: the pure, exhaustive classifier
  `post_process_request_gap` (actions.rs) names the specific gap, and
  post_process_transcription emits the new error-tone
  `post_process_not_configured` notice (overlay card row, hidden-window
  toast, persistent OS notification in the download_missing class) with that
  cause as detail. Pinned by
  `post_process_request_gap_tables_every_config_fallback`; the phone-forwarder classification (companion/mod.rs allow list)
  and the frontend routing tables (noticeRouting.ts, noticeMessage.ts, both
  pinned test mirrors) carry the code exhaustively.
- **KB-254 · FIXED · the pp runs table could say "skipped" but never why
  (round 7).** finish_with_detail wrote the skip explanation onto the pp:
  log line only; the Debug table's record (and the copy button) never
  carried it, and the cloud failure arms concluded through finish() with no
  detail at all. Fix: `PostProcessRunRecord.outcome_detail` (bindings
  regenerated) records the reason at finish time - local skip explanations,
  the no-op reason, and now every cloud failure cause too (the
  conclude_generation! macro and the validation-failure arm pass their
  detail; the review pass caught that the first cut left the cloud path
  detail-free, exactly the gap the field docs claimed was closed) - and the
  Debug table's OutcomeCell tooltip shows the token AND the reason.
  (changed_ratio was already a record field before this pass; the table
  renders neither it nor anything else new.) Pinned by
  finish_with_detail_surfaces_the_reason_in_the_record,
  cloud_401_is_failed_auth_and_raw_transcript (transport-failure detail),
  and the garbage-engine test (validation-failure detail).
- **KB-255 · FIXED · the built-in templates never said the input was an ASR
  transcript (round 7).** Every seed body opened with "a transcript generated
  by a speech-to-text model" and a cleanup list, so the model met homophones,
  missing punctuation, spoken-punctuation words, hallucinated fragments, and
  broken spacing with no instruction to fix them first - a conservative
  template plus a 0.6B engine converges on echo. Fix: the shared
  `PROMPT_ASR_PREAMBLE` (settings.rs) opens every body - fix obvious ASR
  artifacts silently BEFORE the role applies, never invent content - each
  role keeps its specialization (minimal explicitly scopes the preamble down
  to marks, never words), and the seeds ship at generation 2
  (BUILTIN_PROMPT_GENERATION). The versioned reseed in
  ensure_post_process_defaults upgrades a stored builtin ONLY when its
  version is below the current generation AND its body still byte-matches
  the ONE generation-1 scaffold (prior_generation_seed_body). Provenance,
  verified against the repo history during the review pass: the library's
  introduction (13b354b6) seeded that scaffold and no later commit changed
  it before generation 2; the E.g. example line this build's scaffold
  carries was borrowed from the PRE-LIBRARY legacy prompt (fc465b49, which
  predates the library and touched only the single default prompt). A first
  cut of this entry wrongly claimed a second generation-1 scaffold revision
  existed; it did not, and the phantom was removed. The reconstruction was
  verified byte-identical to the operator's live english_casual (his
  selected template, still version 1) before the migration was trusted -
  evidence from the live settings store, read-only, not a repo artifact. A
  user edit bumps the version and changes the body, so edits are doubly
  protected.
  Pinned by: every_builtin_template_carries_the_asr_preamble,
  builtin_templates_contain_no_em_or_en_dashes, and
  generation_bump_reseeds_untouched_builtins_and_never_clobbers_edits.
- **KB-256 · NEW · the Prompt Enhancer built-in (round 7).** A fourteenth
  seed: the input is a roughly dictated instruction for an AI agent or app,
  the output is one clear structured prompt (Goal / Context / Constraints /
  Desired format) preserving every intent, adding no facts, with no preamble
  or commentary. language=auto (it restructures in whatever language was
  dictated), so its keep-language line pins the same-language rule the other
  templates carry. Locale name/description ship in en plus every locale
  file. Pinned by prompt_enhancer_seed_pins_the_restructuring_contract and
  the fourteen-template catalog pin.

Still open after this pass (deliberately, other lanes own the files): the
History UI still renders and copies only entry.transcription_text, so a
successful run's output stays invisible there (the auditor's primary
visibility defect - the History panel is not this lane's surface); the
main-window skip/failure toasts dedupe once per reason per app session
(App.tsx + skipToastDedupe.ts), which mutes a persistently memory-pressured
machine's second-and-later memory-gate refusals; and Apple Intelligence has
still never been exercised as a pp engine on the operator's machine (he
switched after the 10:05PM refusals; no run exists post-switch).

## Round 8 queue: the six structural lows kept open after the v1.6.0 fleet (2026-10-12)

All six verified by the fleet consolidator with path:line evidence; each is
low severity today and needs a design decision or connection-identity work
rather than a one-line fix, so v1.6.0 ships with them disclosed.

- **KB-254 · FIXED (v1.6.2, f2014843) · zero-audio notice can name the wrong phone.**
  Was open: session_device was claimed only at hello-while-idle (mod.rs:182-187,
  KB-197) and on_session_changed(true) re-read the stale slot, so phone B's
  zero-audio session after spectating phone A named A in the notice detail.
  Fixed by construction: per-lane identity means the zero-audio notice names
  the session's own device (round 8 meaning; see the round 9 correction log
  for the id collision with round 7's KB-254).
- **KB-255 · FIXED (v1.6.2, f2014843) · quick reconnect can zero the tray's
  paired count.** Was open: forget_device removed by NAME on every
  connection end (server.rs:520-522, mod.rs:264-268) while keepalive death
  detection lagged 20s, so a stale socket's death could erase a fresh
  same-name registration. Fixed: connections register by (server-assigned
  lane, monotonic generation) and release only on a matching pair, pinned by
  registrations_release_by_lane_and_generation_never_by_name
  (companion/mod.rs:1798 after the fleet pass 2 re-read); the device list derives from live registrations
  (round 8 meaning; see the round 9 correction log for the id collision
  with round 7's KB-255).
- **KB-256 · FIXED (v1.6.2, 8b94fdc1) · a v1.5.x built-in rename strands the
  template on the gen-1 body.** Was open: update_post_process_prompt always
  bumped version on a name-only edit (shortcut/mod.rs:2268-2272), so the
  migration's version<seed guard never reached its body-match fallback.
  Fixed: the version bumps only when the body changed, so a rename of an
  untouched gen-1 builtin takes the generation reseed instead of being
  stranded as a fake user edit (round 8 meaning; see the round 9 correction
  log for the id collision with round 7's KB-256, the Prompt Enhancer).
- **KB-257 · FIXED (v1.6.2, 6b054cd1) · studio batches have no cancellation
  and outlive their UI.** Was open: the backend loop had no cancel token and
  unmounting PromptStudio discarded the cards while the batch ran; a second
  batch could stack on the first. Fixed: a cancel generation checked between
  runs, a Cancel button, and a module-level batch controller so remounts
  reflect (and cannot stack on) the running batch, pinned by
  studio_batch_cancel_generation_retires_only_older_batches_kb257.
- **KB-258 · FIXED (v1.6.5 residuals cycle, lane R2) · the phone 'Polish text'
  toggle only arms the NEXT session.** Was open: prefs freeze at session
  start (mod.rs:738-744, pinned), while the keyboard pp hotkey flips the
  live session; the phone surface did not say so. Fixed via the label
  branch of the either/or: the toggle row carries a hint line under the
  label (companion.postProcessHint, "Takes effect from the next
  dictation.", shipped across all 26 locales; the row renders it at
  companion/client/index.html:612 and initializes it at :740, embedded
  blob regenerated through scripts/embed-companion-strings.ts). Pinned by
  the KB-258 block in companionClientPage.test.ts (hint element inside
  the toggle's own row, postProcessHint key, all-locale coverage;
  mutation-checked by deleting the hint element). The fold-mid-session
  branch stays the deeper change if flipping the live session is ever
  wanted.
- **KB-259 · FIXED (v1.6.2, fd63e677) · Tailscale classification is range-based only.**
  Was open: any 100.64.0.0/10 address was labeled Tailscale and widened the
  pairing token to the whole /10 regardless of interface (ip.rs:61-95); an
  ISP-CGNAT LAN could present as Tailscale in the picker. Fixed: the label
  corroborates the interface name (utun family, ip.rs:82-96) before the
  CGNAT range is called Tailscale; an uncorroborated CGNAT address labels
  and pairs as plain LAN (/24, never the whole /10), and the README carries
  the org-tailnet token-scope security note (README.md:301-307).

---

# Round 9 (v1.6.2 backlog-clear cycle, 2026-10-12, branch cycle10/v1.6.2-backlog-clear)

Before any lane ran, a seven-agent verification fleet re-verified the open
backlog against main at 43f4c092 (v1.6.0): fourteen items were already fixed
(the ledger statuses were stale; flipped in place above and below), and the
rest were verified open with fresh anchors and baked into the lane briefs.
Six implementer lanes then landed the cycle: 86178659 (W1-L0, the tap-lock
audio-admission fix plus activation copy), f2014843 (W1-L2, per-lane
multi-phone sessions), 8b94fdc1 (the data-integrity wave), 58a920ff (W1-L3,
the UX polish wave), 6b054cd1 (W1-L4, the pipeline quality wave), fd63e677
(W1-L5, the platform wave). The cycle then closed under its own
verification: two fleet passes over the landed tree confirmed and fixed
twelve more defects (37a53abd, 801d48d3), a line-by-line review of the
architectural lane's diff found three defects in the per-lane seams plus
two sibling escapes and pinned the prewarm RAM rule (4872ee5f, 9e0124e2,
94291724), and two W1-L4 follow-ups closed the remaining instances of
their defect classes (33529030, 186a222d). The program register with the
operator decisions
lives in the shared vault (programs/mission-control-voxbar-v1.6.2-backlog-clear);
the fleet's per-item verdicts live in the controller session record, and the
anchors below were re-checked in this tree where the fix landed here (fleet
anchors cite 43f4c092 where the file has since moved).

## Closed as already-fixed by the verification fleet (do not re-file)

- **KB-008** (371f2b41, roster pinned by 6670025b): binding_is_active gates
  cycle_post_process_prompt and the pp toggle drives both keys through
  POST_PROCESS_TOGGLE_BINDINGS. Round 4's verified list already named it;
  the P1 entry above was the stale part and is now flipped.
- **KB-009** (backend 371f2b41, frontend ab25b849): change_binding registers
  only post-update-active bindings, restore_registration is gated the same
  way, and the recorder chips hard-guard arming while their master toggle
  is off (fleet anchors: mod.rs:482-504, 538-551, shortcutGating.ts:15-49).
- **KB-036** (371f2b41): rearm_cancel_after_implementation_switch runs at
  both success exits of the impl switch and the KB-199 error exit schedules
  the reconcile too (fleet anchors: mod.rs:269-272, 675, 685, 880-887).
- **KB-159** (371f2b41): binding_conflicts_with_active_binding pre-checks
  chord conflicts in the cancel branch and the generic branch (KB-194's
  twin, fleet anchors mod.rs:116-127, 394-408, 444-454).
- **KB-160** (371f2b41 + ab25b849 + static pins b2e36c72): the single hide
  path emits main-window-hidden exactly once and both recorders cancel and
  release the suspension (shortcutRecorderWindowHide.test.ts:33-76).
- **KB-017** (05ef0cf9, the KB-223 fix): a reliable-paste chord failure
  settles the waiter, restores the clipboard, and surfaces as PasteFailed
  on both platforms; the auto-submit Enter is a pinned not-owed after chord
  failure (paste_tx/mod.rs:97-103, pinned :356-370).
- **KB-106** (754c0fc5, v1.6.0's notification lane): paste_failed routes a
  persistent OS notification through the notice router regardless of window
  or card visibility (noticeRouting.ts:217, desktopNotify.ts:160-172).
- **KB-186** (0b0e3323): the onboarding model step renders an error card
  with Retry and Skip instead of the silent dead end (Onboarding.tsx:312-341
  at 43f4c092); round 4's bracketed correction already said so.
- **KB-035** (by v1.6.0): release notes ship through per-version files up
  to 1.6.0.md and the blank last-seen migration resolves to the newest note
  (settings.rs:2151-2153, releaseNotes.ts:70-78 at 43f4c092).
- **KB-089** (82e8700d): both renderers gate Debug on debug_mode
  (Sidebar.tsx:112-117, SettingsSearch.tsx:42-53 at 43f4c092).
- **KB-113** (obsolete): no loading toast exists on any model-download
  path anymore; round 4 had already ruled closure by removal, the P1 line
  above kept a stale re-verified marker.
- **KB-245** (82e8700d): searchKeyboardReducer is wired into SettingsSearch
  (SettingsSearch.tsx:98-116 at 43f4c092); round 6 item 5 already retired
  the note.
- **KB-100, test:updater half** (2bdd6049, the KB-214 CI mechanism; round 6
  item 5). The other half (check:text-style) is one of the two CI lines
  fd63e677 added this cycle, so KB-100 is now fully closed.
- **KB-012, script half** (dbd2e1b8): the hi/hi-Latn family rule passes.
  The CI half is the second line fd63e677 added, so KB-012 is fully closed
  too (ci.yml:187/190 in this tree).

## Fixed this cycle, by lane

**W1-L0 (86178659): the tap-lock admission defect, now KB-260 below**, plus
the activation copy: the phone's hold_or_toggle idle button and welcome hint
name both modes (companion.holdOrToggleToTalk across en plus all 26 locales,
embedded strings regenerated; client_page.rs:252-294 pins the page carries
the key).

**W1-L2 (f2014843): per-lane multi-phone sessions.** KB-110 and KB-111 die
by construction (each connection owns its recorder, ring, coordinator
session, delivery target, and recents); round 8's KB-254 (wrong-phone
zero-audio attribution) and KB-255 (reconnect erasing a fresh registration)
fall out of per-lane identity. Pins in this tree: route_input at
transcription_coordinator.rs:1710 with input_routing_is_exhaustively_classified
(:4013), two_lanes_each_hold_a_live_session_with_independent_stops (:4247),
a_companion_press_while_the_keyboard_records_never_reaches_the_lane_machine
(:4419), registrations_release_by_lane_and_generation_never_by_name
(companion/mod.rs:1798 after the fleet pass 2 re-read), session_identity_freezes_per_lane_and_cannot_be_renamed,
session_prefs_freeze_per_lane_and_survive_the_stop_pipeline,
transcript_delivery_and_state_frames_flow_through_app_state_per_lane,
stream_owner_tokens_round_trip_and_never_collide; the new capture_device
column (managers/history.rs:70) with
per_device_recents_show_each_phone_only_its_own_sessions (:1900, query at
:611-639). One cross-source rule survives: a local recording and a companion
recording never coexist; companion sessions coexist freely.

**Data-integrity wave (8b94fdc1):** KB-179 (quarantine, below), KB-023 +
KB-240 (update_transcription rewrites the five pp summary columns atomically
and NULLs them when the leg did not run; the retry threads the fresh
summary), KB-025 (delete_entries_and_files emits per-id Deleted events and
the cleanup tests exist), KB-108 (persisted tombstones keep built-in prompts
deleted: deleted_builtin_prompt_tombstones_and_never_resurrects,
tombstone_list_round_trips_through_the_store,
tombstone_with_a_restored_row_drops_the_zombie,
prompt_commands_wire_the_tombstone_and_version_seams), KB-109 remaining
half (retry resolves provider, model, and prompt from the entry's recorded
summary first; a recorded prompt that no longer exists skips the leg with a
notice), KB-022 (the Commands tab passes loaded defaults into every table
build and disables add/remove while unresolved), KB-045 + KB-047 (per-model
download slots claimed atomically, DownloadCleanup removes only its own
token, cancel with no token is a no-op), and round 8's KB-256 (the version
bump only on body change, so a v1.5.x rename of an untouched gen-1 builtin
takes the generation reseed).

**W1-L3 (58a920ff), fourteen verified findings:** KB-032 (the six
text-white-on-accent sites now text-accent-on, Button.tsx:30 in this tree,
pinned by accent.test.ts), KB-105
(kb105_error_hide_snapshots_the_generation_before_the_read_delay), KB-014
(kb014_preview_wait_runs_only_when_something_can_show and the stop-pipeline
twin), KB-093 (ErrorBoundary context="Settings" wraps the settings scroll
content, App.tsx:840), KB-144 (requiresExperimental on the experimental-only
search rows plus the Companion Devices entry, settingsSearchIndex.ts:14,
146, 155), KB-021 (every locale's voice-deletion description names the live
scratch-everything phrase, cross-pinned against the recognizer defaults),
KB-080 (custom cues resolve through an existence check that falls back to
the default theme's cue, four kb080 tests), KB-081 (per-row pending state,
revert-to-pre-click, failure toast on the history star), KB-087
(kb087_tray_copy_reports_every_terminal_path), KB-088 (the debug chord
ignores auto repeats), KB-090 (a custom dark variant follows the app theme;
the log override too), KB-091 (Check Again answers with the current
permission state; paired emerald shades), KB-092 (AppData and LogDirectory
skeletons and errors on tokens), KB-181
(kb181_directory_buttons_open_instead_of_reveal).

**W1-L4 (6b054cd1), nine backlog items:** KB-239 (the Apple Intelligence
FFI moves onto the blocking pool and the output-length logs count chars,
actions.rs:1293, 1359), KB-257 (studio batch cancellation:
studio_batch_cancel_generation_retires_only_older_batches_kb257), KB-176
(whisper_initial_prompt_bounds_and_keeps_custom_words_last_kb176), KB-015 +
KB-122 (the memory-gate refusal gets its own notice code with structured
model/needed/free detail and the transcription-error and model-state-changed
toasts show localized copy:
memory_gate_notice_detail_carries_model_needed_free_kb015_kb122), KB-076 +
KB-135 + KB-178 (script-aware terminal punctuation, the digit-script
validator guard, and the spoken-numbers scope copy in all 26 locales:
test_terminal_punctuation_is_script_aware_kb076_kb135_kb178,
validator_rejects_unsanctioned_digit_rewrites), KB-242
(surface_syncs_propagate_their_failures_kb242), KB-243
(unregister_failure_aborts_the_change_kb243), KB-241 (the inverted
is_model_loading command is removed; no references remain), KB-096 (the
version is injected at build time from package.json and the fallback literal
is pinned to it, src/lib/utils/appVersion.ts).

**W1-L5 (fd63e677), six platform items:** KB-237 (the always-on flip's
close guard extracted and pinned as mode_flip_may_close_local_stream,
managers/audio.rs:300 with the pin at :2156; the strand itself was closed
by the W1-L2 per-lane split), KB-010 (an IOKit lid watcher re-resolves the
desired microphone on real lid transitions with a guarded restart;
kb010_lid_watcher_registers_an_iokit_interest_and_dedups and the
runtime-probe test), KB-018 (multi-line transcripts reach dotool as
per-line type commands plus an explicit enter per newline:
dotool_script_types_each_line_and_presses_enter_per_newline), KB-259
(above), the card_visible TOCTOU that KB-020's residual and KB-207 carried
(notices probe the show and hide generations via card_durably_visible with
the pending-hide marker, pinned by
card_durably_visible_reads_pending_hide_before_current_generation), and the
two CI lines (check:text-style and check:model-languages beside
check:translations, ci.yml:187/190), which finish KB-100 and KB-012.

**Fleet pass 1 over the landed tree (37a53abd), six confirmed defects:**
KB-263 through KB-266 (below), the KB-015/KB-122 residual (the memory-gate
notice code sat in DETAIL_INTERPOLATING_CODES, whose members' OS
notification bodies re-localize with an empty detail, so the gate's body
rendered dangling mid-sentence slots in all 26 locales; the fix removed
the code from the set, the session cap's decision, so the body keeps the
fully interpolated line, noticeMessage.ts:208-235), and
one test-hygiene repair (emit_overlay_notice's probe comment named the
wrong atomic, tripping the AUD-14 comment-accuracy pin).

**Fleet pass 2 (801d48d3), six confirmed defects:** KB-267 through KB-271
(below), plus the KB-179 residual: history_db_recovered joined the notice
discipline (NOTICE_CHANNEL_TABLE, noticeRouting.ts:262), so the recovery
notice consults the notifications master toggle like every persistent
notice; with it off neither durable leg fires, and with it on the in-app
log row rides the OS post as the denied-permission backstop.

**W1-L1, the multi-phone review pass (4872ee5f, 9e0124e2, 94291724):** the
line-by-line review of f2014843's owned seams found KB-272, KB-273, and
KB-274 (below); review round 2 caught the KB-273 guard missing on the two
sibling teardown paths (the scoped cancel in utils.rs and the hotkey
load-failure mirror in managers/transcription.rs); and the prewarm RAM
discipline (keep only the most recent connection's idle recorder, never
touch a recording lane's) was extracted as the pure prewarm_retires and
pinned by
prewarm_retires_every_idle_recorder_except_the_target_and_spares_recording_lanes,
behavior identical.

**W1-L4 follow-ups (33529030, 186a222d):** KB-239's sweep found four more
byte-length logs still labeled as chars (the cloud/structured success log
and the legacy prompt-length log in actions.rs, the cmd-mode release-flush
resolved/total pair, and the live-preview finalize counter in
managers/transcription.rs), pinned by output_length_logs_count_chars_not_bytes_kb239
and chars_labeled_logs_count_chars_not_bytes_kb239, both mutation-checked;
and KB-242's delete arm (delete_post_process_prompt) now propagates its
tray re-sync and badge refresh like the edit sibling, with the source pin
extended to scope the delete body (also mutation-checked).

## New entries

- **KB-260 · FIXED · a tap-locked phone session admitted only the ~62 ms
  press-hold sliver of the speech (round 9, W1-L0, 86178659).** The server's
  per-connection edge tracker ended the audio-admission window at the
  finger-lift release edge while the coordinator kept a hold-or-toggle tap's
  session locked live until the next press, so everything after the first
  ~100 ms was dropped as out-of-session audio (one leading-silence chunk to
  VAD, zero samples at finish, the zero-audio notice). Found from the
  operator's live log by the fleet, not by code review. Admission now
  follows the Mac's live session state for the connection's own lane,
  sampled at every received frame and keepalive tick (server.rs:455-595);
  the release edge no longer closes it, the 15-minute cap keeps
  force-finalizing locked tap sessions, and spectator or busy-swallowed
  presses never feed another session's ring. Pinned by
  session_admission_follows_observed_truth_not_release_edges,
  tap_locked_session_captures_the_full_audio_after_the_release_edge,
  tap_locked_session_still_hits_the_cap_from_the_tick_path,
  session_admission_caps_a_locked_session_at_fifteen_minutes, and
  audio_is_refused_whenever_this_lanes_session_is_not_live.
- **KB-261 · DEFERRED-UPSTREAM (investigated v1.6.5 residuals cycle, lane
  R4) · the build still emits ggml duplicate-symbol warnings (round 9
  fleet).** Both ggml-building sys crates are still linked (transcribe-cpp
  0.3.1 and llama-cpp-2 0.1.159, src-tauri/Cargo.toml:92, 102), so every
  release build logs the duplicate-symbol set: exactly 10 symbols, all
  defined in ggml-backend-reg.cpp, compiled into BOTH sys rlibs (the
  release-build.log pairs libtranscribe_cpp_sys rlib[128]
  (ggml-backend-reg.cpp.o) against libllama_cpp_sys_2 rlib[58]
  (ggml-backend-reg.cpp.o)). The link succeeds (ld keeps the first
  definition); cosmetic, but it muddies every release log. Lane R4
  investigated both vendored sys crates in the cargo registry for the fix
  shape (stop one crate from compiling ggml-backend-reg.cpp or force a
  single ggml instance) and found NO clean seam in this repo; the exact
  upstream constraints, verified against the vendored sources: (1) the
  llama side HAS seams - llama-cpp-2 0.1.159 forwards a system-ggml
  feature (llama-cpp-2/Cargo.toml:59) that sets LLAMA_USE_SYSTEM_GGML=ON,
  making llama.cpp/CMakeLists.txt:214-218 find_package(ggml REQUIRED)
  instead of building its vendored ggml, and its build.rs even exports
  cargo:ggml_cmake_dir (llama-cpp-sys-2 0.1.159 build.rs:1084-1094) with
  the comment "a dependent crate that also builds ggml can put this path
  in CMAKE_PREFIX_PATH and use this ggml. Then there is only one ggml in
  the program. Two copies of ggml in one program cause duplicate
  symbols" - but the system-ggml route needs a system-installed ggml
  whose version matches what the pinned bindings target (ggml 0.24.0 at
  llama.cpp/ggml/CMakeLists.txt:6-8; Homebrew ships one rolling
  unversioned ggml and ggml has no stable ABI across minors), and would
  make the packaged .app depend on bundling that dylib; the
  ggml_cmake_dir route needs the OTHER ggml builder to be a DIRECT
  dependent (Cargo links metadata only flows one edge) that reads
  DEP_LLAMA_GGML_CMAKE_DIR and find_package's ggml - transcribe-cpp-sys
  is a sibling with no edge, no metadata reader, and no find_package
  path, so the export is inert here. (2) The transcribe side has NO seam
  at all: transcribe-cpp-sys 0.3.1 vendors ggml 0.25.3
  (transcribe-cpp-sys/ggml/CMakeLists.txt:5-8) and CMakeLists.txt:430
  add_subdirectory(ggml)'s it UNCONDITIONALLY - its feature set
  (cuda/metal/openmp/rocm/vulkan/shared/dynamic-backends) has no
  system-ggml analog, no GGML_ROOT, and no find_package(ggml) anywhere,
  so nothing can stop it compiling its ggml or point it at another one.
  (3) Version skew blocks the single-instance shape outright: llama
  vendors ggml 0.24.0, transcribe vendors 0.25.3, and a single shared
  ggml must serve both - neither crate can build against the other's
  version (bindgen'd bindings plus API drift between ggml minors).
  (4) dynamic-backends does not help: the duplicated symbols live in
  ggml-backend-reg.cpp, which stays in the base ggml lib under
  GGML_BACKEND_DL. The one LOCAL (non-fork) shape found: build
  transcribe-cpp with its shared feature (TRANSCRIBE_BUILD_SHARED=ON;
  its build.rs already stages the installed dylibs beside cargo
  artifacts) so its ggml objects stop being relocatable inputs to the
  final link and the warnings stop - at the cost of bundling, rpath-ing,
  and codesigning the libtranscribe plus ggml dylibs inside the
  distributed .app, a release-posture change disproportionate to a
  cosmetic warning, so not taken this cycle. Upstream constraint, stated
  for whoever revisits: the clean fix needs transcribe-cpp-sys to gain
  either an external-ggml seam (find_package(ggml) honoring a prefix
  path) or an optional dependency on llama-cpp-sys-2 consuming
  DEP_LLAMA_GGML_CMAKE_DIR (the seam llama-cpp-sys-2's own build comment
  invites), with the two vendored ggml versions aligned. Until one
  lands, both rlibs keep compiling ggml-backend-reg.cpp; no fork or
  vendor patch was made (per this cycle's rules).
- **KB-262 · FIXED (v1.6.5 residuals cycle, lane R2) · the AI-unavailable
  copy cannot distinguish a build-lacking-AI Mac from an OS-lacking one
  (round 9 fleet).** Was open: the availability probe was a single bool,
  so a stub-linked build rendered the same "requires macOS Tahoe" message
  the OS-lacks case gets, and the stub's own distinct string never
  reached the availability path (fleet anchors at 43f4c092:
  commands/mod.rs:141-152, apple_intelligence_stub.swift:24). Fixed per
  the fix shape: the probe returns the available / os-unsupported /
  build-lacks enum (AppleIntelligenceStatus at apple_intelligence.rs:32,
  probe at :49, command at commands/mod.rs:152; build.rs:476 emits the
  apple_intelligence_stub rustc-cfg on the stub arm only - registered at
  build.rs:409 - so a stub-linked build answers build_lacks instead of
  the stub's constant-0 bool), one new locale string
  (settings.postProcessing.api.appleIntelligence.buildLacks) ships across
  all 26 locales, and both surfaces render it (the settings card at
  PostProcessingSettings.tsx:288, the studio engine row at
  PromptStudio.tsx:481; bindings regenerated to
  Promise<AppleIntelligenceStatus>). Pinned by
  availability_probe_reports_the_tri_state_kb262 and
  build_script_flags_the_stub_arm_for_the_probe_kb262
  (apple_intelligence.rs:139, :169) plus appleAvailabilityPins.test.ts
  (tri-state binding, both render surfaces, all-locale coverage;
  mutation-checked by collapsing the binding back to bool and by dropping
  the settings build_lacks branch).
- **KB-263 · FIXED · a grace-expired phone stop ran as a keyboard stop and
  wedged its lane machine in Processing with a stranded recorder (round 9
  fleet pass 1, 37a53abd).** expire_due_graces fired the grace-expired
  Stop effect without its lane tag, so the effect ran through the keyboard
  arm, skipped note_stopping_session, and never closed the lane's recorder:
  every HoldOrToggle hold and every PushToTalk session that ended by grace
  instead of a clean release edge was stranded. Fixed: the loop tags the
  effect with its lane before run_effect. Pinned by
  a_lane_grace_expired_release_stops_with_the_lane_tagged and
  expire_due_graces_tags_lane_stop_effects_before_run_effect
  (transcription_coordinator.rs:4339, 4388).
- **KB-264 · FIXED · one phone's Escape could discard another phone's
  in-flight stop pipeline, and a lane Escape fired the engine-wide cancel
  (round 9 fleet pass 1, 37a53abd).** cancel_recording bumped a single
  global cancel generation, so cancelling phone A invalidated the
  generation phone B's just-queued pipeline gated on (the 200 ms streaming
  release-tail window included); a lane-resolved Escape also cancelled the
  engine-wide transcription. Fixed: per-lane cancel generations bumped only
  through the target-scoped helper, and the engine cancel is a
  per-resolution decision. Pinned by cancel_bump_lane_scopes_the_bump_to_the_cancelled_target,
  cancel_recording_bumps_only_through_the_target_scoped_helper, and
  cancel_current_operation_scopes_singletons_to_the_cancel_target
  (utils.rs:256).
- **KB-265 · FIXED · same-platform phones collapsed into one identity and
  one shared recents list (round 9 fleet pass 1, 37a53abd).** The hello's
  device name is a UA platform token (two iPhones both say iPhone, every
  Chrome-Android phone says Linux), so the per-device recents the W1-L2
  lane built keyed every same-platform phone into one list, private
  phone-destined dictations included. Fixed: the client carries a
  persistent device id (localStorage, client index.html:641-652) and
  identity is name#tag, the id's first four alphanumerics, filtered so a
  hostile id cannot smuggle length or separators into a display string
  (protocol.rs:165-184); registration, session freeze, capture_device
  provenance, and recents all key the composite. Pinned by
  hello_parses_the_persistent_device_id and
  device_identity_name_disambiguates_same_platform_phones.
- **KB-266 · FIXED · one binding's resume failure stranded every later
  binding as a dead hotkey until restart (round 9 fleet pass 1,
  37a53abd).** resume_all_shortcuts returned on the first error, and the
  recorder commit's guaranteed already-in-use duplicate made that
  near-certain after a rebind (change_binding registers the committed
  chord before the frontend resumes; the HashMap iterates arbitrarily, so
  on the stock two-binding config roughly half of rebinds left a dead
  hotkey). Fixed: unmount before re-arm, collect failures, report only
  after the loop. Pinned by
  resume_all_shortcuts_never_aborts_mid_loop_on_a_duplicate
  (shortcut/mod.rs:3558).
- **KB-267 · FIXED · stopping the companion server silently discarded every
  live phone dictation (round 9 fleet pass 2, 801d48d3).** stop() cleared
  the lane session map synchronously while the finalize pipelines it had
  just queued were asynchronous mpsc sends whose Stop effects read those
  records through note_stopping_session: every pipeline raced into a None
  identity (no transcript, no history row) and stranded its recorder
  recording=true. Server stop, restart, and toggle-off all shared the
  path. Fixed: the sweep retains exactly the live lanes' records
  (retain_live_lane_sessions at that round's location in the companion
  module) and never
  touches the transient stopping slot. Pinned then by
  server_stop_sweep_keeps_live_lanes_records_for_their_pipelines and
  stop_routes_the_sweep_not_a_clear. (Fleet pass 2 of the v1.6.4 review
  superseded this shape: the retain sweep removed exactly the
  false-edged-but-unconsumed records whose cancel pipelines were still in
  flight, so records are now removed at their pipeline's consumption
  point, note_stopping_session at companion/mod.rs:1274, and stop()
  performs no map surgery at all; pinned by
  server_stop_leaves_records_for_their_pipelines_in_both_stop_orders and
  stop_leaves_the_session_map_to_its_pipelines.)
- **KB-268 · FIXED · the cancel shortcut's want was one boolean, so the
  first overlapping drain unregistered Escape for every concurrently-live
  session (round 9 fleet pass 2, 801d48d3).** Fixed: the want is a
  per-session set keyed None for the keyboard and Some(lane) per phone
  (CANCEL_WANTERS, shortcut/mod.rs:186); each start adds its key,
  FinishGuard and both teardown paths release exactly their own key
  idempotently, and the Carbon fallback mirrors the set-folded boolean
  (secure_input.rs:87-94). Pinned by
  cancel_wanters_track_sessions_not_the_last_call and
  cancel_shortcut_arms_are_keyed_by_session.
- **KB-269 · FIXED · an Escape with nothing recording ignored in-flight
  pipelines, and a local Escape fired the engine-wide cancel under a lane
  pipeline's engine work (round 9 fleet pass 2, 801d48d3).** Fixed: stop
  pipelines register their session key as in flight (removed by their
  FinishGuard); when the cancel resolves to nothing it bumps every
  in-flight lane's cancel generation so those pipelines abort cleanly at
  their gates (no paste, no spurious failure notice, no empty history
  row). Pinned inside
  cancel_current_operation_scopes_singletons_to_the_cancel_target
  (utils.rs:271-284) and by
  in_flight_pipeline_registry_tracks_lanes_and_drains.
- **KB-270 · FIXED · a failed start could cancel another session's live
  stream and tear down its surfaces mid-dictation (round 9 fleet pass 2,
  801d48d3).** The failed-start teardown cancelled the stream singleton and
  flipped the tray and overlay unconditionally, so a failed non-owner
  start under lane overlap killed a live session's audio. Fixed: cancel
  the stream only when this start claimed the singleton (and hand the
  claim back), retire surfaces only when no other session is live. Pinned
  by failed_start_teardown_is_scoped_to_what_it_owns.
- **KB-271 · FIXED · the start-success probe read the folded is_recording
  mirror, so a failed lane start never rolled back (round 9 fleet pass 2,
  801d48d3).** The mirror stays true while any other lane records, so a
  start that failed under overlap looked successful; its machine wedged in
  Recording with a dead session record. Fixed: the probe reads the
  STARTING session's truth (its lane's recorder, or local_is_recording
  for the keyboard). Pinned by
  start_success_probe_is_scoped_to_the_starting_session
  (transcription_coordinator.rs:2285).
- **KB-272 · FIXED · a phone lane's stop could flip the shared recording
  mirror false mid-capture (round 9 W1-L1 review, 9e0124e2).**
  stop_remote_lane_recording's local fold inlined a Recording-only match
  while its three sibling folds counted Recording|Stopping, so a lane stop
  landing inside the Mac session's Stopping tail lied to the Escape gate,
  the local-LLM keep-warm idle timer, and the failed-start tray guard.
  Fixed: one pure local_capture_active predicate (audio.rs:436) at all
  four fold sites. Pinned by local_capture_active_counts_the_stopping_tail
  and stop_remote_lane_folds_the_mirror_through_the_shared_predicate.
- **KB-273 · FIXED · every stop-pipeline drain retired the shared tray and
  overlay unconditionally, so the first finisher hid the second phone's
  live session (round 9 W1-L1 review, 9e0124e2 + 94291724).** With phones
  A and B both live, the first drain flipped the tray to Idle and hid the
  card under B's still-live session (the overlay's show-generation guard
  only protects newer shows); cancel_current_operation and the hotkey
  load-failure teardown carried the same unconditional flip, escaping the
  first fix's drain-side pin. Fixed: retire_session_surfaces
  (actions.rs:191) skips both flips while any other session is live; all
  fifteen drains plus both siblings retire through it, the error-read
  delay preserved via the readable_notice flag. Pinned by
  stop_pipeline_drains_retire_shared_surfaces_only_when_no_other_session_is_live,
  hotkey_load_failure_retires_shared_surfaces_through_the_guard, and the
  no-direct-flip assertions in
  cancel_current_operation_scopes_singletons_to_the_cancel_target.
- **KB-274 · FIXED · the starting-session slot held the last phone lane
  forever, so its None-for-keyboard contract was false (round 9 W1-L1
  review, 9e0124e2).** Only begin_session wrote the slot, so after any
  phone session every keyboard start read a stale lane (inert solely
  because every consumer re-checks the capture source). Fixed: the
  coordinator's Start arm writes the slot for every start (begin_session
  for a lane, clear_starting_session for the keyboard). Pinned by
  the_starting_slot_reads_none_for_keyboard_starts_after_any_phone_session
  plus the Start-arm assertions in
  start_success_probe_is_scoped_to_the_starting_session
  (transcription_coordinator.rs:2319-2336).
- **KB-275 · PARTIALLY FIXED (gap 2 closed by the post-round-9 fleet;
  flipped round 11) · the Esc resolution cannot reach older live phone
  sessions (round 9 W1-L6 review).** Two gaps in the cancel resolution,
  read from code: (1) last_recording_lane is stored only at recorder
  start (audio.rs:1368 in this tree; fleet pass 2 re-read) and reset only in
  close_remote_stream (audio.rs:1594); the cancel lane arm removes the
  lane's entry without re-pointing the pointer, and it only fires while
  the local machine is Idle (audio.rs:2250; fleet pass 2 re-read), so after the newest
  phone session is cancelled OR merely stops, the next Esc resolves to
  CancelTarget::None and only bumps in-flight stop pipelines: a
  still-live older phone session is unreachable from the keyboard until
  close_remote_stream resets the pointer; with the keyboard recording,
  Esc always resolves to the keyboard no matter which session started
  later (documented design, so only the no-re-point half is a defect).
  This gap is the open residual. (2) CLOSED by the fleet commits that
  ran after this section was first written (81dae89a, 520a47e6, the
  cancel fan-out fix): Command::Cancel now carries the resolved lane and
  the coordinator arm resets exactly the cancelled lane's machine or the
  keyboard (transcription_coordinator.rs:1298-1325, source pin at
  :5130-5165), while cancel_current_operation scopes its singleton
  teardown to the same target (utils.rs:86-140, pinned by
  cancel_current_operation_scopes_singletons_to_the_cancel_target);
  verified against this tree in round 11. Fix shape for the residual:
  re-point last_recording_lane to another recording lane on cancel and
  stop, or resolve by most-recent start across live lanes.

## Still open after this cycle (anchors refreshed where re-read)

- **KB-028:** closed in the v1.6.5 residuals cycle (lane R1): stop()'s
  lane-recorder close now parks on the audio manager when a recording is
  live and runs at the last session end (companion/mod.rs:758-763 on the
  residuals tree; the pre-v1.6.5 anchor read mod.rs:750-755
  after the v1.6.4 rewrite and the fleet pass 2 re-read, and the anchor
  at round 9 time was mod.rs:604-609). The shape had survived the W1-L2
  rewrite, both fleet passes, and the v1.6.4 lane.
- **KB-029:** closed in the v1.6.5 residuals cycle (lane R1): the reset
  command now writes status.error and notices on failure exactly like the
  restart leg, and the panel toasts (fleet verdict at 43f4c092; the
  companion rewrite did not touch the reset legs).
- **KB-082:** the self-signed cert SAN still freezes the first LAN IP; the
  v1.6.0 manual-cert-pair mitigation stands (fleet verdict; cert.rs:31-33
  at 43f4c092).
- **KB-085 residual:** the rapid off/on port race (stop returns without
  awaiting the accept loop, whose only signal is the shutdown send at
  companion/mod.rs:751 in this tree; the anchor was mod.rs:600 before the
  v1.6.4 companion rewrite, :779 before the fleet pass 1 companion
  edits, and :763 before the fleet pass 2 edits; bind fails EADDRINUSE
  with no retry). The
  pre-auth deadline half was already fixed in 9655d8fc. (Round 11
  re-verified against the rewritten file, anchors refreshed by the fleet
  pass 1 companion re-read and refreshed again by pass 2: start() spawns
  the accept loop
  fire-and-forget at companion/mod.rs:678 and waits only on the bind
  outcome, stop() sends and returns at :746, restart() bridges the two
  with a fixed 150 ms sleep at :780, and the accept loop releases the
  listener only when its select breaks on the shutdown watch at
  server.rs:83 - so a rebinding start inside the sleep window can still
  lose the race, exactly the round 9 shape.)
- **KB-086 residual:** the two dead protocol fields. The zero-tests half is
  closed this cycle: server.rs now carries a tests module
  (companion/server.rs:952 after the fleet pass 2 re-read)
  from the W1-L0 work.
- **KB-155 residual:** CLOSED in the v1.6.5 residuals cycle (lane R2),
  together with the round 9 half: the cancel handler now checks
  cancelDownload's false result and the store error it records, and
  renders the row alert with the localized cancelFailed copy next to the
  delete halves (ModelsSettings.tsx:229-256; pinned in
  modelActionErrorPins.test.ts).
- **KB-236 residual: CLOSED in the v1.6.5 residuals cycle (lane R3).**
  The impl-switch teardown gap for an armed handy-keys recorder is closed
  at the command itself, so the invariant no longer depends on tab
  layout: change_keyboard_implementation_setting
  (shortcut/mod.rs:817) now force-stops an armed recorder BEFORE
  unregistering the old backend (the force_stop_armed_recorder call at
  shortcut/mod.rs:842 precedes the unregister_all_shortcuts teardown at
  shortcut/mod.rs:847), through HandyKeysState::force_stop_recording
  (shortcut/handy_keys.rs:527), which consumes the same stop path the
  recorder's own command wrapper uses. The refusal seam the round 11
  re-verification named stays as the pin anchor: handy_keys.rs:421-423.
  Pinned by kb236_the_impl_switch_force_stops_an_armed_recorder
  (shortcut/handy_keys.rs tests; arms a recorder through a mock-managed
  state, runs the switch's teardown, verifies the stop and its
  idempotence) and impl_switch_stops_an_armed_recorder_before_unregistering_kb236
  (shortcut/mod.rs tests; the ordering source pin).
- **KB-244 residual:** the post-processing row keys are still unindexed
  (api.model, timeout, keep-warm, provider rows; re-checked in this tree:
  no such keys in settingsSearchIndex.ts, whose pp section ends at the four
  keys before :141's requiresExperimental block).
- **KB-258:** CLOSED in the v1.6.5 residuals cycle (lane R2) via the
  label branch: per-lane prefs still freeze at session start
  (freeze_session, companion/mod.rs:467 after the fleet pass 2 re-read,
  pinned by session_prefs_freeze_per_lane_and_survive_the_stop_pipeline),
  but the phone surface now says so - the toggle row carries the
  companion.postProcessHint next-dictation hint across all 26 locales
  (companion/client/index.html:612, :740).
- **KB-261, KB-262, and KB-275 above** (KB-275 now reads PARTIALLY
  FIXED: the fleet commits 81dae89a/520a47e6 closed its lane-machine
  reset gap after this section was first written; the no-re-point half is
  the residual. Flipped round 11.)

## Correction log (this round)

- Round 8's queue reused KB-254, KB-255, and KB-256, ids round 7 had
  already spent on the post-processing truth pass (skip-detail, ASR
  preamble, Prompt Enhancer). Both rounds' entries stand as written; every
  v1.6.2 commit message cites the ROUND 8 meanings (wrong-phone attribution,
  reconnect registration race, rename strand), and this round's flips
  above follow that convention. Search with the round qualifier when an id
  hits twice.
- The fleet also re-verified the README against v1.6.0 behavior and found
  no material gaps (one trust-boundary nit, closed by fd63e677's security
  note); this round's W1-L6 pass then updated it for the v1.6.2 multi-phone
  session rules, the per-phone recents, and the history quarantine.
- This round was first written mid-cycle (09b31437, after W1-L5, before
  the verification passes); the section as it stands is the final record:
  the fleet passes, the W1-L1 review, and the W1-L4 follow-ups are folded
  in with their own ids (KB-263 through KB-274), the fixed-this-cycle pin
  anchors were re-read at HEAD (the closing commits moved several), and
  the still-open anchors were re-verified. The final W1-L6 pass also
  taught the README the phone-identity tag, the Esc resolution order under
  overlapping sessions, and the quarantine notice's master-toggle
  behavior.
- The W1-L6 review round caught two errors in this section's first final
  draft, both fixed here: the fleet-pass-1 residual parenthetical
  inverted the memory-gate fix (the code SAT in DETAIL_INTERPOLATING_CODES
  and the fix removed it; the set's members are the codes whose OS bodies
  re-localize with an empty detail, noticeMessage.ts:267-271), and the
  README's first Esc sentence claimed a one-at-a-time teardown the
  resolution logic does not deliver. The Esc resolution's own gaps,
  surfaced by that review, are filed as KB-275.

## Round 10 queue: the nineteen verified lows kept open after the v1.6.2 fleets (2026-10-12)

All verified by the fleet consolidators with anchors; each is low severity and
ships disclosed in v1.6.2.

- KB-260 device-id fallback collision on no-crypto browsers (client index.html:646-657 + protocol.rs:171-176: timestamp-leading tag chars identical across devices for hours).
- KB-261 per-phone identity resets when the Mac's advertised origin changes (LAN IP change or Tailscale-vs-LAN pairing mints a new localStorage deviceId; Recent list silently resets; index.html:641-659, mod.rs:446-462, history.rs:616-624).
- KB-262 zeroAudio recovery copy is activation-blind ("Hold the button" under the default tap-to-lock; en companion.zeroAudio).
- KB-263 disconnect ack_remote_pause runs before any Stop raises the pause request, so drop-finalize always pays the full 2s timeout (server.rs:632).
- KB-264 utun corroboration is not Tailscale-specific; other utun VPNs with CGNAT addresses still widen the token to the /10 (ip.rs:82-92, 240-252; README tailnet framing understates).
- KB-265 digit-script guard tie-breaks against Latin and sanctions only dominant-script carry-over, discarding valid polish on mixed-digit text (post_process_runs.rs:934-940, 985-987; worked trace in the fleet evidence).
- KB-266 cancel-wanted fold to Secure Input races outside the want-set lock (shortcut/mod.rs:229-242; stale armed or missing Carbon Escape shadow until the next event).
- KB-267 emerald-600 Granted labels still 3.64:1 in light (need emerald-700; AccessibilityOnboarding.tsx:463,523).
- KB-268 AppData/LogDirectory open buttons swallow {status:"error"} resolutions (bindings never throw; only transport errors caught; LogDirectory.tsx:47-61).
- KB-269 dotool split keeps literal \r on CRLF transcripts, typing stray carriage returns (clipboard.rs:682-696; test covers \n only).
- KB-270 per-lane bookkeeping grows without bound (coordinator lanes map never pruned, dead lane_sessions until server stop, cancel generations never removed; flapping phone = monotonic growth).
- KB-271 historyDbRecovery swallows {status:"error"} pulls with no log anywhere, so the KB-179 notice can silently never post (historyDbRecovery.ts:52-66).
- KB-272 push_remote_chunk drops admitted audio silently when a lane's recorder is closed, with zero tests at the only call site (server.rs:599, audio.rs:1470-1479).
- KB-273 lid watcher restarts the stream on every flip even with no clamshell mic configured, under the state mutex (audio.rs:1843-1861; the UI-freeze class).
- KB-275 opener:allow-open-path granted to the webview but unused by any frontend code (capabilities/default.json:9); drop or scope it.
- KB-276 plural parity model misses the two secureInput count families and liveLogs.lineCount; wrong grammar in ru/uk/pl/cs/he/ar plus "1 lines" in English (pluralCoverage.test.ts:39-46).
- KB-277 README streaming-overlap sentence contradicts the singleton-engine design (first session keeps streaming; README.md:264-265 vs actions.rs:1880-1888).
- KB-278 stale notice-routing comments describe pre-cycle behavior (memory-gate body stripping removed; history_db_recovered click target exception undocumented; noticeRouting.ts:230-234, 391-393).
- (Plus the carried KB-254..259 six-pack from round 8, unchanged this
  cycle - though only KB-258 of it was still open when this queue was
  cut: 254, 255, 256, 257, and 259 already carried their FIXED markers
  in the round 8 entries above. Clarified round 11.)

---

# Round 11 (v1.6.4 stabilization cycle, 2026-10-13, branch cycle13/v1.6.4-stabilization)

A reconciliation pass, not a review fleet: the ledger was re-verified
against the code this cycle landed on (base d9647255, the v1.6.3 merge,
plus this cycle's six commits), every status flip cites the commit or
path:line it was verified against, and the copy and commit lint ran
clean. One inter-cycle record first: the v1.6.3 companion-proof cycle
(cycle12, merged as d9647255) never got a ledger section. Its commits
are acb656d2 (the phone resampler's NaN stream silenced every session),
fb3ffb76 (the post-save recent-history refresh moved off the stop
pipeline onto a blocking worker after proof runs reproduced the stop
task wedging between the history write and transcript delivery), and
bbaf7ed6 (release-macos.sh exports the full Xcode SDKROOT when unset);
none touched a KB item, so no statuses moved.

## The named reconciliations

- **KB-236 · CLOSED in the v1.6.5 residuals cycle (lane R3); flipped
  from OPEN with refreshed anchors (see the round 10 residual entry
  above).** The impl-switch teardown gap is closed: the switch command
  force-stops an armed recorder before unregistering (the
  force_stop_armed_recorder call at shortcut/mod.rs:842 precedes the
  teardown at shortcut/mod.rs:847), the state can no longer survive a
  round trip armed, and the next start can no longer refuse with
  "Already recording" (handy_keys.rs:421-423 stays as the pin's
  refusal-seam anchor). The round 5 and round 9 markers stand as
  history.
- **KB-237 · FIXED, verified against this tree.** The pin
  mode_flip_may_close_local_stream is live (managers/audio.rs:300, call
  site :1202, pin cases :3026-3036; anchors fleet pass 2 re-read). The round 5 and round 9 W1-L5
  markers stand.
- **KB-254 · FIXED (round 8 meaning), verified against this tree.** The
  zero-audio detail names the stopping session's own device, with the
  KB-254 note at the emit seam (actions.rs:2282-2296); per-lane identity
  is pinned by session_identity_freezes_per_lane_and_cannot_be_renamed
  (companion/mod.rs:1543). Round 8's fixed-by-construction marker
  stands.
- **KB-255 · FIXED (round 8 meaning), verified against this tree.** The
  lane-plus-generation release rule is pinned by
  registrations_release_by_lane_and_generation_never_by_name
  (companion/mod.rs:1798 after the fleet pass 2 re-read). Round 8's
  marker stands.
- **KB-241 · FIXED, verified.** No is_model_loading reference remains in
  any shipped source (grep over src/, src-tauri/src/, and the generated
  bindings; the only repo hit is a .zcode workflow draft artifact, which
  is session scratch, not shipped code). Round 9's removal marker
  stands.
- **KB-245 · FIXED, verified.** searchKeyboardReducer is imported and
  dispatched in SettingsSearch.tsx:9 and :108. Round 9's marker stands.

## Stale markers flipped this pass

- **KB-275 flipped to PARTIALLY FIXED in round 9 above.** Round 9's
  entry was finalized before the v1.6.2 fleets finished; the fleet
  commits 81dae89a and 520a47e6 closed its gap (2), and the entry now
  says so with this tree's anchors (the scoped coordinator arm at
  transcription_coordinator.rs:1298-1325 with its source pin at
  :5130-5165, and the scoped singleton teardown at utils.rs:86-140).
  Gap (1), the last_recording_lane no-re-point, is the open residual
  (stored only at audio.rs:1368, reset only at :1594, read only while
  Idle at :2250; anchors fleet pass 2 re-read).
- The round 10 tail's carried six-pack line was cut while five of the
  six round 8 entries already read FIXED; it now says so. Only KB-258
  was and is open (re-verified: the phone's "Polish text" label,
  client/index.html:554-557, still carries no next-session hint).
- Round 9's KB-028 anchor refreshed to the current tree
  (the fleet pass 2 companion edits left it at mod.rs:750-755;
  the v1.6.5 residuals fix moved the close block to companion/mod.rs:758-763).

## Round 10 closures recorded (this cycle's lanes; round 10 meanings)

Six commits: 645e3eff and cba077a9 (the companion lane plus its review
pass), cdb14c51 (the validator wave), 5223d8a2, e7f9abc5, and 560440a2
(the frontend lows, their review pass, and the numeral follow-up).

- **KB-260 · FIXED (645e3eff).** The no-crypto fallback device id leads
  with entropy (client/index.html:660-672, protocol.rs:186), so
  same-hour phones stop sharing an identity tag.
- **KB-261 · FIXED (645e3eff + cba077a9).** A persisted known-identity
  set survives a Mac origin change (companion/mod.rs:213, seeded at
  :584, persisted at :1072; anchors fleet pass 2 re-read after the pass 1
  rewrite removed adoption) and identity resolution is atomic with
  registration under one critical section (companion/server.rs:526,
  barrier race pin at companion/mod.rs:1945).
- **KB-262 · FIXED (5223d8a2; the mechanical fix landed in v1.6.3 under
  the same id).** The zeroAudio recovery advice is activation-aware,
  all 26 locales ship the variants, locked by the KB-262 block in
  companionClientPage.test.ts.
- **KB-263 · FIXED (645e3eff, corrected by cba077a9).** The disconnect
  path performs no pause ack at all (the eager ack was a no-op and is
  removed; the producer-gone close is the ack), pinned across server,
  audio, and recorder tests (the no-ack contract lives in
  finalize_disconnected_connection, companion/server.rs:313 after the
  fleet pass 2 extraction, source pin :1391).
- **KB-265 · FIXED (cdb14c51).** The digit-script guard keeps a strict
  tie leader and sanctions carried digit runs verbatim; pinned at
  post_process_runs.rs:1516.
- **KB-266 · FIXED (cdb14c51).** The cancel-wanted fold rides the
  want-set lock hold; pinned at shortcut/mod.rs:3218.
- **KB-267 · FIXED (5223d8a2 + e7f9abc5).** The Granted labels reach
  5.30:1 in light and the pin now computes the WCAG ratio from the real
  theme tokens and sweeps every non-test source under src for a
  surviving text-emerald-600 (AccessibilityOnboarding.test.ts:94-176;
  range refreshed by the KB-288 pass, which moved the hex table into
  lib/utils/wcagContrast.ts).
- **KB-268 · FIXED (5223d8a2).** The AppData and LogDirectory open
  buttons toast on {status:"error"} resolutions; pinned by
  directoryOpenErrorPins.test.ts.
- **KB-269 · FIXED (645e3eff).** The dotool split normalizes CRLF and
  lone CR before typing (clipboard.rs:682, pin :1268).
- **KB-270 · FIXED (645e3eff; consumption point moved by fleet pass
  2).** The lane record is removed by its stop pipeline's handoff
  (note_stopping_session, companion/mod.rs:1274 - the pass 2 fix for the
  cancel-vs-Stop interleave that used to strip the record before the
  pipeline could read it; pin :2077) and the disconnect retires the
  lane's coordinator machine and bookkeeping
  (companion/server.rs:297 inside finalize_disconnected_connection,
  source pin :1446).
- **KB-272 · FIXED (645e3eff + cba077a9).** push_remote_chunk routes
  through the free core with real-lane-recorder coverage: round trip,
  lane isolation, and the removed-lane false (audio.rs:468,
  companion/server.rs:436, pins :1278 and :1332; anchors fleet pass 2
  re-read).
- **KB-273 · FIXED (645e3eff).** The lid-driven capture restart is
  gated on a configured clamshell microphone before the state lock
  (audio.rs:1880-1894, source pin :2552).
- **KB-275 · FIXED (5223d8a2).** The opener:allow-open-path capability
  is gone from capabilities/default.json and the KB-181 comment states
  the real gate (commands/mod.rs:98-108).
- **KB-276 · FIXED (5223d8a2 + e7f9abc5 + 560440a2).** All nine count
  families interpolate the count where CLDR fires one at 21 and up, and
  the plural coverage numeral facet covers every family as a static
  rule (pluralCoverage.test.ts).
- **KB-277 · FIXED (cdb14c51).** The README streaming-overlap sentence
  states the singleton-engine truth (README.md:265).
- **KB-278 · FIXED (cdb14c51).** The notice-routing comments match
  reality: the memory-gate body keeps its interpolated amounts with the
  reason named at the seam, and history_db_recovered's History click
  target is documented at the click seam and in App.tsx.

Still open from round 10: **KB-264** (any utun interface corroborates
Tailscale, companion/ip.rs:86-91 in this tree; the file is untouched
this cycle) and **KB-271** (historyDbRecovery still swallows
{status:"error"} pulls with no log anywhere, historyDbRecovery.ts:52-60;
untouched this cycle).

## Round 9 open list, re-verified this pass

KB-028 (closed in the v1.6.5 residuals cycle, lane R1 - the close block
the fleet had pinned at mod.rs:750-755 after fleet pass 2 now
parks and re-runs; the residuals-tree anchor is
companion/mod.rs:758-763),
KB-029
(closed in the v1.6.5 residuals cycle, lane R1 - the reset legs
the fleet read at mod.rs:905-918 after fleet pass 2 live at
companion/mod.rs:915-930 on the residuals tree; reset_pairing itself is
unchanged, the fix landed in the command and the panel), KB-085 residual (re-verified against
the REWRITTEN companion file, not waved off: this cycle's diff touched
companion/mod.rs and server.rs heavily, so the round 9 anchor was
re-read - the residual holds, with the shutdown send now at
companion/mod.rs:751 (fleet pass 2 re-read), the fire-and-forget accept
spawn at :673, the
fixed 150 ms restart sleep at :780, and the accept loop's shutdown
break at server.rs:83; the round 9 entry above carries the refreshed
anchor), KB-082 and KB-155 residual (their anchored files, cert.rs and
the model-settings sources, are genuinely untouched by this cycle's
diff), KB-086
residual (protocol.rs's only change this cycle is the KB-260 doc
comment, so the dead fields stand), KB-244 residual
(settingsSearchIndex.ts untouched this cycle), KB-258 (above), KB-261
(both ggml-building crates still linked, Cargo.toml:92 and :102),
KB-262 (closed in the v1.6.5 residuals cycle, lane R2: the probe now
returns the tri-state; this round 11 note records the v1.6.4 state, when
commands/mod.rs changed only its KB-181 comment), and KB-275's gap-1
residual (above).

## Copy and commit lint

- Marketing-vapor sweep over every en locale string (including the
  companion.\* phone page keys, which scripts/embed-companion-strings.ts
  embeds from the same files), the phone page's inline English
  fallbacks, and the README: zero hits for seamlessly, effortlessly,
  elevate, delve, cutting-edge, game-changing, state-of-the-art, and a
  wider net (unleash, empower, supercharge, revolutionize,
  next-generation, blazing, redefining, intuitive, frictionless, and
  more). The one near-hit, "amazing" in the transcribe.cpp credits line
  (en translation.json:1133), is a thank-you to upstream authors rather
  than product vapor and stays as written.
- Last 200 commit subjects: 0 contain an em or en dash (verified twice,
  once with grep and once with an independent perl unicode pass), and 0
  commit bodies carry a co-authored-by, generated-with, or other AI
  trailer. All 200 commits are downstream-era work by the operator (177
  nexiouscaliver, 23 Shahil kadia; the operator's first commit is
  2026-10-07 and the window starts 2026-10-09); the upstream fork era
  (572 commits by CJ Pais on this history, 2025-02-03 through
  2026-10-07, plus other upstream authors) never enters the window, so
  no carve-out was needed.

## Round 12 (fleet pass 1 fixes, 2026-10-13)

The fleet review pass confirmed six findings on this branch; all six are
fixed in one commit, each with its own pin (every pin mutation-checked by
reintroducing its defect and watching it trip).

- **KB-261 connect-edge lost update · FIXED (fleet pass 1).** The
  first-pairing hello spawns persist_known_identities and
  note_device_connected as two detached workers, and each did
  get_settings -> mutate one field -> write_settings over the WHOLE
  settings object, so the interleave [persist reads][connected reads]
  [persist writes][connected writes] let the last whole-settings writer
  erase the other's field - and both fire together exactly when a first
  pairing mints an identity and changes the last device. Both cycles now
  route through CONNECT_EDGE_SETTINGS_RMW (companion/mod.rs:1033), whose
  guard spans the read through the write (companion/mod.rs:1049), making
  the persist doc's no-clobber claim true. Pinned by
  kb261_connect_edge_settings_rmws_serialize_through_one_lock
  (companion/mod.rs:2079 after the fleet pass 2 re-read; the real store cannot mount under a mock app,
  so the lock discipline is pinned on the source, the established
  convention). Mutation-checked by reverting note_device_connected to a
  direct get/write pair and watching the pin fail.
- **KB-261 platform-name adoption removed · FIXED (fleet pass 1).** The
  v1.6.4 adoption rule matched a fresh-id hello to a known identity on
  platform-name equality alone with only a not-currently-connected guard.
  That match key is client-controlled and shared by every same-platform
  phone, so any pairing-token holder helloing while the prior phone was
  away (asleep, off-WiFi - the normal steady state, and the common
  two-Android-phones case needs both hellos to merely say "Linux")
  received the prior's Recent list and transcript history on its socket
  and attributed its own dictations to the prior's capture_device: the
  KB-265 defect class between household token holders. Adoption also
  poisoned itself (an ordinary re-pair while the old socket lingers up
  to the 20 s keepalive deadline was refused, minted a fresh id, and
  pushed it to the known set, after which the platform match was
  ambiguous forever) and let bare no-id hellos adopt in both directions,
  violating the pinned protocol invariant that a client with no id keeps
  the bare name. Identities are now exact
  (companion/mod.rs:395, register_connection_with_identity); the known
  set stays as the durable pairing record (seeded at :584, persisted at
  :1048; anchors fleet pass 2 re-read). Disclosed cost: a Mac origin change resets that phone's Recent
  list, the pre-v1.6.4 behavior - the round 10 KB-261 UX half this cycle
  tried to fix stays open as a design question (a consent prompt on the
  phone is the shape that could bring adoption back safely). Pinned by
  kb261_identities_are_exact_and_never_adopt (companion/mod.rs:1861) plus
  a source pin that the adoption matcher cannot reappear
  (mutation-checked by re-adding the function and watching the pin
  trip); the round 11 KB-261 FIXED marker above records the v1.6.4
  commits' state and stands as history.
- **KB-268 onboarding mic settings · FIXED (fleet pass 1).** The two
  openMicrophonePrivacySettings handlers in AccessibilityOnboarding
  swallowed {status:"error"} resolutions (the bindings resolve a Rust Err
  instead of throwing; the command has real Err paths on every platform)
  - the KB-268 lane fixed this class for the AppData/LogDirectory
    buttons but missed the onboarding siblings in the file it edited. After
    a swallowed failure, handleGrantMicrophone armed the 60 s waiting
    spinner that no settings flip could satisfy and handleOpenMicSettings'
    button silently did nothing. Both now check result.status and toast
    (AccessibilityOnboarding.tsx:337 and :362), pinned by
    micSettingsErrorPins.test.ts executing the committed handler bodies
    against error, ok, and rejected resolutions, asserting the spinner
    never arms on failure (mutation-checked by stripping the status check).
- **ru/uk runHint_one engine claim · FIXED (fleet pass 1).** The studio
  Run hint counts the prompts x engines cartesian product, and CLDR
  ru/uk 'one' fires at 21, 31, ... 101, so "{{count}} шаблон на 1
  движке" asserted "21 templates on 1 engine" for multi-engine batches.
  Both one-forms now carry the pair shape (ru/uk translation.json:1199),
  and pluralCoverage.test.ts gains a fifth facet: ru/uk runHint must
  name the pair and carry no hardcoded numeral in any category
  (mutation-checked by restoring the old ru string and watching it fail).
- **KB-276 call-site half pinned · FIXED (fleet pass 1).** The locale
  half of KB-276 was pinned by pluralCoverage, but nothing executed
  SecureInputWarning's message expression, so a regression back to the
  hand-rolled blockedWithCulprit\_${suffix} key kept the suite green.
  secureInputCallSitePins.test.ts now executes the committed expression
  verbatim with a recording t() across all four status/count
  combinations and asserts the bare family keys with count riding the
  call (mutation-checked by regressing to a suffixed key).

### Round 12, fleet pass 2 (2026-10-13)

Six more confirmed findings on the same branch, fixed in one commit, each
pinned and mutation-checked (the mutation reintroduced the defect and
watched its pin trip; every restore re-verified green).

- **Welcome-failure leak · FIXED.** The only post-registration early
  exit - the Welcome send failing because the phone died between hello
  and welcome (companion/server.rs:614) - was a bare silent return that
  skipped the ENTIRE disconnect finalize: the connections entry survived
  until server stop (a phantom phone in the tray's paired count) and the
  KB-270 lane machine, post-process mirror entry, and cancel generation
  leaked forever. The whole teardown now lives in
  finalize_disconnected_connection (companion/server.rs:274) and BOTH
  exits route through it (the session loop's tail and the
  welcome-failure arm, which also logs like every other exit on the
  path). Pinned by
  welcome_send_failure_finalizes_the_lane_instead_of_leaking_it
  (companion/server.rs:1596; mutation-checked by restoring the bare
  return).
- **Stale ledger anchors · FIXED.** The pass 1 companion rewrite moved
  13+ companion/mod.rs and companion/server.rs citations in the round
  10/11 sections (and KB-086's tests-module anchor had been stale since
  the v1.6.4 rewrite), while the goldmine pin live-checked exactly one
  anchor. Every companion citation in the ledger is refreshed against
  this tree (each marked with its re-read), and the pin now
  live-checks ALL of them: an anchor table maps every cited
  companion-file span to the token its lines must carry, the
  enumeration check forces every NEW citation through a table row, and
  dead rows fail. Mutation-checked both ways: shifting a ledger line by
  one trips the enumeration check; moving the code without the ledger
  trips the token check. Anchors outside the companion module remain
  outside the pin's scope (disclosed).
- **KB-270 review: the cancel-vs-Stop interleave · FIXED.** The session
  record was removed at on_lane_session_changed(false) on the assumption
  the stop pipeline had already consumed the identity, but the cancel
  path false-edges the session BEFORE its pipeline runs
  note_stopping_session - the handoff found nothing, and the stop
  pipeline fell back to companion_last_device (the most recent phone to
  hello, possibly a different phone), attributing the dictation's
  history row and Recent routing to the wrong identity. The record now
  survives the false edge as live=false and is removed at the
  consumption point (note_stopping_session, companion/mod.rs:1274);
  stop()'s retain sweep is gone entirely (it removed exactly the
  pending-consumption records), so every record still present at server
  stop belongs to a pipeline that will consume it. The false edge with
  no record (the normal post-consumption stop) still sends the phone's
  State frame and clears the overlay instead of returning early.
  Pinned by
  server_stop_leaves_records_for_their_pipelines_in_both_stop_orders
  (companion/mod.rs:1666),
  stop_leaves_the_session_map_to_its_pipelines (companion/mod.rs:1742), and the
  reworked kb270_lane_session_records_are_consumed_at_every_session_end
  (companion/mod.rs:2196); mutation-checked by restoring the edge removal.
- **Accept-wedge hardening, closed seam · FIXED.** The Welcome frame's
  settings read ran get_settings synchronously ON the async connection
  task - a read-modify-write over the plugin store's mutex (migration
  and binding-merge write-backs), the exact blocking class the
  hardening moved off the task, one line after the workers it created
  for that purpose. The read now rides
  tauri::async_runtime::spawn_blocking and the task awaits the handle
  (a failed read falls back to the shipped defaults). The pin's blanket
  claim ("every synchronous seam ... never inside the async task") is
  scoped to the seams it actually enumerates, the four spawn_worker
  seams plus the welcome read. Pinned by
  connection_blocking_seams_run_off_the_async_connection_task
  (companion/server.rs:1476; mutation-checked by restoring the inline
  read).
- **Phone-path lock poison tolerance · FIXED.** route_remote_chunk (the
  KB-272 core that runs on EVERY phone audio frame on the connection
  task) and drop_lane_cancel_generation (newly called from the
  coordinator thread's retire_dead_lane, whose catch_unwind logs once
  and exits with no restart) still locked with .unwrap(), the exact
  failure class the companion manager converted every one of its own
  Mutexes away from (a panic under try_start_remote_lane_recording's
  CoreAudio/VAD open poisons the map, and every later push panics its
  connection task). Both acquire poison-tolerantly now (managers/
  audio.rs:468 and :2101). Pinned by
  kb272_route_remote_chunk_survives_a_poisoned_lanes_guard (executes the
  real routing core against a deliberately poisoned guard) and
  phone_path_lock_sites_are_poison_tolerant (source slice;
  mutation-checked by reverting the unwrap).
- **Worker panics observable · FIXED.** The four detached
  connect/disconnect workers (persist_known_identities,
  note_device_connected, send_recent_history_to_lane,
  note_device_disconnected) were bare std::thread::spawn bodies whose
  panic output never reached the app's rotating log (no panic hook
  exists anywhere in src-tauri, and tauri_plugin_log captures log
  records, not panics) - a packaged .app launched from Finder showed
  nothing while the pairing record silently diverged. Every worker body
  now rides spawn_worker (companion/server.rs:252): catch_unwind plus an
  error log through the app's logger. Pinned inside
  connection_blocking_seams_run_off_the_async_connection_task (each seam
  must be introduced by spawn_worker with its name, and the helper must
  catch and log; mutation-checked by reducing the helper to a bare
  spawn).

## Round 12 queue: twenty verified lows kept open after the v1.6.4 fleets (2026-10-13)

All verified with anchors by the fleet consolidators; each ships disclosed in
v1.6.4. The full evidence for every entry lives in the stabilization run's
report (dwfrun-adf93b4e).

- KB-279 CLOSED in the v1.6.5 residuals cycle (lane R1): every audio-side remote_lanes and lane_cancel_generations acquisition now recovers poison through unwrap_or_else(|e| e.into_inner()) - 12 remote_lanes sites plus 4 lane_cancel_generations sites on the residuals tree (at find time only audio.rs:473 and 2089 were tolerant; the rest unwrapped), and the poison pin phone_path_lock_sites_are_poison_tolerant now enumerates BOTH MAPS wholesale (whitespace-stripped, so multi-line acquisitions count) instead of only the two fn slices the fleet pass converted.
- KB-280 cancel-after-LaneGone interleave leaks a deferred machine past KB-270's retirement (Command::Cancel resets a Recording machine that LaneGone deferred to a drain that never comes). transcription_coordinator.rs:1298-1325, 1382-1388.
- KB-281 CLOSED in the v1.6.5 residuals cycle (lane R3, hardened in the review round): the late register of the unregister-before-register inversion can no longer re-add a lane-keyed cancel want. register_cancel_shortcut (shortcut/mod.rs:262) gates lane keys on the lane's live companion session record through lane_keyed_register_may_add_want (shortcut/mod.rs:256) BEFORE the want-set add, and the review round moved the liveness read INSIDE the same CANCEL_WANTERS lock hold as the add - the first cut read liveness under the CompanionManager lock and added under CANCEL_WANTERS, so a teardown landing its record-flip AND want-release between the read and the add leaked the want exactly as pre-fix (read live, flip dead, remove absent, add). Under one hold the interleave closes: every want-releasing teardown flips the record dead BEFORE taking the want-set lock (the Escape teardown utils.rs:104-115 and the load-failure teardown managers/transcription.rs:2618-2620, both order-pinned), so a register either reads dead and is refused, or adds under the hold the release must wait for, leaving the want present for that release to remove; the wanters -> lane_sessions nesting is deadlock-safe because no path holds lane_sessions into the want-set lock (on_lane_session_changed drops the record lock before its sends). The record flips live on the start path before its register arm runs (actions.rs:1948 precedes actions.rs:2038); the keyboard's None key and manager-less contexts keep the legacy arm. The load-failure teardown now ends the cancelled lane's session record like the Escape teardown already did, before releasing the want (the W1-L0 rule from utils.rs). Pinned by kb281_a_dead_lanes_late_register_cannot_re_add_the_cancel_want (executable against a real CompanionManager), kb281_the_register_arm_gates_on_the_lanes_live_session_record (structural pin: lock acquisition precedes the liveness read, read precedes the add, the hold closes only after the fold), load_failure_teardown_ends_the_cancelled_lanes_session_record_kb281 (transcription.rs), and the order assertion added to cancel_current_operation_ends_a_cancelled_lanes_companion_session (utils.rs: flip precedes release). Residual, disclosed: a teardown that completes BEFORE the start path creates the session record (before actions.rs:1948) is invisible to the gate, because the record the register then reads is the fresh live one the start path inserts - that sub-window is the stale-live-record race, not the named inversion, and is bounded by the same session-record consumption rules (KB-270).
- KB-282 CLOSED in the v1.6.5 residuals cycle (lane R3): the default capability swaps opener:default for opener:allow-open-url plus opener:allow-default-urls (capabilities/default.json:8-9), so no arbitrary-path command (reveal-item-in-dir, open-path) is webview-reachable while every frontend openUrl call (https help, releases, and whats-new links) keeps working under the default URL schemes; the Rust-side OpenerExt calls bypass the ACL and stay untouched (the KB-181 open-path note at commands/mod.rs stands). Pinned by src/capabilitiesAclPins.test.ts (allow-open-url and allow-default-urls present; opener:default, reveal-item-in-dir, and open-path absent from the permissions array and the file).
- KB-283 CLOSED in the v1.6.5 residuals cycle (lane R2): the skipped/unchanged outcome legs and the live-log WARN tag carry amber-700 (PostProcessRunsTable.tsx:78, LiveLogViewer.tsx:55; 4.85:1 on the light background vs amber-600's 3.08:1), and the KB-267 contrast pin gained the amber family in the same pattern - the WCAG ratio is computed from a palette hex table, every non-test source under src is swept for a surviving text-amber-600, and both debug files must carry the amber-700/400 pair (AccessibilityOnboarding.test.ts:179; marker line moved by the KB-288 pass, which moved the hex table into lib/utils/wcagContrast.ts; mutation-checked by reverting one WARN tag to amber-600).
- KB-284 CLOSED in the v1.6.5 residuals cycle (lane R4): no production comment in audio.rs names the deleted ack_remote_pause any more. The three stale sites (found at managers/audio.rs:1557-1562, 2244, 2735 on the round 12 tree) now document the handshake that actually exists: the pause is acknowledged from the pushing side - every write routed through push_remote_chunk acknowledges an in-flight pause request under the remote_lanes lock, and the source close / producer drop acknowledges it for a vanished phone (the KB-263 rule: the source close IS the acknowledgement) - so the comments' lock-ordering rationale (stop outside the lanes lock) survives with true premises (managers/audio.rs:1630-1637, 2376-2383, and the source pin's own doc at 3113-3121). Pinned by kb284_no_comment_documents_the_deleted_ack_remote_pause_as_live (managers/audio.rs:3186-3205; the production slice of the file must not contain the token, so reintroducing a comment that names the deleted helper fails CI; the companion server's KB-263 pin keeps its historical pre-fix mention, which describes the past, not a live route).
- KB-285 get_settings runs inline on the connection task after the four spawned seams; its migration/salvage writes are the same wedge vector. server.rs:401, settings.rs:2143-2179.
- KB-286 CLOSED in the v1.6.5 residuals cycle (lane R2): every Arabic dual form that carried {{count}} beside the dual noun now uses the bare dual - listSummary, run, runHint, results, charsChanged, added, removed, blockedWithCulprit, blockedNoCulprit (ar/translation.json:713, 1229, 1235, 1248, 1261, 1270, 1276, 1441, 1445), the lineCount_two pattern; showAll_two keeps {{count}} because its parenthesized count badge has no noun to number. pluralCoverage.test.ts facet 6 pins that every Arabic _two count-family form stays bare (mutation-checked by restoring one "{{count}} dual-noun" form).
- KB-287 CLOSED in the v1.6.5 residuals cycle (lane R4): a content pin now reads README.md itself, so a revert of the KB-277 fix (cdb14c51) fails CI instead of silently restoring the false sentence. The pin slices the companion recording section and asserts the singleton-engine phrasing on all three clauses - the first session keeps streaming live text, a concurrently-started session transcribes at stop, and the streaming engine is a singleton - plus a negative assert that the exact pre-fix wording ("both record and transcribe at stop instead of streaming live text") cannot return (docs/quality-goldmine.test.ts:451-501; the code truth it mirrors is the ownership decision on the coordinator's serialized thread, actions.rs:1880-1888). Mutation-checked by reverting the README sentence in the working tree: the pin failed on the first clause, and passed again after restore.
- KB-288 CLOSED in the v1.6.5 residuals cycle (lane R4): the KB-267 and KB-283 contrast pins no longer hardcode the light background or the palette hexes. Both blocks now read them from one shared table, lib/utils/wcagContrast.ts (the module: contrastRatio at :35, themeHex at :45, TAILWIND_TEXT_HEX at :64), whose themeHex parses --light-color-background out of src/styles/theme.css at run time and fails loudly if the token is renamed or deleted, and whose Tailwind hex table is the single definition for the emerald and amber families (the repo's CSS never redefines those shades). The onboarding pin consumes it at AccessibilityOnboarding.test.ts:94-127 (emerald) and :179-253 (amber). Mutation-checked by repointing theme.css's --light-color-background at the dark hex #2c2b29: the emerald ratio assert failed with the moved measurement ("2.58:1 on #2c2b29"), proving the pin tracks the live token, and passed again after restore. accent.test.ts still carries its own THEME_BACKGROUND copy (out of this item's scope, disclosed; it measures the accent palette, not the text-on-background contrast families this item named).
- KB-289 CLOSED in the v1.6.5 residuals cycle (lane R3): all three whole-settings writers now ride the serialized RMW cycle instead of bare get/write pairs outside any lock. start()'s pairing-token rebind runs through connect_edge_settings_rmw (companion/mod.rs:593-612), reset_pairing persists its fresh token through the same cycle (companion/mod.rs:915-930), and set_log_level's persist routes through crate::companion::connect_edge_settings_rmw (commands/mod.rs:86-94). The helper stays under its historical name and lock (companion/mod.rs:1049) - the smaller blast radius versus lifting the guard to a store-level lock, which would touch every get_settings/write_settings caller; its doc now records the widened role and the KB-261 pins still pass unchanged. Pinned by kb289_whole_settings_writers_ride_the_serialized_rmw_lock (companion tests: all three sites carry the helper call, no bare whole-settings write remains in the reset_pairing/set_log_level bodies, and the rebind closure keeps computing staleness and minting the token).
- KB-290 stop-then-start re-seed can drop a freshly minted identity whose detached persist worker is mid-flight. companion/mod.rs:577-584, companion/server.rs:539-543.
- KB-291 companion_known_identities is behaviorally write-only machinery justified by a stale cost rationale; delete or wire it. companion/mod.rs:395-434, companion/mod.rs:1048-1075.
- KB-292 every hello/disconnect spawns uncoalesced workers (up to 3 + 1) and both edges rebuild the tray; a coalescing flag bounds the storm. companion/server.rs:543-655, companion/server.rs:655.
- KB-293 retire_dead_lane guards by correlation (Stage==Idle + connection-gone) not the IN_FLIGHT_PIPELINES registry; one registry check makes it by-construction. transcription_coordinator.rs:1186-1222.
- KB-294 CLOSED in the v1.6.5 residuals cycle (lane R1): the lane stop sizes its release-buffer window through lane_release_buffer_ms(lane_route_live(...), ...) - the disconnect edge unregisters the connection BEFORE queueing the forced finalize, so a route-gone lane pays NO tail (found at audio.rs:1397-1433 paying it); route-live keeps effective_release_buffer_ms exactly. Pinned by kb294_a_dead_lanes_route_pays_no_release_buffer_window (pure seam, all settings combos) and kb294_lane_stop_gates_the_tail_on_the_lanes_own_route (source pin: no direct effective_release_buffer_ms call may return to the lane stop).
- KB-295 companion_known_identities lacks the house serde round-trip/backfill test pattern. settings.rs test module.
- KB-296 retire_dead_lane polls the connections mutex after every dictation drain on the coordinator's hot loop; a deferred-retire set removes the lock. transcription_coordinator.rs:1191-1225, 1347.
- KB-297 the identity-list hydration seed does not clamp to KNOWN_IDENTITIES_LIMIT at the read boundary. companion/mod.rs:577-584.
- KB-298 the persist payload Vec is documented and pinned as the set to persist but every consumer ignores it; make it a flag or fix the docs. companion/mod.rs:125-133, companion/mod.rs:1063-1067.

## Round 13: the v1.6.5 residuals cycle record (2026-10-13, branch cycle14/v1.6.5-residuals, base origin/main 28f3b040)

Four lanes worked the round 12 queue and the carried residuals; every
closure flipped its entry in place above, and this section is the
cycle-closing record. All citations below carry the full
companion/xx.rs:N-M prefix form the ledger live-check pin requires (the
companion anchors were re-verified against the anchor table in
docs/quality-goldmine.test.ts, which passes at this tree).

- Lane R1 (bad4490b): KB-028 closed (the teardown's skipped lane-recorder
  close parks and runs at every session end, companion/mod.rs:758-763),
  KB-029 closed (reset-pairing failures surface on the restart error
  path), KB-279 closed (every audio-side remote_lanes and
  lane_cancel_generations acquisition is poison-tolerant, both maps
  enumerated by the poison pin), and KB-294 closed (a route-gone lane
  pays no release-buffer tail).
- Lane R2 (deb873a5): KB-155 closed (model delete and download-cancel
  failures route through the settings row alert, x26 locales), KB-262
  closed (the Apple Intelligence probe is a tri-state with a
  build-lacks string rendered in settings and studio, x26), KB-283
  closed (the amber legs carry amber-700 at 4.85:1), KB-286 closed (the
  Arabic dual forms are bare, pluralCoverage facet), and KB-258 closed
  (the phone Polish-text toggle labels the next dictation, x26).
- Lane R3 (2e556778, hardened in the review round 65757b06): KB-236
  closed (the impl-switch force-stops an armed recorder before the
  backend teardown), KB-281 closed and then hardened (the lane-liveness
  gate reads inside the same CANCEL_WANTERS hold as the want add; the
  disclosed sub-window is recorded in the entry), KB-282 closed (the
  default capability grants opener:allow-open-url plus
  opener:allow-default-urls, no arbitrary-path command), and KB-289
  closed (all three whole-settings writers ride the serialized RMW
  cycle, companion/mod.rs:593-612, companion/mod.rs:915-930, and the
  helper at companion/mod.rs:1049).
- Lane R4 (this lane's commit): KB-284 closed (no production comment in
  managers/audio.rs names the deleted ack_remote_pause; the three sites
  now document the push-side post-write ack plus the source-close
  producer_gone route, pinned by
  kb284_no_comment_documents_the_deleted_ack_remote_pause_as_live at
  managers/audio.rs:3186-3205), KB-287 closed (a content pin now reads
  README.md and asserts the KB-277 singleton-engine phrasing on all
  three clauses plus the absence of the pre-fix sentence,
  docs/quality-goldmine.test.ts:451-501), and KB-288 closed (the KB-267
  and KB-283 contrast pins read the light background and the palette
  hexes from the shared table lib/utils/wcagContrast.ts, which parses
  --light-color-background out of src/styles/theme.css at run time).
  KB-261 (the ggml duplicate-symbol warnings) is moved to
  DEFERRED-UPSTREAM with the full investigation in its entry above: no
  clean seam exists in either vendored sys crate at the pinned versions
  (llama-cpp-sys-2 0.1.159's system-ggml and ggml_cmake_dir seams both
  require a consumer transcribe-cpp-sys does not offer, the vendored
  ggml versions skew 0.24.0 vs 0.25.3, and the one local shape -
  building transcribe-cpp shared - is a release-posture change
  disproportionate to a cosmetic warning).
- Goldmine hygiene this round: two citations refreshed after lane R4's
  test-file edits moved the pinned blocks (the KB-267 emerald sweep now
  lives at AccessibilityOnboarding.test.ts:94-176 and the KB-283 amber
  marker at :179; both noted inline where they are cited).

Still open from the round 12 queue after this cycle, unchanged and
available for the next fleet: KB-280, KB-285, KB-290, KB-291, KB-292,
KB-293, KB-295, KB-296, KB-297, KB-298.
