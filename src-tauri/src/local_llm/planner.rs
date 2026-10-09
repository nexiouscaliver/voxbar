//! The pure swap state machine (spec section 3).
//!
//! Zero I/O: states, signals, actions, a transition table, the timeout
//! constant table, and the terminal handoff rule as pure decisions, in the
//! codebase's pure-decision style (classify_busy_input, decide_memory_gate).
//! The executor (manager.rs) drives [`SwapPlanner::step`] with signals it
//! observes and performs the returned [`Action`]s; nothing here touches the
//! app, threads, processes, or clocks.
//!
//! The core invariant this machine encodes (spec L2): the voice model and
//! the local LLM are never in RAM together. The LLM load action is only
//! reachable AFTER the voice unload action completed, and every terminal
//! path either hands the loading slot to a voice restore load or drops it
//! with the app's ordinary lazy-load path intact.

use std::time::Duration;

use super::SkipReason;

/// Bound on the voice unload wait: the queued unload completes behind any
/// in-flight transcription, and 10s is generous for a worker exit while
/// still proving the phase cannot hang (reviewer finding R6/R13).
pub const VOICE_UNLOAD_TIMEOUT: Duration = Duration::from_secs(10);

/// Bound on the worker Load (model mmap + context creation).
pub const LLM_LOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound on one generation. The L11 token cap bounds the work; this bounds
/// the wall clock.
pub const GENERATE_TIMEOUT: Duration = Duration::from_secs(30);

/// Graceful window for the worker to exit after `Exit` before it is killed.
pub const WORKER_GRACEFUL_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Hard bound on waiting for the child to die after a kill.
pub const WORKER_KILL_WAIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Total budget over the try phases (Gating + UnloadingVoice + LoadingLlm +
/// Generating; reviewer finding R7). Teardown (5s graceful + 10s kill wait)
/// is recovery and runs OUTSIDE this budget. Worst-case paste delay is
/// therefore about 45s + 15s, versus unbounded under per-phase bounds alone.
pub const TOTAL_SWAP_DEADLINE: Duration = Duration::from_secs(45);

/// How often every phase's wait loop re-checks abort signals and deadlines.
/// Same cadence as the stop path's CANCELLATION_POLL_INTERVAL.
pub const ABORT_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Retry cadence for the lease and slot acquisition tickers.
pub const ACQUIRE_RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// Deadline for acquiring the swap lease (another swap may hold it).
pub const LEASE_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(10);

/// Deadline for acquiring the loading slot (another load may be in flight).
pub const SLOT_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// A phase's effective budget: never more than the phase bound, never more
/// than the remaining total (spec 3.3).
pub fn effective_budget(phase_bound: Duration, remaining_total: Duration) -> Duration {
    phase_bound.min(remaining_total)
}

/// The phases of one swap. One per swap invocation; `Done` is terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapState {
    Idle,
    Gating,
    UnloadingVoice,
    LoadingLlm,
    Generating,
    UnloadingLlm,
    RestoringVoice,
    Done,
}

/// Why an in-flight swap was aborted. `TotalDeadline` is the 45s budget
/// tripping (the user sees a timeout skip toast); the others are
/// "dictation wins" (a remembered press, a recording that started, or a
/// user cancel) and complete silently with the raw transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbortReason {
    PressPending,
    RecordingStarted,
    UserCancel,
    TotalDeadline,
}

/// The wait phases that carry their own bound (Gating is a fast probe and
/// is covered by the total deadline alone).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    LoadingLlm,
    Generating,
}

/// What the runner observes and feeds into the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signal {
    /// Kick the swap off: acquire the lease, the loading slot, then gate.
    Start,
    GateAllowed,
    /// The memory gate refused; `detail` is the formatted refusal with the
    /// real numbers (memory_gate_refusal_message output).
    GateRefused {
        detail: String,
    },
    /// The unload helper's done flag was observed within the bound.
    VoiceUnloaded,
    /// The 10s voice-unload bound tripped. The LLM must NOT load: voice RAM
    /// is not provably freed.
    VoiceUnloadTimeout,
    LlmLoaded,
    LlmLoadFailed {
        reason: String,
    },
    LlmGenerated {
        text: String,
    },
    LlmGenFailed {
        reason: String,
    },
    /// The worker process exited (gracefully or after a kill).
    LlmUnloaded,
    /// The worker ignored the kill and the 10s wait bound tripped.
    LlmKillTimedOut,
    /// The loading slot was transferred into a restore loader thread.
    RestoreHandedOff,
    /// The restore was skipped (voice not loaded before, or the
    /// model_unload_timeout is Immediately) and the guard dropped.
    RestoreSkipped,
    /// The swap lease stayed held by another swap past its deadline.
    LeaseDenied,
    /// The loading slot stayed held by another load past its deadline.
    SlotDenied,
    Abort(AbortReason),
    Timeout(Phase),
}

/// What the runner must do. Pure data; the executor gives them effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Acquire the swap lease via a 100ms retry ticker (10s deadline).
    TakeLease,
    /// Acquire the loading slot via try_start_loading retries (5s deadline).
    TakeSlot,
    /// Compute the forecast and run the memory gate with the voice-resident
    /// credit; produces GateAllowed or GateRefused.
    Gate,
    /// Unload the voice model on the unload helper thread and poll its done
    /// flag (bounded, abort-polled).
    UnloadVoice,
    /// Spawn the `--llm-worker` child process.
    SpawnWorker,
    /// Send the Load frame and await Loaded (bounded, abort-polled).
    LoadLlm,
    /// Send the Generate frame and await Generated (bounded, abort-polled).
    Generate,
    /// Kill the worker child immediately (generation is not resumable).
    KillWorker,
    /// Ask the worker to exit gracefully and poll its exit, escalating to a
    /// kill after the graceful window; bounded by the kill-wait deadline.
    WaitExit,
    /// Transfer the LoadingGuard into the restore loader thread
    /// (tm.restore_model_under_guard). Consumes the slot.
    RestoreHandoff,
    /// Drop the LoadingGuard normally (clears is_loading, wakes waiters).
    DropSlotGuard,
    /// Release the swap lease.
    ReleaseLease,
    /// Emit the post-process skip event with the given reason.
    EmitSkip {
        reason: SkipReason,
        detail: Option<String>,
    },
}

/// TERMINAL HANDOFF RULE (reviewer finding R14), as a pure decision: at any
/// terminal transition where the runner still holds the loading slot, an
/// in-flight recording forces a restore handoff (its own Start-path load was
/// suppressed by the slot, so the handoff reproduces the load it is waiting
/// on); no recording means a plain guard drop (nothing is waiting on the
/// slot). Paths that never took the slot get nothing: the busy slot belongs
/// to a load that is already serving the recording.
pub fn terminal_handoff(holds_slot: bool, is_recording: bool) -> Option<Action> {
    if !holds_slot {
        return None;
    }
    if is_recording {
        Some(Action::RestoreHandoff)
    } else {
        Some(Action::DropSlotGuard)
    }
}

/// The restore decision at RestoringVoice: restore when the voice model was
/// resident before the swap AND the unload timeout is not Immediately
/// (FinishGuard would drop it moments later anyway); a live recording
/// forces the restore regardless (the terminal handoff rule again).
pub fn should_restore(
    voice_was_loaded: bool,
    unload_immediately: bool,
    is_recording: bool,
) -> bool {
    (voice_was_loaded && !unload_immediately) || is_recording
}

/// The per-swap planner. Tracks which resources the swap currently holds
/// (lease, loading slot) so every terminal action list is complete: the
/// slot is always either handed off or dropped, the lease always released
/// once acquired.
#[derive(Debug, Clone)]
pub struct SwapPlanner {
    pub state: SwapState,
    lease_held: bool,
    holds_slot: bool,
    voice_was_loaded: bool,
    unload_immediately: bool,
}

impl SwapPlanner {
    /// `voice_was_loaded`: a voice model was resident when the swap started
    /// (tm.get_current_model() was Some). `unload_immediately`: the
    /// model_unload_timeout setting is Immediately.
    pub fn new(voice_was_loaded: bool, unload_immediately: bool) -> Self {
        Self {
            state: SwapState::Idle,
            lease_held: false,
            holds_slot: false,
            voice_was_loaded,
            unload_immediately,
        }
    }

    /// Whether this planner already reached its terminal state.
    pub fn is_done(&self) -> bool {
        self.state == SwapState::Done
    }

    /// The restore decision at RestoringVoice, over this planner's own
    /// captured context: restore when the voice model was resident and the
    /// unload timeout is not Immediately, or whenever a recording is in
    /// flight (the terminal handoff rule).
    pub fn restore_decision(&self, is_recording: bool) -> bool {
        should_restore(self.voice_was_loaded, self.unload_immediately, is_recording)
    }

    /// Refresh the restore context at the moment the swap actually unloads
    /// the voice model. The context captured at runner start can go stale
    /// across the lease/slot acquisition waits (bounded by their deadlines,
    /// up to ~15s in production): a voice-model load that completes in
    /// that window leaves a model resident that the swap WILL unload, so
    /// it must be restored afterwards; conversely a model that was unloaded
    /// by its own timeout in that window must not be re-loaded off a stale
    /// true. The runner calls this right before issuing the unload, while
    /// it already holds the loading slot, so no other load can slip in
    /// between the refresh and the unload.
    pub fn refresh_restore_context(&mut self, voice_was_loaded: bool, unload_immediately: bool) {
        self.voice_was_loaded = voice_was_loaded;
        self.unload_immediately = unload_immediately;
    }

    /// Bookkeeping: the runner confirms each acquisition as it happens, so
    /// terminal action lists release exactly what the swap really holds.
    /// Pure state driven by real events; no resource lives here.
    pub fn mark_lease_acquired(&mut self) {
        self.lease_held = true;
    }

    /// Bookkeeping companion to [`Self::mark_lease_acquired`] for the
    /// loading slot.
    pub fn mark_slot_acquired(&mut self) {
        self.holds_slot = true;
    }

    /// Feed one observed signal; returns the actions to perform and moves
    /// to the next state. `is_recording` is the runner's live reading at
    /// signal time (the terminal handoff rule consults it).
    ///
    /// Unreachable combinations (signals after Done, signals for a phase we
    /// are not in) are defensive no-ops: the machine never rewinds and
    /// never invents resource state.
    pub fn step(&mut self, signal: Signal, is_recording: bool) -> Vec<Action> {
        let state = self.state;
        match (state, signal) {
            // ---- Idle/Gating acquisition: Start emits the acquire-then-
            // gate sequence; LeaseDenied/SlotDenied can arrive while the
            // runner is still inside it (the planner sits in Gating) ----
            (SwapState::Idle, Signal::Start) => {
                self.state = SwapState::Gating;
                vec![Action::TakeLease, Action::TakeSlot, Action::Gate]
            }
            // Never held anything: raw result, no side effects, and by the
            // terminal handoff rule's never-held-the-slot row, no handoff
            // action either (the other swap's runner owes the handoff).
            (SwapState::Idle | SwapState::Gating, Signal::LeaseDenied) => {
                self.lease_held = false;
                self.holds_slot = false;
                self.state = SwapState::Done;
                Vec::new()
            }
            // Lease held, slot never taken: another load is already in
            // flight and serving any recording, so still no handoff.
            (SwapState::Idle | SwapState::Gating, Signal::SlotDenied) => {
                self.holds_slot = false;
                self.state = SwapState::Done;
                vec![Action::ReleaseLease]
            }
            // Cancelled before anything was acquired: nothing to unwind.
            (SwapState::Idle, Signal::Abort(_)) => {
                self.state = SwapState::Done;
                Vec::new()
            }
            (SwapState::Idle, _) => Vec::new(),

            // ---- Gating: the voice model is still resident and credited ----
            (SwapState::Gating, Signal::GateAllowed) => {
                self.state = SwapState::UnloadingVoice;
                vec![Action::UnloadVoice]
            }
            // Refusal: NO voice disturbance, NO LLM load. The voice model
            // is still resident, so the terminal rule's not-recording row
            // drops the slot (no restore needed); a live recording still
            // forces the handoff per the rule.
            (SwapState::Gating, Signal::GateRefused { detail }) => {
                self.state = SwapState::Done;
                let mut actions = vec![Action::EmitSkip {
                    reason: SkipReason::MemoryGate,
                    detail: Some(detail),
                }];
                actions.extend(self.terminal(is_recording));
                actions
            }
            (SwapState::Gating, Signal::Abort(reason)) => {
                self.state = SwapState::Done;
                let mut actions = deadline_skip(reason);
                actions.extend(self.terminal(is_recording));
                actions
            }
            (SwapState::Gating, _) => Vec::new(),

            // ---- UnloadingVoice: voice unload issued, exit pending ----
            (SwapState::UnloadingVoice, Signal::VoiceUnloaded) => {
                self.state = SwapState::LoadingLlm;
                vec![Action::SpawnWorker, Action::LoadLlm]
            }
            // 10s bound tripped: the LLM must NOT load (voice RAM not
            // provably freed). The queued unload finishes on its own.
            (SwapState::UnloadingVoice, Signal::VoiceUnloadTimeout) => {
                self.state = SwapState::Done;
                let mut actions = vec![Action::EmitSkip {
                    reason: SkipReason::EngineFailed,
                    detail: Some(
                        "voice model unload timed out; the local post-process engine did not run"
                            .to_string(),
                    ),
                }];
                actions.extend(self.terminal(is_recording));
                actions
            }
            // An abort here means dictation wins: the LLM never loads, the
            // in-flight unload completes on its own behind any in-flight
            // transcription, and the terminal rule decides the slot.
            (SwapState::UnloadingVoice, Signal::Abort(reason)) => {
                self.state = SwapState::Done;
                let mut actions = deadline_skip(reason);
                actions.extend(self.terminal(is_recording));
                actions
            }
            (SwapState::UnloadingVoice, _) => Vec::new(),

            // ---- LoadingLlm: worker spawned, Load in flight ----
            (SwapState::LoadingLlm, Signal::LlmLoaded) => {
                self.state = SwapState::Generating;
                vec![Action::Generate]
            }
            (SwapState::LoadingLlm, Signal::LlmLoadFailed { reason }) => {
                self.state = SwapState::UnloadingLlm;
                vec![
                    Action::EmitSkip {
                        reason: SkipReason::EngineFailed,
                        detail: Some(reason),
                    },
                    Action::KillWorker,
                    Action::WaitExit,
                ]
            }
            (SwapState::LoadingLlm, Signal::Timeout(Phase::LoadingLlm)) => {
                self.state = SwapState::UnloadingLlm;
                vec![
                    Action::EmitSkip {
                        reason: SkipReason::Timeout,
                        detail: Some("local model load timed out".to_string()),
                    },
                    Action::KillWorker,
                    Action::WaitExit,
                ]
            }
            // Dictation wins even mid-load: kill what exists, teardown.
            // The total-deadline abort additionally carries its skip.
            (SwapState::LoadingLlm, Signal::Abort(reason)) => {
                self.state = SwapState::UnloadingLlm;
                let mut actions = deadline_skip(reason);
                actions.push(Action::KillWorker);
                actions.push(Action::WaitExit);
                actions
            }
            (SwapState::LoadingLlm, _) => Vec::new(),

            // ---- Generating: Generate in flight (the poll-cancel window) ----
            // The runner validates the text (JSON, extraction, strip,
            // fidelity guard) when it observes the result; invalid output
            // folds to raw via EmitSkip during teardown. The unload path is
            // identical either way.
            (SwapState::Generating, Signal::LlmGenerated { .. }) => {
                self.state = SwapState::UnloadingLlm;
                vec![Action::WaitExit]
            }
            (SwapState::Generating, Signal::LlmGenFailed { reason }) => {
                self.state = SwapState::UnloadingLlm;
                vec![
                    Action::EmitSkip {
                        reason: SkipReason::EngineFailed,
                        detail: Some(reason),
                    },
                    Action::KillWorker,
                    Action::WaitExit,
                ]
            }
            (SwapState::Generating, Signal::Timeout(Phase::Generating)) => {
                self.state = SwapState::UnloadingLlm;
                vec![
                    Action::EmitSkip {
                        reason: SkipReason::Timeout,
                        detail: Some("local model generation timed out".to_string()),
                    },
                    Action::KillWorker,
                    Action::WaitExit,
                ]
            }
            (SwapState::Generating, Signal::Abort(reason)) => {
                self.state = SwapState::UnloadingLlm;
                let mut actions = deadline_skip(reason);
                actions.push(Action::KillWorker);
                actions.push(Action::WaitExit);
                actions
            }
            (SwapState::Generating, _) => Vec::new(),

            // ---- UnloadingLlm: teardown, never abandoned ----
            // A press during teardown is remembered by the coordinator; the
            // teardown itself is only accelerated (straight to kill). The
            // arm MUST end with WaitExit (or another signal-producing
            // action): the runner only refills a missing signal at
            // RestoringVoice, so an action list that leaves the state at
            // UnloadingLlm with no signal would spin forever holding the
            // lease and slot in release builds. Unreachable in the current
            // wiring (the WaitExit loop consumes aborts internally), kept
            // safe for any future producer that feeds Abort here.
            (SwapState::UnloadingLlm, Signal::Abort(_)) => {
                vec![Action::KillWorker, Action::WaitExit]
            }
            (SwapState::UnloadingLlm, Signal::LlmUnloaded)
            | (SwapState::UnloadingLlm, Signal::LlmKillTimedOut) => {
                self.state = SwapState::RestoringVoice;
                Vec::new()
            }
            (SwapState::UnloadingLlm, _) => Vec::new(),

            // ---- RestoringVoice: the runner decides handoff vs skip via
            // should_restore() and feeds the matching signal ----
            (SwapState::RestoringVoice, Signal::RestoreHandedOff) => {
                self.state = SwapState::Done;
                self.holds_slot = false;
                // The slot lives in the restore loader thread now; only the
                // lease remains for the runner to release.
                vec![Action::RestoreHandoff, Action::ReleaseLease]
            }
            (SwapState::RestoringVoice, Signal::RestoreSkipped) => {
                self.state = SwapState::Done;
                self.holds_slot = false;
                vec![Action::DropSlotGuard, Action::ReleaseLease]
            }
            (SwapState::RestoringVoice, _) => Vec::new(),

            // ---- Done: terminal; nothing more to do ----
            (SwapState::Done, _) => Vec::new(),
        }
    }

    /// The terminal action tail: the handoff rule's decision for the slot,
    /// then the lease release. Consumes both resources' bookkeeping.
    fn terminal(&mut self, is_recording: bool) -> Vec<Action> {
        let mut actions = Vec::new();
        if let Some(handoff) = terminal_handoff(self.holds_slot, is_recording) {
            actions.push(handoff);
        }
        self.holds_slot = false;
        if self.lease_held {
            actions.push(Action::ReleaseLease);
        }
        self.lease_held = false;
        actions
    }
}

/// Only the total-deadline abort carries a user-facing skip; a deliberate
/// abort (press, recording, cancel) completes silently with the raw text.
fn deadline_skip(reason: AbortReason) -> Vec<Action> {
    if reason == AbortReason::TotalDeadline {
        vec![Action::EmitSkip {
            reason: SkipReason::Timeout,
            detail: Some("local post-process exceeded its total time budget".to_string()),
        }]
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dictation-pipeline context used by the happy-path tests: a voice
    /// model was resident, the unload timeout is NOT Immediately, and no
    /// recording is live (the swap runs inside the Processing window).
    fn dictation_ctx() -> SwapPlanner {
        SwapPlanner::new(true, false)
    }

    /// Drive a full happy-path swap and collect every action in order.
    fn drive_happy(p: &mut SwapPlanner, transcript: &str) -> Vec<Action> {
        let mut all = Vec::new();
        let mut feed = |p: &mut SwapPlanner, s: Signal| all.extend(p.step(s, false));
        feed(p, Signal::Start);
        // The runner confirms each acquisition as it happens.
        p.mark_lease_acquired();
        p.mark_slot_acquired();
        feed(p, Signal::GateAllowed);
        feed(p, Signal::VoiceUnloaded);
        feed(p, Signal::LlmLoaded);
        feed(
            p,
            Signal::LlmGenerated {
                text: transcript.to_string(),
            },
        );
        feed(p, Signal::LlmUnloaded);
        feed(p, Signal::RestoreHandedOff);
        all
    }

    /// T1: Idle through Done with processed text; the action order is
    /// exactly the swap sequence (voice out waited, LLM in, generate, LLM
    /// out waited, restore handoff, lease release).
    #[test]
    fn happy_path_action_order_is_exact() {
        let mut p = dictation_ctx();
        let all = drive_happy(&mut p, "{\"transcription\":\"clean\"}");
        assert_eq!(
            all,
            vec![
                Action::TakeLease,
                Action::TakeSlot,
                Action::Gate,
                Action::UnloadVoice,
                Action::SpawnWorker,
                Action::LoadLlm,
                Action::Generate,
                Action::WaitExit,
                Action::RestoreHandoff,
                Action::ReleaseLease,
            ]
        );
        assert_eq!(p.state, SwapState::Done);
        assert!(p.is_done());
        // Post-terminal signals are inert no-ops.
        assert!(p.step(Signal::Start, false).is_empty());
    }

    /// T2: a gate refusal produces NO voice unload and NO LLM action, a
    /// raw outcome, and the skip action carrying the refusal payload. The
    /// voice model is still resident, so (not recording) the slot is
    /// dropped, not restored.
    #[test]
    fn gate_refusal_skips_everything() {
        let mut p = dictation_ctx();
        p.step(Signal::Start, false);
        // The runner confirms each acquisition as it happens.
        p.mark_lease_acquired();
        p.mark_slot_acquired();
        let actions = p.step(
            Signal::GateRefused {
                detail: "Not enough free memory for Qwen3 0.6B: needs ~915 MB, ~700 MB free"
                    .to_string(),
            },
            false,
        );
        assert_eq!(
            actions,
            vec![
                Action::EmitSkip {
                    reason: SkipReason::MemoryGate,
                    detail: Some(
                        "Not enough free memory for Qwen3 0.6B: needs ~915 MB, ~700 MB free"
                            .to_string()
                    ),
                },
                Action::DropSlotGuard,
                Action::ReleaseLease,
            ]
        );
        assert_eq!(p.state, SwapState::Done);
        assert!(
            !actions.contains(&Action::UnloadVoice),
            "a refusal must not disturb the voice model"
        );
        assert!(
            !actions.contains(&Action::SpawnWorker) && !actions.contains(&Action::LoadLlm),
            "a refusal must not load the LLM"
        );
    }

    /// T3: a voice-unload timeout ends Done(raw) with ZERO LLM actions; the
    /// LLM must not load when voice RAM is not provably freed. No recording
    /// in flight, so the handoff is a plain slot drop (T33 covers the
    /// recording row).
    #[test]
    fn voice_unload_timeout_never_loads_llm() {
        let mut p = dictation_ctx();
        p.step(Signal::Start, false);
        // The runner confirms each acquisition as it happens.
        p.mark_lease_acquired();
        p.mark_slot_acquired();
        p.step(Signal::GateAllowed, false);
        let actions = p.step(Signal::VoiceUnloadTimeout, false);
        assert_eq!(p.state, SwapState::Done);
        assert_eq!(
            actions,
            vec![
                Action::EmitSkip {
                    reason: SkipReason::EngineFailed,
                    detail: Some(
                        "voice model unload timed out; the local post-process engine did not run"
                            .to_string()
                    ),
                },
                Action::DropSlotGuard,
                Action::ReleaseLease,
            ]
        );
        assert!(!actions.contains(&Action::SpawnWorker));
        assert!(!actions.contains(&Action::LoadLlm));
        assert!(!actions.contains(&Action::Generate));
    }

    /// T4: a generation timeout yields the timeout skip, Kill + WaitExit,
    /// and (dictation context) the restore handoff, then the lease release.
    #[test]
    fn generation_timeout_cancels_and_unloads() {
        let mut p = dictation_ctx();
        p.step(Signal::Start, false);
        // The runner confirms each acquisition as it happens.
        p.mark_lease_acquired();
        p.mark_slot_acquired();
        p.step(Signal::GateAllowed, false);
        p.step(Signal::VoiceUnloaded, false);
        p.step(Signal::LlmLoaded, false);
        let teardown = p.step(Signal::Timeout(Phase::Generating), false);
        assert_eq!(
            teardown,
            vec![
                Action::EmitSkip {
                    reason: SkipReason::Timeout,
                    detail: Some("local model generation timed out".to_string()),
                },
                Action::KillWorker,
                Action::WaitExit,
            ]
        );
        assert_eq!(p.state, SwapState::UnloadingLlm);
        p.step(Signal::LlmUnloaded, false);
        let restore = p.step(Signal::RestoreHandedOff, false);
        assert_eq!(restore, vec![Action::RestoreHandoff, Action::ReleaseLease]);
        assert_eq!(p.state, SwapState::Done);
    }

    /// T5: dictation wins at EVERY boundary. Pre-LLM boundaries (gating,
    /// unloading voice) end with no LLM actions and a plain slot drop (no
    /// recording is live inside the Processing window). Post-unload
    /// boundaries (loading, mid-generating) kill the worker and hand the
    /// slot to a restore. An abort mid-teardown only accelerates it.
    #[test]
    fn abort_press_pending_wins_at_every_boundary() {
        // Gating: nothing to kill, slot dropped, lease released.
        let mut p = dictation_ctx();
        p.step(Signal::Start, false);
        // The runner confirms each acquisition as it happens.
        p.mark_lease_acquired();
        p.mark_slot_acquired();
        let actions = p.step(Signal::Abort(AbortReason::PressPending), false);
        assert_eq!(actions, vec![Action::DropSlotGuard, Action::ReleaseLease]);
        assert_eq!(p.state, SwapState::Done);

        // UnloadingVoice: the LLM never loads; same terminal tail.
        let mut p = dictation_ctx();
        p.step(Signal::Start, false);
        // The runner confirms each acquisition as it happens.
        p.mark_lease_acquired();
        p.mark_slot_acquired();
        p.step(Signal::GateAllowed, false);
        let actions = p.step(Signal::Abort(AbortReason::RecordingStarted), false);
        assert_eq!(actions, vec![Action::DropSlotGuard, Action::ReleaseLease]);
        assert!(!actions.contains(&Action::SpawnWorker));
        assert_eq!(p.state, SwapState::Done);

        // LoadingLlm: kill, teardown, restore handoff.
        let mut p = dictation_ctx();
        p.step(Signal::Start, false);
        // The runner confirms each acquisition as it happens.
        p.mark_lease_acquired();
        p.mark_slot_acquired();
        p.step(Signal::GateAllowed, false);
        p.step(Signal::VoiceUnloaded, false);
        let actions = p.step(Signal::Abort(AbortReason::PressPending), false);
        assert_eq!(actions, vec![Action::KillWorker, Action::WaitExit]);
        p.step(Signal::LlmUnloaded, false);
        assert_eq!(p.state, SwapState::RestoringVoice);
        let restore = p.step(Signal::RestoreHandedOff, false);
        assert_eq!(restore, vec![Action::RestoreHandoff, Action::ReleaseLease]);

        // Mid-Generating: identical kill-then-teardown shape.
        let mut p = dictation_ctx();
        p.step(Signal::Start, false);
        // The runner confirms each acquisition as it happens.
        p.mark_lease_acquired();
        p.mark_slot_acquired();
        p.step(Signal::GateAllowed, false);
        p.step(Signal::VoiceUnloaded, false);
        p.step(Signal::LlmLoaded, false);
        let actions = p.step(Signal::Abort(AbortReason::UserCancel), false);
        assert_eq!(actions, vec![Action::KillWorker, Action::WaitExit]);
        // A deliberate abort emits NO skip event (the user asked for this).
        assert!(!actions.iter().any(|a| matches!(a, Action::EmitSkip { .. })));

        // Mid-UnloadLlm: teardown is never abandoned, only accelerated.
        let mut p = dictation_ctx();
        p.step(Signal::Start, false);
        // The runner confirms each acquisition as it happens.
        p.mark_lease_acquired();
        p.mark_slot_acquired();
        p.step(Signal::GateAllowed, false);
        p.step(Signal::VoiceUnloaded, false);
        p.step(Signal::LlmLoaded, false);
        p.step(
            Signal::LlmGenerated {
                text: "{}".to_string(),
            },
            false,
        );
        assert_eq!(p.state, SwapState::UnloadingLlm);
        let actions = p.step(Signal::Abort(AbortReason::PressPending), false);
        // The acceleration keeps the bounded, signal-producing exit wait:
        // a bare kill would leave the runner without a signal in a state
        // where it never refills one (only RestoringVoice does).
        assert_eq!(actions, vec![Action::KillWorker, Action::WaitExit]);
        assert_eq!(p.state, SwapState::UnloadingLlm, "teardown continues");
        p.step(Signal::LlmUnloaded, false);
        assert_eq!(p.state, SwapState::RestoringVoice);
    }

    /// T5 companion: the total-deadline abort is the one abort users hear
    /// about (a timeout skip event rides along).
    #[test]
    fn total_deadline_abort_emits_timeout_skip() {
        let mut p = dictation_ctx();
        p.step(Signal::Start, false);
        // The runner confirms each acquisition as it happens.
        p.mark_lease_acquired();
        p.mark_slot_acquired();
        p.step(Signal::GateAllowed, false);
        p.step(Signal::VoiceUnloaded, false);
        let actions = p.step(Signal::Abort(AbortReason::TotalDeadline), false);
        assert_eq!(
            actions,
            vec![
                Action::EmitSkip {
                    reason: SkipReason::Timeout,
                    detail: Some("local post-process exceeded its total time budget".to_string()),
                },
                Action::KillWorker,
                Action::WaitExit,
            ]
        );
    }

    /// T6: even when the worker ignores the kill (LlmKillTimedOut), the
    /// restore still happens and the lease is still released.
    #[test]
    fn kill_time_out_still_hands_off_and_releases() {
        let mut p = dictation_ctx();
        p.step(Signal::Start, false);
        // The runner confirms each acquisition as it happens.
        p.mark_lease_acquired();
        p.mark_slot_acquired();
        p.step(Signal::GateAllowed, false);
        p.step(Signal::VoiceUnloaded, false);
        p.step(Signal::LlmLoaded, false);
        p.step(
            Signal::LlmGenerated {
                text: "{}".to_string(),
            },
            false,
        );
        p.step(Signal::LlmKillTimedOut, false);
        assert_eq!(p.state, SwapState::RestoringVoice);
        let actions = p.step(Signal::RestoreHandedOff, false);
        assert_eq!(actions, vec![Action::RestoreHandoff, Action::ReleaseLease]);
        assert_eq!(p.state, SwapState::Done);
    }

    /// T7: the restore is skipped when the unload timeout is Immediately
    /// (FinishGuard would drop the model moments later anyway), and when no
    /// voice model was loaded before the swap (the normal history-retry
    /// case under Immediately). Both with no recording: plain slot release.
    #[test]
    fn restore_skipped_when_immediately_or_voice_not_loaded() {
        for (voice_was_loaded, immediately) in [(true, true), (false, false), (false, true)] {
            let mut p = SwapPlanner::new(voice_was_loaded, immediately);
            assert!(
                !should_restore(voice_was_loaded, immediately, false),
                "ctx voice_was_loaded={} immediately={}",
                voice_was_loaded,
                immediately
            );
            p.step(Signal::Start, false);
            // The runner confirms each acquisition as it happens.
            p.mark_lease_acquired();
            p.mark_slot_acquired();
            p.step(Signal::GateAllowed, false);
            p.step(Signal::VoiceUnloaded, false);
            p.step(Signal::LlmLoaded, false);
            p.step(
                Signal::LlmGenerated {
                    text: "{}".to_string(),
                },
                false,
            );
            p.step(Signal::LlmUnloaded, false);
            let actions = p.step(Signal::RestoreSkipped, false);
            assert_eq!(
                actions,
                vec![Action::DropSlotGuard, Action::ReleaseLease],
                "ctx voice_was_loaded={} immediately={}",
                voice_was_loaded,
                immediately
            );
            assert_eq!(p.state, SwapState::Done);
        }

        // And the positive control: the dictation context restores.
        assert!(should_restore(true, false, false));
    }

    /// T8: lease or slot denial ends Done(raw) with no side effects on the
    /// voice model or the LLM, and never a handoff action.
    #[test]
    fn lease_or_slot_denied_ends_raw_with_no_side_effects() {
        let mut p = dictation_ctx();
        p.step(Signal::Start, false);
        // Lease acquisition failed: nothing was ever held.
        let actions = p.step(Signal::LeaseDenied, false);
        assert_eq!(actions, Vec::new(), "lease denial: nothing to unwind");
        assert_eq!(p.state, SwapState::Done);

        let mut p = dictation_ctx();
        p.step(Signal::Start, false);
        // Lease acquired, slot denied: the lease is the only thing to unwind.
        p.mark_lease_acquired();
        let actions = p.step(Signal::SlotDenied, false);
        assert_eq!(actions, vec![Action::ReleaseLease]);
        assert_eq!(p.state, SwapState::Done);
        assert!(
            !actions.iter().any(|a| matches!(
                a,
                Action::RestoreHandoff | Action::DropSlotGuard | Action::UnloadVoice
            )),
            "a busy slot belongs to a load already serving any recording"
        );
    }

    /// T32: the bounds table is exactly the spec's numbers, and each
    /// phase's effective budget is min(phase bound, remaining total).
    #[test]
    fn phase_bounds_table_and_effective_budget() {
        assert_eq!(VOICE_UNLOAD_TIMEOUT, Duration::from_secs(10));
        assert_eq!(LLM_LOAD_TIMEOUT, Duration::from_secs(30));
        assert_eq!(GENERATE_TIMEOUT, Duration::from_secs(30));
        assert_eq!(WORKER_GRACEFUL_EXIT_TIMEOUT, Duration::from_secs(5));
        assert_eq!(WORKER_KILL_WAIT_TIMEOUT, Duration::from_secs(10));
        assert_eq!(TOTAL_SWAP_DEADLINE, Duration::from_secs(45));
        assert_eq!(ABORT_POLL_INTERVAL, Duration::from_millis(25));
        assert_eq!(ACQUIRE_RETRY_INTERVAL, Duration::from_millis(100));
        assert_eq!(LEASE_ACQUIRE_TIMEOUT, Duration::from_secs(10));
        assert_eq!(SLOT_ACQUIRE_TIMEOUT, Duration::from_secs(5));

        // Plenty of total budget left: the phase bound stands.
        assert_eq!(
            effective_budget(LLM_LOAD_TIMEOUT, TOTAL_SWAP_DEADLINE),
            LLM_LOAD_TIMEOUT
        );
        // Almost out of total budget: the remainder wins.
        assert_eq!(
            effective_budget(LLM_LOAD_TIMEOUT, Duration::from_secs(2)),
            Duration::from_secs(2)
        );
        // Exact tie keeps the shared value.
        assert_eq!(
            effective_budget(GENERATE_TIMEOUT, GENERATE_TIMEOUT),
            GENERATE_TIMEOUT
        );
        // Zero remaining total is a zero budget (the total-deadline abort
        // fires on the same tick).
        assert_eq!(
            effective_budget(GENERATE_TIMEOUT, Duration::ZERO),
            Duration::ZERO
        );
    }

    /// T33: the terminal handoff rule truth table, including the
    /// never-took-slot rows. Every slot-holding terminal path with a live
    /// recording forces a restore handoff; without one, a plain guard drop;
    /// paths that never held the slot emit nothing.
    #[test]
    fn terminal_handoff_truth_table() {
        assert_eq!(
            terminal_handoff(true, true),
            Some(Action::RestoreHandoff),
            "recording in flight: hand the slot to a restore load"
        );
        assert_eq!(
            terminal_handoff(true, false),
            Some(Action::DropSlotGuard),
            "no recording: nothing waits on the slot, drop it"
        );
        assert_eq!(
            terminal_handoff(false, true),
            None,
            "never held the slot: the busy slot already serves the recording"
        );
        assert_eq!(terminal_handoff(false, false), None);
    }

    /// T33 companion: the recording row of the truth table applied at the
    /// paths the rule covers (GateRefused, VoiceUnloadTimeout, and both
    /// RestoreSkipped branches). A live recording overrides the skip.
    #[test]
    fn terminal_paths_with_live_recording_force_handoff() {
        // GateRefused while a history-retry race left a recording live.
        let mut p = dictation_ctx();
        p.step(Signal::Start, false);
        // The runner confirms each acquisition as it happens.
        p.mark_lease_acquired();
        p.mark_slot_acquired();
        let actions = p.step(
            Signal::GateRefused {
                detail: "no memory".to_string(),
            },
            true,
        );
        assert_eq!(
            actions[1],
            Action::RestoreHandoff,
            "gate refusal with a live recording still restores"
        );

        // VoiceUnloadTimeout with a live recording.
        let mut p = dictation_ctx();
        p.step(Signal::Start, false);
        // The runner confirms each acquisition as it happens.
        p.mark_lease_acquired();
        p.mark_slot_acquired();
        p.step(Signal::GateAllowed, false);
        let actions = p.step(Signal::VoiceUnloadTimeout, true);
        assert_eq!(actions[1], Action::RestoreHandoff);

        // RestoreSkipped branch (Immediately) overridden by a recording:
        // the runner consults should_restore with the live reading and
        // feeds RestoreHandedOff instead.
        assert!(should_restore(true, true, true));
        assert!(should_restore(false, true, true));
        let mut p = SwapPlanner::new(false, true);
        p.step(Signal::Start, false);
        // The runner confirms each acquisition as it happens.
        p.mark_lease_acquired();
        p.mark_slot_acquired();
        p.step(Signal::GateAllowed, false);
        p.step(Signal::VoiceUnloaded, false);
        p.step(Signal::LlmLoaded, false);
        p.step(
            Signal::LlmGenerated {
                text: "{}".to_string(),
            },
            false,
        );
        p.step(Signal::LlmKillTimedOut, false);
        let actions = p.step(Signal::RestoreHandedOff, true);
        assert_eq!(actions, vec![Action::RestoreHandoff, Action::ReleaseLease]);
    }

    /// Signals for a phase we are not in, and signals after Done, are
    /// defensive no-ops: the machine never rewinds and never invents
    /// resource state.
    #[test]
    fn out_of_phase_signals_are_no_ops() {
        let mut p = dictation_ctx();
        p.step(Signal::Start, false);
        // The runner confirms each acquisition as it happens.
        p.mark_lease_acquired();
        p.mark_slot_acquired();
        assert_eq!(p.state, SwapState::Gating);
        // A load result while still gating does nothing.
        assert!(p.step(Signal::LlmLoaded, false).is_empty());
        assert_eq!(p.state, SwapState::Gating);
        // A generate timeout while gating does nothing.
        assert!(p.step(Signal::Timeout(Phase::Generating), false).is_empty());
        assert_eq!(p.state, SwapState::Gating);
        // Restore signals out of phase do nothing.
        assert!(p.step(Signal::RestoreSkipped, false).is_empty());
        // ...and the machine still completes normally afterwards.
        p.step(Signal::GateAllowed, false);
        p.step(Signal::VoiceUnloaded, false);
        p.step(Signal::LlmLoaded, false);
        p.step(
            Signal::LlmGenerated {
                text: "x".to_string(),
            },
            false,
        );
        p.step(Signal::LlmUnloaded, false);
        p.step(Signal::RestoreHandedOff, false);
        assert_eq!(p.state, SwapState::Done);
    }

    /// A voice-model load that completes during the runner's lease/slot
    /// acquisition waits changes what the swap must restore: the planner's
    /// refresh hook updates the captured context so should_restore()
    /// decides over the model the swap actually unloads, not the one
    /// resident at runner start. Stale in both directions: a freshly
    /// loaded model must be restored, a timed-out one must not.
    #[test]
    fn refreshed_context_drives_the_restore_decision() {
        // Started with nothing resident; a load completed before the
        // swap's unload: the refreshed planner restores it.
        let mut p = SwapPlanner::new(false, false);
        assert!(!p.restore_decision(false), "nothing was resident at start");
        p.refresh_restore_context(true, false);
        assert!(
            p.restore_decision(false),
            "a model loaded during the acquire waits must be restored"
        );

        // Started with a resident model; it timed out during the waits:
        // the refreshed planner does not re-load it off the stale true.
        let mut q = SwapPlanner::new(true, false);
        assert!(q.restore_decision(false));
        q.refresh_restore_context(false, false);
        assert!(
            !q.restore_decision(false),
            "a model unloaded by its own timeout must not be restored off stale context"
        );

        // The unload-timeout flip lands the same way.
        let mut r = SwapPlanner::new(true, false);
        r.refresh_restore_context(true, true);
        assert!(
            !r.restore_decision(false),
            "Immediately still suppresses the restore"
        );
        // A live recording still forces the restore regardless.
        assert!(r.restore_decision(true));
    }

    /// The (UnloadingLlm, Abort) arm must never return an action list
    /// without a signal-producing tail: the runner only refills a missing
    /// signal at RestoringVoice, so a bare KillWorker would spin forever
    /// holding the lease and slot in release builds. The arm is currently
    /// unreachable (the WaitExit loop consumes aborts internally); this
    /// pins the safety contract for any future producer.
    #[test]
    fn unloading_llm_abort_arm_always_terminates() {
        let mut p = dictation_ctx();
        p.step(Signal::Start, false);
        p.mark_lease_acquired();
        p.mark_slot_acquired();
        p.step(Signal::GateAllowed, false);
        p.step(Signal::VoiceUnloaded, false);
        p.step(Signal::LlmLoaded, false);
        p.step(
            Signal::LlmGenerated {
                text: "x".to_string(),
            },
            false,
        );
        assert_eq!(p.state, SwapState::UnloadingLlm);

        let actions = p.step(Signal::Abort(AbortReason::UserCancel), false);
        assert_eq!(p.state, SwapState::UnloadingLlm);
        assert_eq!(
            actions,
            vec![Action::KillWorker, Action::WaitExit],
            "the abort during teardown must accelerate it AND end in the bounded, \
             signal-producing exit wait"
        );

        // And the machine still completes normally afterwards.
        p.step(Signal::LlmUnloaded, false);
        assert_eq!(p.state, SwapState::RestoringVoice);
        let tail = p.step(Signal::RestoreHandedOff, false);
        assert_eq!(tail, vec![Action::RestoreHandoff, Action::ReleaseLease]);
        assert!(p.is_done());
    }
}
