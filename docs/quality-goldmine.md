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
commits a9e1159e..a6997aec) has since closed 17 of them — 12 P1 and 5 P2 —
with the entries updated in place below; five are partial (KB-020/033/148/154/190)
and keep a residual line.

## P0: the one that got away, now fixed

**KB-150 · FIXED · switching the selected microphone while a recording was live silently killed and discarded that recording.** As found: `update_selected_device` (audio.rs:1129-1139 ✓R4) invalidated the cache and, when the stream was open (always true mid-recording), did `stop_microphone_stream()` + `start_microphone_stream()`: the stop **discarded the samples** (`let _ = rec.stop()` at audio.rs:887-894 [R3]) and zeroed `is_recording` while the state machine still said Recording; on hotkey release the empty buffer hit the silent skip branch "Recording produced no audio samples; skipping persistence" (actions.rs:1433-1439 [R3]); overlay hidden, tray Idle, zero feedback; streaming sessions lost everything after the switch. The sibling `update_selected_channel` locked state and rejected exactly this with a comment explaining why [R3]: the asymmetry was visible in the same read ✓R4. UI unguarded: MicrophoneSelector.tsx:60-64 disables only for isUpdating/isLoading/empty list [R3]. No test coverage at the time (R3 ran the full audio-module filter → 47 passed, none on this path).

Fixed after round 3 in commit c8b0f890 ("fix(review round 3): switching the microphone mid-recording silently discarded the dictation"): `update_selected_device` now takes the state lock and rejects the switch while Recording or Stopping through the shared `capture_restart_forbidden` predicate (managers/audio.rs:1152), the same rule as `update_selected_channel`; the predicate is pinned by test over all three recording states (managers/audio.rs:1457-1467). Verified in code for this document.

No P0 remains open at v1.4.0.

---

## Fixed and verified in code

- **KB-002 · FIXED · cancel leaks the forced system mute.** `cancel_recording` now calls `self.remove_mute()` in **both** the Recording and Stopping branches, with a comment naming KB-002 (audio.rs:1319-1325, 1353-1360 ✓R4); covers the local always-on path, the remote branch (close_remote_stream has no restore), and the lazy-close deferral. R3 ran `cargo test --lib mute_restore` → 1 passed, `managers::audio` → 3 passed. **Follow-up: KB-151** (the quit path still strands the mute) — closed in cycle 3: a9e1159e makes `RunEvent::Exit` call `remove_mute()`.
- **KB-019 · FIXED · local post-process gate reads the SELECTED model.** `selected_llm_model_id(app)` now feeds the gate, with a comment naming KB-019 (actions.rs:859-871 ✓R4); same resolution as the swap runner. R3 ran `local_availability_follows_the_selected_model(_not_the_pinned_one)` → 1 passed; actions::tests 27; local_llm::manager 24. Do not re-report.
- **KB-103 · FIXED · always_on boot panic + poisoned setting.** Constructor's AlwaysOn open is best-effort (`if let Err(e) = … error!`, audio.rs:556-572 ✓R4); `update_microphone_mode` applies the runtime flip (on spawn_blocking) BEFORE persisting, with the rationale comment (commands/audio.rs:181-208 ✓R4); the store updater throws on error so the toggle rolls back (settingsStore.ts:106-114 ✓R4). **Follow-up: KB-104 residual + KB-179** (same boot-panic class, history side).
- **KB-104 · PARTIALLY FIXED.** `update_microphone_mode` is fixed (above); **`set_selected_microphone` still persists BEFORE applying** (commands/audio.rs:246-263 ✓R4: settings write at :250-254, `update_selected_device` after) and the generic rollback only reverts local state, so a failed switch leaves settings_store.json holding a mic the UI no longer shows until the next stream open self-heals [R3]. Much lower impact now KB-103 is fixed. Residual stays P2.
- **KB-005 · still FIXED, but its guard test is unwired:** historyLimitInput.test.ts registers 0 tests under `bun test`, no package.json script, not in CI (R3 ran both invocations); third instance of the KB-136 class.

Prior FIXED (unchanged, do not re-report): KB-001, KB-003, KB-004, KB-006, KB-007, KB-041-core, KB-096 (residual: FALLBACK_APP_VERSION '0.1.2' at appVersion.ts:9; re-verified [R3 ×2]).

**Narrowed in the final round:** KB-011 residual (no-model path only: re-verified [R3 ×2]); KB-013 residuals (show wipes notice + Info-tone fallback: re-verified [R3 ×2]); KB-083 residual folded into KB-085 [R3]; KB-025 **correction** (see below).

---

## P1, MEDIUM, OPEN (38 at v1.4.0 — 12 since fixed in cycle 3, entries updated in place)

**KB-008 · OPEN (re-verified 3× at 5bf5b893, mod.rs:1407-1428).** Post-process toggle off unregisters only `transcribe_with_post_process`; cycle key stays registered + system-swallowed while `CyclePromptAction::start` no-ops (actions.rs:1965-1978). Fix template: delete-last-word/undo/command-mode handlers all live-unregister (mod.rs:2110-2185 [R3]). Stale UI comment stands.

**KB-009 · OPEN.** `change_binding` registers without `binding_is_active` (mod.rs:391-414); disabled is opacity-only (SettingContainer.tsx:135); recorder onClick unguarded. Secure-input's shadow DOES consult binding_is_active (secure_input.rs:483-488); the intended rule exists [R3].

**KB-010 · OPEN.** No lid watcher (only `is_clamshell` caller is desired_microphone); AlwaysOn never switches on lid close; `set_clamshell_microphone` never restarts (commands/audio.rs:338-347) vs `set_selected_channel`'s guarded restart [R3 full mechanics].

**KB-012 · OPEN (fresh run in the final consolidation pass: exit 1, 15 hi/hi-Latn ambiguities; also re-run by R3).** Not wired into CI (ci.yml:100-109).

**KB-014 · OPEN (re-verified 2×).** Preview burns the full 1.2 s with overlay_style=none (actions.rs:1655-1686 gates only on the toggle; show_final_preview_overlay no-ops on None, overlay.rs:644-648). New copy angle: the row renders in the Output tab far from the overlay-style control; OFF is what the copy already promises [R3].

**KB-015 · OPEN (re-verified 2×).** model_load_failed interpolates raw English into the {{model}} slot; full memory-gate refusal sentence lands in the slot (transcription.rs:1953-1984, 234-268 [R3]).

**KB-016 · FIXED · companion codes reach the main window.** As found [R3]: companion codes had no main-window surface; `companion_session_capped` had **no NoticeCode variant at all** (enum transcription.rs:394-459); broadcast to phones only; disconnected is Info (no sound). Fixed in cycle 3: d540241b adds the `CompanionSessionCapped` NoticeCode (Info, detail = device name) and emits it Mac-side from the server cap site; a6997aec routes it through the App.tsx notice router (toast, plus a macOS notification when the window is hidden and the card could not render) with `overlay.notice.companionSessionCapped` copy in all 26 locales; the shared code→message mapping is pinned over the full enum by src/lib/noticeMessage.test.ts. Device-name detail on disconnect remains KB-163 (open).

**KB-017 · OPEN.** Failed reliable-paste chord silent on macOS+Windows AND drops the owed auto-submit Enter + clipboard write (settle requires receipt_seen, impossible after chord failure: macos.rs:142-155 [R3]).

**KB-018 · OPEN (static, all rounds: no dotool on this host).** Multi-line transcript written as one `type` line; later lines execute as dotool commands (clipboard.rs:691-692).

**KB-020 · PARTIALLY FIXED · card-less notices now have surfaces.** As found [R3]: Info-tone notices fired while the card is hidden were invisible on every surface; cycle-prompt was the worst case (pressed between sessions by design) (transcription.rs:458-476, 543-562; RecordingOverlay.tsx:441). Fixed in cycle 3: d540241b computes `card_visible` at emit time on OverlayNoticeEvent; a6997aec turns the App.tsx listener into a router — card-less notices toast (error tone for Error kind) and fire a macOS notification when the main window is hidden — via the shared mapping in src/lib/noticeMessage.ts, pinned over the full enum. Residual: three info codes are now routed hidden-only, and the fade-window TOCTOU stays open — `card_visible` is probed at emit time, so a notice emitted just before the card fades out is still marked card-visible and never routed.

**KB-021 · OPEN (all 26 locales re-swept by R3 python script: start_over=True, scratch_everything=False everywhere).**

**KB-022 · OPEN.** Commands-tab add/remove before defaults resolve (or after silent `.catch(() => {})`) persists ~29 zero-phrase commands (CommandsSettings.tsx:212-251; backend accepts, command_matrix.rs:378-424) [R3]. resetRow IS guarded: the odd one out.

**KB-023 · OPEN, sharpened: the retry refresh is HALF-done**: `update_transcription` rewrites post_process_prompt but none of the five summary columns, so one row half-reports two different runs (managers/history.rs:404-418 [R3]).

**KB-025 · OPEN + CORRECTION.** Cleanup emits no event (ghost rows; save-before-emit order at history.rs:296-307 [R3]); and the in-tab HistoryLimit/retention controls make ghost rows manifest inside the same view. **Correction: the R1/R2 claim "history_limit=0 pinned by test" is WRONG for this tree: no test anywhere covers cleanup** (R3 ran managers::history → 7 passed; grep found no cleanup test).

**KB-027 · FIXED · companion toggle applies before persisting and rolls back.** As found [R3]: `change_companion_devices_setting` persisted+Ok'd unconditionally (mod.rs:845-851) while the always_on updater directly above (landed in 5bf5b893) threw and rolled back. Fixed in a9e1159e: `companion::apply_enabled` returns Err on a failed start (logged, stored on the manager, noticed) and the command applies it BEFORE persisting, so the settings store rolls the toggle back instead of persisting an enabled state with no server behind it; the startup path stays best-effort.

**KB-028 · OPEN.** Disable-during-local-recording leaks the pre-warmed remote recorder; nothing re-triggers the close after the session ends (mod.rs:285-289; prewarm refuses only while live, audio.rs:1049-1056) [R3].

**KB-029 · OPEN, sharper.** Reset-pairing failures console.error only AND the restart leg calls manager.start() directly so status.error is never set: the panel renders a clean "Stopped" [R3].

**KB-031 · FIXED · change_theme_setting syncs the tray.** As found [R3]: the theme setter never rebuilt the tray, so the menu bar kept the previous theme's icon (mod.rs:940-959). Fixed in a9e1159e: `change_theme_setting` calls `tray::update_tray_menu` after `apply_window_theme` (which the icon lookup reads the window theme from), so the tray re-syncs on every theme change.

**KB-032 · OPEN, impact-sized.** Six text-white-on-accent sites; accent.ts ships white/gray/red/orange/amber dark fills that are light → near-invisible labels (e.g. #fafafa fill + white text) [R3].

**KB-033 · PARTIALLY FIXED · update checks fail closed while the lock is unknown.** As found [R3]: About button ungated, silent no-op (AboutSettings.tsx:49-55; updaterFlow.ts:332 exits before reveal :364); `updateChecksAllowed` failed OPEN while `updateChecksLocked` was still null — a locked install could run a real network check during the loading window. Fixed in 14369da8: the gate now fails CLOSED (`updateChecksLocked !== false` → not allowed) and the About check button mirrors it exactly (disabled while not known-allowed, locked/disabled tooltip). Residual: the inFlight drop and the tray lock-null window are now handled as well (the update-check gate module, this cycle) — recheck under AUD-10.

**KB-034 · FIXED · update_checks toggle rebuilds the tray.** As found [R3 ×2]: the toggle never re-synced the tray's "Check for Updates" enablement (mod.rs:1161-1181; en description promises exactly the tray behavior). Fixed in a9e1159e: `change_update_checks_setting` calls `tray::update_tray_menu` right after persisting, so the tray reflects the toggle immediately instead of after the next restart.

**KB-035 · OPEN + NEW ANGLE.** Release notes stop at 1.2.0.md vs version 1.3.0 (all three version files [R3]); modal reads only bundled files. **New: pre-key migrants see the stale 1.2.0 notes** (default_whats_new_last_seen_version migration blanks, settings.rs:953-955, 2002-2003 [R3]).

**KB-036 · OPEN.** Keyboard-impl switch mid-recording deterministically kills the cancel key; the promised reconcile is never scheduled (grep: only mod.rs:169/177/372) [R3].

**KB-045 · OPEN, sharpened.** No concurrent-start guard on either download path (model.rs:2288-2292, 2606-2610 [R3]); **cancel_download always returns Ok and clears is_downloading even when no token exists** (:3017-3028); after the token overwrite, Cancel flips the card back to Downloadable while a transfer still runs; a third click starts a third transfer [R3].

**KB-047 · OPEN.** DownloadCleanup::drop removes the token without ownership check (model.rs:603-616) [R3 ×2].

**KB-105 · OPEN (mechanics fully pinned by R3).** Delayed hide-after-error (2.6 s) hides the NEXT session's card: hide snapshots generation at CALL time (after the sleep), so a show during the window is overwritten; two doc comments promise the opposite; armed at four sites (actions.rs:1344/1755/1826, transcription.rs:2297). Fix shape: snapshot at ERROR time or re-check after sleeping [R3].

**KB-106 · OPEN.** Linux default config: failed paste invisible everywhere (App overlayNotice listener drops paste_failed; card never exists under style=none) [R3].

**KB-107 · FIXED · api-key change drops the stale cached model list.** As found [R3]: the api-key handler re-hydrated the OLD key's cached model list (`[]` treated as absent; base-URL handler clears the cache, api-key handler didn't: mod.rs:1487-1499 contrast :1441-1470). Fixed in a9e1159e: `change_post_process_api_key_setting` removes the provider's `post_process_model_lists` entry before persisting, so the dropdown refetches with the new key.

**KB-108 · OPEN (re-verified 2×).** Built-in template delete not durable: seed loop pushes missing ids on every read (settings.rs:1581-1604), resurrecting at the END of the catalog after selection was reassigned (mod.rs:1728-1752) [R3].

**KB-109 · OPEN (re-verified 3×).** History retry ignores the post-process master toggle: per-entry snapshot is the only gate; `active_post_process_provider` has no enabled check; retry sends the stored transcript to the CURRENT (possibly cloud) provider/model/prompt [R3].

**KB-110 · OPEN.** Second phone's press contaminates/cuts the shared session (per-connection trackers press unconditionally; one shared ring; shared binding) (server.rs:414-461) [R3; companion tests 24 passed, no two-connection coverage].

**KB-111 · OPEN.** Either phone dropping finalizes the shared session while the other streams (disconnect tests active_source==Remote only) (server.rs:492-519) [R3].

**KB-112 · OPEN.** Autostart lying toggle (apply_autostart returns (); all failures warn-only; command Ok unconditionally) [R3].

**KB-113 · OPEN (re-verified 2×).** Successful download never dismisses the loading toast (only the failure path reuses the id; sonner loading toasts never auto-dismiss) [R3].

**KB-144 · OPEN, upgraded to medium.** Search indexes experimental-gated rows (jump targets don't render) AND Companion Devices has no search entry at all [R3 ×3].

**KB-148 · PARTIALLY FIXED · update-flow prompts notify while the window is hidden.** As found [R3]: the auto-check 'ask' card + install-policy restart prompts lived only in the hidden window; no reveal, no tray badge. Fixed in a6997aec: the ask card, the download/install failure toast and the restart prompt each also fire a macOS notification (`notifyDesktop`) when `document.visibilityState !== "visible"`. Residual: the auto-check-failure path — runUpdateCheck's catch toasts only when `trigger === "manual"`, so a failed AUTO check stays console-only with the window hidden (addressed separately by this cycle's check-failure notify work).

**KB-151 · FIXED · exit restores the system mute.** Was [R3]: quitting during an active mute_while_recording session stranded the system-wide mute — `RunEvent::Exit` did tray::stop_ram_refresh + companion::shutdown only, no `remove_mute` (lib.rs:1202-1208 ✓R4); the tray Quit path reaches it via app.exit(0). Same user impact as KB-002 via a narrower window. Fixed in a9e1159e: Exit now calls `recording.remove_mute()` (best-effort; warn-only when the manager is gone) before the shutdown legs.

**KB-154 · PARTIALLY FIXED · arch allowlists are disjoint again.** Was [R3]: "granite" sat in BOTH KNOWN_ARCHES and LLM_ARCHES, breaking the documented disjointness both add-from-HF gates rely on (model_capabilities.rs:43 and :61 ✓R4): a granite LLM text GGUF registers as an ASR model; a granite ASR GGUF passes the post-process gate. Fixed in a9e1159e: bare `granite` removed from KNOWN_ARCHES (llama.cpp's LLM arch lives in LLM_ARCHES alone; transcribe-cpp's granite ASR family is the suffixed variants), with disjointness now pinned by tests in model_capabilities.rs and catalog/llm.rs. Residual: granite_nar/cohere arch-string mismatches remain (pre-existing catalog-metadata drift, not introduced by the fix).

**KB-155 · OPEN · NEW · model delete and download-cancel failures are invisible in the settings UI**: both handlers console.error only and ignore the store's recorded error (ModelsSettings.tsx:189-204; modelStore sets error + returns false), so a swap-guard refusal or fs error leaves the user with zero feedback after confirming a native dialog [R3].

**KB-159 · OPEN · NEW · rebinding cancel to a chord another binding already holds reports success but silently leaves cancel dead**: the branch does format-only validation, retires the old registration, returns Ok (mod.rs:346-379 ✓R4 shape; conflict rejection happens only at the deferred reconcile, whose register failure is error!-only at mod.rs:223-227); every future session retries and fails until the conflict is removed [R3]. (KB-001 follow-on.)

**KB-160 · OPEN · NEW · closing the settings window while the Tauri-backend shortcut recorder is armed leaves every global binding suspended with zero feedback**: suspension resumes only on commit/cancel inside the component; window close is prevent_close+hide (lib.rs:1142-1145) so the armed state persists hidden; hotkeys dead until the window is reopened and clicked. HandyKeys twin cleans up correctly (contrast at HandyKeysShortcutInput.tsx:175-183) [R3].

**KB-162 · FIXED · the batch final preview keeps the session's tail notice.** Was [R3]: the show listener's reset covered 'preview' (setNotice(null) + noticeTimer cleared), so a post-process skip emitted during transcribing was cut off before its designed 5 s read window — an error-sound skip (memory gate) showed nothing on the card exactly when the user was looking at the previewed text; the streaming branch kept the notice. Fixed in a6997aec: the reset now skips the notice (and its timer) when `overlayState === "preview"`; fresh sessions (recording/streaming) still start notice-clean and hide-overlay still clears it.

**KB-176 · OPEN · NEW · the whisper initial prompt is unbounded and puts custom words at the HEAD**: the default matrix's ~109 Insert phrases (English + Devanagari + romanized + CJK) join ~1.2k chars ahead of custom words with no cap; HF's get_prompt_ids truncates keeping the TAIL, so custom words are first to fall off for every non-Latin output language (transcription.rs:3663-3676, 2995-3012; engine contract read from transcribe-cpp-sys headers). Truncation not executed (no model run); boundlessness/head-placement are code-read [R3].

**KB-179 · OPEN · NEW · a corrupted or unwritable history.db panics the app at every launch**: `HistoryManager::new(...).expect(...)` in setup; new() propagates create_dir_all/Connection/migrations errors. Same class 5bf5b893 just fixed for audio; recovery requires deleting history.db outside the app. Medium (needs external corruption/disk-full, no in-app toggle arms it) [R3].

**KB-183 · OPEN · NEW · turning Experimental off is not a companion kill switch:** the LAN TLS server keeps running with its only control unmounted (AdvancedSettings gates rendering only; companion::init arms purely on companion_devices_enabled), re-arms every boot, findable by neither search nor tray [R3].

**KB-185 · FIXED · tray "Unload After → Custom…" switches mode before focusing.** Was [R3]: a silent no-op whenever the setting was a preset — the custom input rendered only when the stored value was already Custom (`customActive = isCustom(storedValue)` ✓R4 at ModelUnloadTimeout.tsx:41), so the promised focus landed on a null ref and nothing switched modes. Fixed in 14369da8: the tray-focus effect first switches the stored setting to custom (backend command + optimistic store update, seeding 90 s like the dropdown does) when a preset is stored, so the field has rendered by the time the await resolves and the ref is attached.

**KB-186 · OPEN · NEW · first-run onboarding's model step is a silent dead end offline:** the catalog-fetch-failure state renders only logo/subtitle/hotkey hint: no error, no retry, no skip: and onboarding gates the entire app (Onboarding.tsx:331,353; store error never read) [R3].

---

## P2, LOW / OPPORTUNITY / COHESION, OPEN

_Re-verified in round 3 (tag `[R3]`) unless marked `carried`:_

- **KB-024** history copies raw only vs tray/paste processed [R3]. **KB-026** limit no-op unless PreserveLimit + star exemption undocumented (de/zh/hi/ja spot-checked) [R3]. **KB-029** see P1. **KB-030** companion_port dead control [R3].
- **KB-037** raw backend English in toasts (25 locales); _carried_ (referenced by R3's KB-039 evidence; not directly re-read). **KB-038 · FIXED · command-mode no-session rides the notice channel.** Was a bare `command-mode-no-session` emit only the main window heard (re-verified ×2 [R3]); d540241b migrates it to `emit_overlay_notice(CommandModeNoSession)` — log + event + card visibility like every other no-session feedback — and a6997aec's router toasts/notifies it card-less, reusing the existing `app.commandNoSession` copy. **KB-039** reset toast drops the list [R3].
- **KB-040** custom sound theme silent; error-cue Stop fallback also unchecked (audio*feedback.rs full read [R3]). **KB-042** preview tooltip literal [R3]. **KB-043** 0.5 vs 1.0 flash [R3]. **KB-044** is_downloading wrongly cleared: \_carried* (R2-narrowed). **KB-046** 16 #[ignore] supervisor tests (R3 re-ran: 11 passed/16 ignored) [R3].
- **KB-048** word-correction f64 unclamped; consumed raw where a huge threshold rewrites ordinary words (mod.rs:1236-1241) [R3]. **KB-049** AccelerationSelector display mismatch [R3]. **KB-050** style→none never hides live card + frontend reconcile handles only language/placement [R3]. **KB-051** streaming preview indistinguishable from working: exact mechanism pinned (only two emit_stream_working sites; caret suppression) [R3]. **KB-052** arming affordances + aria 'cancel' (localized Cancel keys exist to lift) [R3]. **KB-053** Windows preview-size misclassification (static, all rounds) [R3]. **KB-054** dropdown flashes 'live' [R3].
- **KB-055** stale debug-gated claim [R3]. **KB-056** delay sliders inert with reliable paste [R3]. **KB-057** auto-submit × paste-method None [R3]. **KB-058** Dropdown English placeholder: instances now also: stale custom sound theme (SoundPicker builds Custom only when wavs exist) [R3]. **KB-059** send_edit_action dead + still-false doc (grep exit=1) [R3]. **KB-060** Linux probe forking (static) [R3]. **KB-061** generic paste toast + enigo init console.warn only [R3]. **KB-062** main-thread paste sleeps >10 s worst case [R3]. **KB-063** TypingTool probe staleness [R3].
- **KB-064** global timeout dead + wired command [R3]. **KB-065** verdict detail English (26 locales) [R3]. **KB-066** tray staleness: now THREE surfaces: prompt submenu (all five settings-side mutations silent), local-LLM submenu (post-process-model-changed has zero listeners), AND voice Models submenu (download-complete/model-deleted never rebuild; only model-state-changed does: lib.rs:468-478) [R3 ×4]. **KB-067** runs table binding column: _carried_. **KB-068** renamed built-in masked [R3]. **KB-069** reseed drops NAME; no rename-only test case (re-ran, confirms gap) [R3]. **KB-070** evicted-live run loses outcome line + starts leak [R3]. **KB-071** ${output} never validated; legacy path sends no transcript [R3]. **KB-072** timestamp-ms ids + false comment + unguarded Duplicate [R3]. **KB-073** versionLabel dead key (re-grepped: 26 locale files, 0 refs) [R3 ×2].
- **KB-074** spoken undo/paste inert vs copy in 26 locales [R3]. **KB-075** mid-session snapshot vs fresh finalize (fresh line evidence) [R3]. **KB-076** ASCII terminal punctuation on CJK/Hindi [R3]. **KB-077** filler \b never fires between Han [R3: bites only once a writer exists, cf KB-078]. **KB-078** custom_filler_words dead setting [R3]. **KB-079** lazy retention, three call sites [R3].
- **KB-080 · upgraded to medium [R3]:** silent play of missing WAV: reachable in one step from the "Open Recordings Folder" button. **KB-081** star toggle race + blind revert [R3]. **KB-082** cert SAN freezes first LAN IP (load_persisted ignores current IP, cert.rs:31-33) [R3]. **KB-085** pre-auth no deadline + rapid off/on port race (KB-083 residual folded here) [R3]. **KB-086** zero server.rs tests + two dead protocol fields [R3]. **KB-087** copy-last silent on all paths [R3]. **KB-088** debug-key auto-repeat [R3]. **KB-089** ghost Debug section (sharpened: neither renderer checks debug_mode) [R3]. **KB-090** dark: follows OS (+ log-surface override also media-query-only) [R3]. **KB-091** verify-state mislabel + light colors + no feedback on still-denied [R3]. **KB-092** AppData+LogDirectory light colors [R3 ×2]. **KB-093** no ErrorBoundary on settings tree [R3].
- **KB-094** failed install still shows "installed" prompt [R3 ×2]. **KB-095** confirm-card stacking [R3]. **KB-097** attribution literals [R3]. **KB-098** tray empty-string hole (re-measured clean today; gate counts "" as present) [R3 ×2]. **KB-099** translocation toast: mount-only, 15 s, expires unseen for start_hidden; comment's promise wrong; English race [R3 ×2]. **KB-100** only updaterPlatform.test.ts; **CI runs neither test:updater nor check:text-style** [R3 extended]. **KB-101** up-to-date path logs nothing [R3]. **KB-149** text-style omits release-notes + tsx literals (ran clean today) [R3].
- **KB-114** command-mode latch (re-verified ×2) [R3]. **KB-115** unclamped u64 family: now also paste*delay_ms/paste_delay_after_ms (five setters total; R3 re-read all bodies) [R3 ×2]. **KB-116** margin no upper bound [R3]. **KB-117** delete_model no in-flight guard, both layers (re-verified ×2) [R3]. **KB-118** keyboard-impl dropdown literals: instance family grows: **LogLevelSelector.tsx:8-13 same class** [R3]. **KB-119** SoundPicker no disabled prop [R3]. **KB-120** per-drag full-store writes: family now: volume + **all five Output-tab sliders + the two new post-process sliders (timeout/keep-warm) + word-correction slider** [R3 ×3]. **KB-121** position/style/language no settings-changed emit → inverted card (fix shape: one emit per command) [R3]. **KB-122** raw-English interpolation umbrella: instances re-confirmed across overlay notices, channel toast, companion panel+notice, verdicts (check:translations re-run, structurally blind) [R3 ×3]. **KB-123** Shift+F4 on macOS + setter accepts hidden methods [R3]. **KB-124** toggle-off no-op no feedback (re-verified ×2) [R3]. **KB-125** external script unvalidated [R3]. **KB-126** Windows auto-submit try_lock skip: \_carried*. **KB-127** final clipboard-write failure inverts signal [R3]. **KB-128** isNotAPostProcessModelError dead [R3]. **KB-129** allow_base_url_edit decorative (three hardcoded gates) [R3]. **KB-130** off-path grid flash [R3]. **KB-131** Apple chars_out bytes (changed_ratio ~3× for CJK) [R3 ×2]. **KB-132** prompt_test consumes toast budgets: extended: also the skip-reason budget AND arms the overlay "Polishing" chip during a Test [R3 ×2]. **KB-133** shared test state/panel [R3 ×2]. **KB-134** everyday-gate English-only [R3]. **KB-135** CJK ASCII punctuation (pinned by test) [R3]. **KB-136** assert-script tests never run in CI: now THREE instances: commandGroups, skipToastDedupe, historyLimitInput (R3 ran both invocations for two of them) [R3 ×2]. **KB-137** `|| 5` coercion [R3]. **KB-138** AudioPlayer aria [R3]. **KB-139** retry toast drops detail [R3]. **KB-140** no-confirm delete + page-1 reload [R3]. **KB-141** stale synthesized-release comments (one now factually wrong about stop()) [R3]. **KB-142** sync-command main-thread block: second instance: **change_keyboard_implementation_setting, up to 10 s on first handy_keys switch** [R3]. **KB-143** tray submenu sync rejections log-only [R3]. **KB-145** experimental-off hides but doesn't deactivate [R3]. **KB-146** degenerate submenu label [R3 ×2]. **KB-147** sidebar mouse-only [R3].

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
- **KB-171 ·** LLM catalog per-file trust anchors are dead for non-default quants: expected_sha256_for resolves only the default file's id, so alternate quants download unverified and skip the load-time check though the catalog pins them (catalog/llm.rs:198-219; every_file_is_fully_pinned test asserts the anchors exist) [R3].
- **KB-172 ·** BaseUrlField's disabled tooltip is hardcoded English AND wrong for its only reachable state ("managed by the selected provider": it renders only for custom) (BaseUrlField.tsx:20-22) [R3].
- **KB-173 ·** the four prompt CRUD handlers drop backend error statuses (specta resolves {status:"error"}, handlers never check); a refused add/update/delete/duplicate leaves the UI silently unsaved; worst on Update where the draft keeps showing the edit that never persisted (PostProcessingSettings.tsx:584-632) [R3].
- **KB-174 ·** user templates can never carry language/register/description (add hardcodes auto/General/empty; update takes only name+prompt; no editor controls); badges are read-only decoration and the script guard can never sanction a custom script-switch template (mod.rs:1538-1547) [R3].
- **KB-175 ·** the dictation-cancel sweep concludes live prompt_test runs as failed:cancelled (only history_retry exempted), dropping the test's real outcome from the runs table (post_process_runs.rs:566-573) [R3].
- **KB-177 ·** "Reset to default" on an unedited matrix (stored=null) silently converts the store to Some(snapshot); freezing the user out of future default-phrase additions; only Reset All restores (CommandsSettings.tsx:273-289) [R3].
- **KB-178 ·** Spoken Numbers only runs for English/Hindi-Hinglish output; the dropdown promises digit conversion in all 26 locales with no scope caveat (number_format.rs:98-110) [R3].
- **KB-180 ·** failed first-page history load renders the "No transcriptions yet" empty state (catch console-only; no error state/retry) (HistorySettings.tsx:113-114, 255-259) [R3].
- **KB-181 ·** "Open Recordings Folder" reveals-not-opens on macOS (NSWorkspace activateFileViewerSelectingURLs selects the folder in its parent); deliberate ACL workaround, residual is label-vs-behavior (commands/mod.rs:92-107; plugin source read; static) [R3].
- **KB-182 ·** a failed history save during dictation is invisible on every surface (both call sites log-only; text pastes normally but never appears in History) (actions.rs:1601-1612, 1812-1813) [R3].
- **KB-184 ·** the phone vibrates for every forwarded overlay notice but can display only three codes: any other code buzzes with no visible message (client/index.html:400-407; ~25 codes exist) [R3].
- **KB-187 · FIXED.** Was: onboarding permission cards used bg-white/5 — invisible fill in light theme (AccessibilityOnboarding.tsx:423, 482) [R3]. Fixed in 14369da8: both permission cards fill with `bg-background`, theme-safe in light and dark.
- **KB-188 ·** tray Post-process Prompt submenu offered unconditionally: with pp off, picking a template silently moves a checkmark that affects nothing (tray.rs:710-726) [R3].
- **KB-189 ·** nine dead footer updater keys (downloading, installing, preparing, updateAvailableShort, five portableUpdate\*) referenced by zero components but forced into all 26 locales by the parity gate [R3].
- **KB-190 · PARTIALLY FIXED.** Was: App Language and Update Policy rows missing from settings search while every other About row was indexed (SettingsSearch.tsx:168-176) [R3]. Fixed in 14369da8: both rows added to the static index, in AboutSettings tab order. Residual note: the gap was wider than the KB text — the Theme and Accent Color rows are now indexed as well (this cycle's search-index module), so the About index is complete; future About rows must be added on arrival.
- **KB-191 ·** re-enabling update checks mid-session fires an immediate silent auto check at toggle time (hasAutoChecked only latches when the guard previously passed) (UpdateChecker.tsx:38-49) [R3].
- **KB-192 ·** app language applies i18n.changeLanguage before persist; failed persist leaves webview in the new language while store/tray keep the old until relaunch: KB-104 family (AppLanguageSelector.tsx:30-33) [R3].
- **KB-193 · FIXED.** Was: About version row rendered a bare "v" while the version IPC was in flight (state starts empty; span ungated) (AboutSettings.tsx:22, 47) [R3]. Fixed in 14369da8: the span renders only once `version !== ""` — no flash.

---

## Verified solid (do not re-litigate)

[R3 positive verifications:] the secure-input subsystem + recording lifecycle end-to-end (monitor/fallback/tray badge, per-binding hold windows, coordinator parity, cancel-during-Processing); KB-019's fix resolution shared by all four surfaces (gate/Test Connection/tray/delete); history slice: show_history_model off path, retention=Never deletes nothing, failed settings writes roll back+toast, outcome tokens always resolve, retry can't hang on a stuck load, dates localized, asset protocol scoped; tray i18n scan clean across 26 locales; ThemeSelector/AccentColorSelector optimistic-apply solid; prompt-library test batch green (13 required templates, keep-language rule, one-time migration). R3 also ran green: actions::tests 27, local_llm::manager 24, companion:: 24 + finalize_companion 5, tray/autostart/portable/legacy_migration 59, managers::history 7, managers::audio 3, mute_restore 1, post_process_runs 13, prompt 11, engine_supervisor 11+16 ignored, clipboard 7 (one flaky: KB-165), check:translations, test:updater, historyLimitInput + localLlmRouting + postProcessModelCache direct scripts.

## Cross-cutting themes (updated)

1. **Hidden-window / hidden-card feedback gap**: still the largest class (KB-011 residual, 013 residuals, 029, 061, 087, 106, 124, 139, 143, 155, 180, 182, 184 + device-name KB-163). Cycle 3 landed the routing decision itself — `card_visible` + the App.tsx router with toasts and macOS notifications (d540241b + a6997aec) — closing 016/020/038/148 and 162 (020/148 keep residuals); retiring the legacy duplicate failure listeners is the remaining leverage here.
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
- **KB-200** unload-timeout settings writes (incl. the tray Custom flow's own
  seed) never rebuild the tray submenu (commands/transcription.rs:20/93).
- **KB-201** set_post_process_provider never rebuilds; the pp Model submenu
  appears/disappears only on the next unrelated rebuild (mod.rs:1638-1646).
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
- **KB-206** Theme + Accent Color rows still missing from settings search;
  the "every About row indexed" comment is now false (SettingsSearch.tsx:167).
- **KB-207** 300ms hide-unmap race: card_visible=true while the overlay
  already returned null; an info notice in the window is invisible on every
  surface (overlay.rs:731-742 vs RecordingOverlay.tsx:385).

## New findings (P3)

- **KB-208** (folds into KB-163) companion capped + disconnected both drop
  the device-name detail in the shared mapping (noticeMessage.ts:85-99).
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
- **KB-214** KB-136 family grows: shortcutGating.test.ts unwired; CI still
  never runs test:updater (now two tests) (ci.yml:106-109).
- **KB-215** silent value coercion: theme/sound-theme/overlay-position/style
  setters warn + substitute defaults yet return Ok (mod.rs:986-1123).
- **KB-216** write_settings is unobservable (no error surface); persist
  honesty stops at the apply leg (settings.rs:2127-2134).
- **KB-015 reach grew**: model_load_failed raw-English detail now also lands
  in the desktop-notification body via noticeMessage.

## Standing verdict

Cycle 3 closed 29 goldmine items with zero false fix claims. The open P1
queue is now: KB-194, KB-195, KB-109, KB-198, KB-186 (offline onboarding dead
end, re-confirmed HIGH by the UI reviewer). Highest-leverage wave-5 pattern:
honest hotkey commits (KB-194 + KB-198 + KB-199 + KB-212 share one file) and
the notification-gate rework (KB-195 unblocks the whole KB-148 surface).
