use crate::actions::ACTION_MAP;
use crate::managers::audio::AudioRecordingManager;
use crate::settings::ShortcutActivation;
use log::{debug, error, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};

const DEBOUNCE: Duration = Duration::from_millis(30);
const RELEASE_GRACE: Duration = Duration::from_millis(50);

/// KB-221: how long a completed press..release hold stays resolvable after
/// its release. The discovering text snapshot renders within one cadence of
/// the material (worst case the menu-fallback operating point: ~1.5-2.1 s
/// interim cadence plus the commit lag), so 5 s is generous headroom while
/// still bounding the silent-gap corner (a pair surviving long after the
/// user stopped speaking must not resolve against much later dictation).
const MISSED_HOLD_FRESHNESS: Duration = Duration::from_secs(5);

/// KB-221: whether a missed hold's release edge is recent enough that the
/// discovering snapshot's fresh material can still be bounded by it.
fn missed_hold_pair_is_fresh(released_at: Instant, now: Instant) -> bool {
    now.duration_since(released_at) <= MISSED_HOLD_FRESHNESS
}

/// The binding id of the phone/tablet companion trigger. Every companion
/// edge (live presses from the phone, synthesized finalize edges from the
/// server) routes the lifecycle through this one id.
const COMPANION_BINDING_ID: &str = "transcribe_companion";

/// The binding id of the post-process dictation hotkey (v1.6.0 W1-L7). A
/// press with no live session starts a post-processed recording as always;
/// a press while ANOTHER binding's session is live upgrades that session
/// to post-processed (a further press reverts it) instead of competing
/// with it - never a second recording, never the busy notice.
const POST_PROCESS_BINDING_ID: &str = "transcribe_with_post_process";

// Operator rule, stated absolutely: the ONLY always-on binding is the
// transcribe trigger. Every other binding (delete, undo, command modifier)
// may act while a dictation session is LIVE and never after it ends.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PttAction {
    Passthrough,
    DeferRelease,
    CancelRelease,
}

/// A key-up deferred by `RELEASE_GRACE` so a synthesized X11 auto-repeat
/// press can cancel it (#1539). When the grace elapses the hold is resolved
/// by [`CoordinatorState::finish_hold`] (recording) or
/// [`CoordinatorState::finish_pending_hold`] (press remembered while busy).
struct PendingRelease {
    binding_id: String,
    hotkey_string: String,
    deadline: Instant,
    /// When the key actually went up. The hold duration is measured to this
    /// instant, not to the grace expiry.
    released_at: Instant,
    /// Holds at least this long stop recording; shorter ones lock it on.
    /// Push-to-talk passes zero so every release stops.
    hold_threshold: Duration,
}

/// A press that arrived while the pipeline was still busy processing the
/// previous transcription. Toggle-style triggers (SIGUSR2, CLI flags, some
/// pedal setups) flip state on every edge, so dropping a busy press desyncs
/// the parity: the next edge starts a recording nobody will ever stop.
struct PendingPress {
    binding_id: String,
    hotkey_string: String,
    /// The companion prefs the remembered press carried, so the session
    /// the drain starts runs with the prefs of the press that asked for
    /// it, not whatever a later press left in the shared slot.
    companion_prefs: Option<crate::companion::protocol::SessionPrefs>,
    /// The real key-down time, so a hold that straddles the drain is still
    /// measured from when the user pressed, not from when recording began.
    pressed_at: Instant,
    /// The recording will start locked on when the pipeline drains: set from
    /// the start for toggle, and for hold-or-toggle once the key came back up
    /// within the threshold (a tap). An unlocked pending press is a key we
    /// believe is still held.
    locked: bool,
}

impl PendingPress {
    fn remembered(&self) -> Remembered {
        if self.locked {
            Remembered::Locked
        } else {
            Remembered::Held
        }
    }
}

/// What kind of press is already waiting for the pipeline to drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Remembered {
    /// The key is still down as far as we know.
    Held,
    /// A toggle press or a classified tap: it will start a locked session.
    Locked,
}

/// Bookkeeping for the key press that started the current recording.
struct Hold {
    pressed_at: Instant,
    /// Recording outlives the key: the next press stops it, releases are
    /// ignored. Always set for toggle; set for hold-or-toggle once a release
    /// has been classified as a tap.
    locked: bool,
}

/// What to do with an input that arrives while the pipeline is busy
/// (`Stage::Processing`). `remembered` is the press for the same binding
/// already waiting for the pipeline to drain, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BusyAction {
    /// Ignore the input entirely.
    Ignore,
    /// Remember the press; start recording when the pipeline finishes.
    Remember,
    /// This press cancels a previously remembered press: two presses during
    /// one busy window net to no-op, exactly as a press stops a locked
    /// session once recording.
    Forget,
}

fn classify_busy_input(
    is_pressed: bool,
    mode: ShortcutActivation,
    remembered: Option<Remembered>,
) -> BusyAction {
    use ShortcutActivation::*;
    match (mode, is_pressed, remembered) {
        // Toggle: presses alternate remember/forget to preserve parity.
        (Toggle, true, Some(_)) => BusyAction::Forget,
        (Toggle, true, None) => BusyAction::Remember,
        // Toggle mode ignores releases.
        (Toggle, false, _) => BusyAction::Ignore,
        // Hold modes: a press while busy means the user is holding the key -
        // start as soon as the pipeline drains. A press on a queued tap stops
        // it (parity); a press while the key is already down is a repeat.
        (PushToTalk | HoldOrToggle, true, None) => BusyAction::Remember,
        (PushToTalk | HoldOrToggle, true, Some(Remembered::Locked)) => BusyAction::Forget,
        (PushToTalk | HoldOrToggle, true, Some(Remembered::Held)) => BusyAction::Ignore,
        // Releases of a held pending press are deferred by the grace window
        // before reaching here and resolved by `finish_pending_hold`; any
        // other release (no press remembered, or already locked) is noise.
        (PushToTalk | HoldOrToggle, false, _) => BusyAction::Ignore,
    }
}

/// Pipeline lifecycle.
#[derive(Debug, PartialEq, Eq)]
enum Stage {
    Idle,
    Recording(String), // binding_id
    Processing,
}

/// A keyboard/signal edge for a transcribe binding.
struct InputEvent {
    binding_id: String,
    hotkey_string: String,
    is_pressed: bool,
    mode: ShortcutActivation,
    /// Hold-or-toggle: minimum press duration that counts as a hold.
    hold_threshold: Duration,
    /// External triggers (SIGUSR2, CLI flags) rather than physical keys.
    /// They fire on every edge by design and must never be debounced -
    /// dropping one desyncs toggle parity and wedges recording on.
    external: bool,
    /// v1.6.0: the companion session preferences folded on the connection
    /// that sent this edge. Only companion edges carry them; the effect
    /// they arm applies them to the live slot at the same serialized
    /// instant the start is committed, so a concurrent press from another
    /// phone can never overwrite them in the arm-to-freeze window.
    companion_prefs: Option<crate::companion::protocol::SessionPrefs>,
}

impl InputEvent {
    /// The hold duration at or above which a release stops recording.
    fn effective_hold_threshold(&self) -> Duration {
        match self.mode {
            ShortcutActivation::HoldOrToggle => self.hold_threshold,
            // Every release stops; toggle never defers releases at all.
            ShortcutActivation::PushToTalk | ShortcutActivation::Toggle => Duration::ZERO,
        }
    }
}

/// A side effect decided by [`CoordinatorState`]; the coordinator thread is
/// the only executor. Keeping decisions pure lets tests drive the exact
/// production transitions without a Tauri `AppHandle` or real timers.
#[derive(Debug, PartialEq, Eq)]
enum Effect {
    Start {
        binding_id: String,
        hotkey_string: String,
        /// v1.6.0: the companion prefs the starting press edge carried;
        /// the executor applies them as the session's own before the
        /// start preamble runs. None for keyboard/signal starts.
        companion_prefs: Option<crate::companion::protocol::SessionPrefs>,
    },
    Stop {
        binding_id: String,
        hotkey_string: String,
    },
    /// The command-mode binding was pressed with no live dictation session.
    /// Surfaced as a toast so the press is not a silent no-op (the binding
    /// deliberately never starts a recording; without feedback a press
    /// between sessions reads as "commands stopped working").
    NotifyCommandIdle,
    /// A transcribe binding was pressed while a DIFFERENT binding is already
    /// recording; the press was swallowed to protect the live session.
    /// Surfaced through the notice channel so the press is not silent.
    NotifyRecordingBusy { binding_id: String },
    /// The command-mode modifier engaged (true) or disengaged (false) for
    /// the live session; drives the overlay's command-mode badge. Emitted on
    /// every transition path, session end included, so the badge can never
    /// outlive the session that armed it. KB-227: the engage edge is
    /// published only while a live streaming run exists (the gate lives in
    /// [`run_effect`]) - the only configuration in which anything interprets
    /// commands - while every false edge publishes unconditionally.
    CommandModifierChanged { active: bool },
    /// The post-process hotkey was pressed while another binding's session
    /// is live (v1.6.0 W1-L7): the executor resolves the session's current
    /// mode and flips it ([`CoordinatorState::set_session_post_process`]),
    /// confirming through the notice channel and the badge event. The
    /// recording lifecycle is untouched - no second recording.
    ToggleSessionPostProcess,
    /// The live session's post-process badge lit (true) or cleared (false).
    /// Drives the overlay's PP chip the same way
    /// [`Effect::CommandModifierChanged`] drives the CMD chip: emitted on
    /// every real transition, session end and cancel included, so the badge
    /// can never outlive the session that armed it.
    PostProcessModeChanged { active: bool },
}

/// Commands processed sequentially by the coordinator thread.
enum Command {
    Input(InputEvent),
    Cancel {
        recording_was_active: bool,
    },
    ProcessingFinished,
    /// Press/release of the command-mode binding (a during-dictation
    /// modifier, never a recording trigger).
    CommandModifier {
        is_pressed: bool,
    },
    /// The companion trigger source is over (phone disconnected, the
    /// 15-minute session cap fired, the companion server is stopping):
    /// end the companion's session now, regardless of activation-mode
    /// locks. Unlike a key release, this edge is deliberate and terminal.
    FinalizeCompanion,
}

/// Decide whether a key-up should be deferred (so auto-repeat can cancel it)
/// or a key-down cancels a deferred release. `hold_to_talk` is whether a
/// release currently ends the session: true for push-to-talk and for an
/// unlocked hold-or-toggle session, false for toggle and a locked session.
/// `held_binding` is the binding whose key we believe is down - the one
/// recording, or the one remembered while the pipeline is busy.
fn classify_ptt_event(
    pending_release_binding: Option<&str>,
    is_pressed: bool,
    hold_to_talk: bool,
    binding_id: &str,
    held_binding: Option<&str>,
) -> PttAction {
    if !hold_to_talk {
        return PttAction::Passthrough;
    }

    if is_pressed {
        if pending_release_binding == Some(binding_id) {
            PttAction::CancelRelease
        } else {
            PttAction::Passthrough
        }
    } else if held_binding == Some(binding_id) && pending_release_binding.is_none() {
        PttAction::DeferRelease
    } else {
        PttAction::Passthrough
    }
}

/// Pure lifecycle state machine: owns every transition decision (release
/// grace, hold-vs-tap classification, debounce, busy-pipeline
/// remember/forget, cancel, drain). Produces [`Effect`]s instead of touching
/// the app, so unit tests exercise the real production logic.
///
/// All three activation modes run through one machine. A recording starts on
/// key-down in every mode; what differs is how it ends:
///
/// * push-to-talk - every release stops (hold threshold of zero)
/// * toggle - releases are ignored, the next press stops (locked from the start)
/// * hold-or-toggle - a release after a long hold stops; a release after a
///   short tap locks the session, and the next press stops
struct CoordinatorState {
    stage: Stage,
    hold: Option<Hold>,
    last_press: Option<Instant>,
    pending_release: Option<PendingRelease>,
    pending_press: Option<PendingPress>,
    /// True while the command-mode binding is held AND a dictation session
    /// it can modulate is live. Set on a press that arrives during a live
    /// recording; cleared on release and on every path that ends the
    /// session (finalize, cancel, failed-start rollback). A press with no
    /// live session is a complete no-op, so a session that starts later
    /// does not inherit a modifier that was never activated for it.
    command_modifier: bool,
    /// Pending command-modifier transitions for the overlay badge, drained
    /// by the coordinator thread after the command that caused them. Every
    /// real transition of `command_modifier` (engage, release, and the
    /// session-end clears) enqueues exactly one notification, so the badge
    /// tracks the modifier without the thread polling the mirror.
    modifier_notifications: Vec<bool>,
    /// KB-221: wall-clock instant the command modifier was pressed while a
    /// dictation session was live (the engage edge). Paired with the
    /// release edge into [`Self::command_hold_edges`]; cleared on every
    /// path that ends the session so a press can never complete across
    /// sessions.
    command_press_at: Option<Instant>,
    /// KB-221: a completed press..release hold that no text-bearing
    /// snapshot has observed yet, waiting for the coordinator thread to
    /// publish it to the missed-hold mirror (whence the snapshot emitter or
    /// the finalize fold drains it). The brisk sub-snapshot hold used to
    /// vanish silently: `held_now` was never sampled true, so the buffer's
    /// engagement/latch machinery never ran and the spoken command word
    /// pasted as literal dictation.
    command_hold_edges: Option<(Instant, Instant)>,
    /// v1.6.0 W1-L7: the live (or just-stopped, pre-drain) session's
    /// post-process mode as an EXPLICIT in-session decision. Every session
    /// that began recording gets its mode resolved ONCE at start (the pp
    /// hotkey's own start always true; a companion session while its frozen
    /// preference asks AND the master toggle is on at that instant; a
    /// keyboard normal session false), so the stop pipeline never re-reads
    /// the master toggle at a second instant - the badge and the shipped
    /// behavior cannot diverge when the operator flips the global switch
    /// mid-session. Each in-session pp-hotkey press flips the resolved
    /// mode. None means no session this coordinator arm resolved (test
    /// contexts, pre-start states); the legacy stop-time resolution is the
    /// fallback for that case only. Read by the stop pipeline through the
    /// mirror: it deliberately survives
    /// [`CoordinatorState::begin_processing`] so the pipeline honors the
    /// FINAL mode, and is cleared on cancel, failed-start rollback, and
    /// every new [`CoordinatorState::begin_recording`].
    session_post_process: Option<bool>,
    /// Whether the overlay's PP badge is currently lit. Cleared on every
    /// path that ends the session (finalize, cancel, failed-start
    /// rollback), so the badge can never go stale; the override above
    /// deliberately survives those clears for the stop pipeline.
    pp_badge: bool,
    /// Pending PP badge transitions for the overlay, drained by the
    /// coordinator thread after the command that caused them (the same
    /// discipline as `modifier_notifications`).
    post_process_notifications: Vec<bool>,
}

impl CoordinatorState {
    fn new() -> Self {
        Self {
            stage: Stage::Idle,
            hold: None,
            last_press: None,
            pending_release: None,
            pending_press: None,
            command_modifier: false,
            modifier_notifications: Vec::new(),
            command_press_at: None,
            command_hold_edges: None,
            session_post_process: None,
            pp_badge: false,
            post_process_notifications: Vec::new(),
        }
    }

    /// The single writer of `command_modifier`: flips the flag and enqueues
    /// a badge notification whenever the value actually changes. No-op
    /// assignments (clearing an already-clear flag) stay silent.
    fn set_command_modifier(&mut self, active: bool) {
        if self.command_modifier != active {
            self.command_modifier = active;
            self.modifier_notifications.push(active);
        }
    }

    /// Badge notifications waiting for the thread, oldest first.
    fn drain_modifier_notifications(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.modifier_notifications)
            .into_iter()
            .map(|active| Effect::CommandModifierChanged { active })
            .collect()
    }

    /// The single writer of the session's post-process mode (v1.6.0 W1-L7):
    /// records the override and lights or clears the badge with it,
    /// enqueueing exactly one badge notification per real on/off
    /// transition. No-op assignments (setting an already-held value) stay
    /// silent, so the badge never flickers on a redundant write.
    fn set_session_post_process(&mut self, mode: Option<bool>) {
        self.session_post_process = mode;
        let badge = mode == Some(true);
        if self.pp_badge != badge {
            self.pp_badge = badge;
            self.post_process_notifications.push(badge);
        }
    }

    /// The session is ending: the badge must not outlive it. The override
    /// deliberately survives - the stop pipeline reads the session's FINAL
    /// mode through the mirror - so this clears only the badge half.
    fn clear_pp_badge(&mut self) {
        if self.pp_badge {
            self.pp_badge = false;
            self.post_process_notifications.push(false);
        }
    }

    /// PP badge notifications waiting for the thread, oldest first.
    fn drain_post_process_notifications(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.post_process_notifications)
            .into_iter()
            .map(|active| Effect::PostProcessModeChanged { active })
            .collect()
    }

    /// Deadline of the deferred release, if any - drives `recv_timeout`.
    fn grace_deadline(&self) -> Option<Instant> {
        self.pending_release.as_ref().map(|p| p.deadline)
    }

    /// Whether the current session (recording, or remembered for the drain)
    /// outlives the key, so releases are ignored and the next press ends it.
    fn is_locked(&self) -> bool {
        self.hold.as_ref().is_some_and(|h| h.locked)
            || self.pending_press.as_ref().is_some_and(|p| p.locked)
    }

    fn on_input(&mut self, input: InputEvent, now: Instant) -> Option<Effect> {
        let pending_release_binding = self
            .pending_release
            .as_ref()
            .map(|pending| pending.binding_id.as_str());
        let held_binding = match &self.stage {
            Stage::Recording(id) => Some(id.as_str()),
            Stage::Processing => self.pending_press.as_ref().map(|p| p.binding_id.as_str()),
            Stage::Idle => None,
        };
        let hold_to_talk = input.mode != ShortcutActivation::Toggle && !self.is_locked();

        match classify_ptt_event(
            pending_release_binding,
            input.is_pressed,
            hold_to_talk,
            &input.binding_id,
            held_binding,
        ) {
            PttAction::CancelRelease => {
                self.pending_release = None;
                return None;
            }
            PttAction::DeferRelease => {
                self.pending_release = Some(PendingRelease {
                    hold_threshold: input.effective_hold_threshold(),
                    binding_id: input.binding_id,
                    hotkey_string: input.hotkey_string,
                    deadline: now + RELEASE_GRACE,
                    released_at: now,
                });
                return None;
            }
            PttAction::Passthrough => {}
        }

        // Debounce rapid-fire press events (key repeat / double-tap).
        // Releases in the hold modes are deferred above to absorb X11 auto-repeat.
        // External triggers are exempt: each one is a deliberate edge from the
        // user's own integration, and dropping it desyncs toggle parity.
        if input.is_pressed && !input.external {
            if self
                .last_press
                .is_some_and(|t| now.duration_since(t) < DEBOUNCE)
            {
                debug!("Debounced press for '{}'", input.binding_id);
                return None;
            }
            self.last_press = Some(now);
        }

        // A busy pipeline can't accept lifecycle changes now: classify the
        // input against any already-remembered press instead of dropping it
        // silently.
        if let Stage::Processing = self.stage {
            // Only one press can be remembered. Once a binding has claimed it,
            // inputs for a different binding are ignored - the same rule as a
            // different binding pressed while recording - rather than silently
            // replacing the remembered press and breaking its parity.
            if let Some(pending) = &self.pending_press {
                if pending.binding_id != input.binding_id {
                    debug!(
                        "Ignoring input for '{}': '{}' is already pending",
                        input.binding_id, pending.binding_id
                    );
                    return None;
                }
            }
            let remembered = self.pending_press.as_ref().map(|p| p.remembered());
            match classify_busy_input(input.is_pressed, input.mode, remembered) {
                BusyAction::Remember => {
                    debug!(
                        "Remembering press for '{}': pipeline busy",
                        input.binding_id
                    );
                    self.pending_press = Some(PendingPress {
                        // Toggle never ends on a release: locked from the start.
                        locked: input.mode == ShortcutActivation::Toggle,
                        binding_id: input.binding_id,
                        hotkey_string: input.hotkey_string,
                        companion_prefs: input.companion_prefs,
                        pressed_at: now,
                    });
                }
                BusyAction::Forget => {
                    debug!("Forgetting remembered press for '{}'", input.binding_id);
                    self.pending_press = None;
                }
                BusyAction::Ignore => {
                    debug!("Ignoring input for '{}': pipeline busy", input.binding_id);
                }
            }
            return None;
        }

        if input.is_pressed {
            match &self.stage {
                Stage::Idle => {
                    // Toggle never ends on a release: locked from the start.
                    let locked = input.mode == ShortcutActivation::Toggle;
                    return Some(self.begin_recording(
                        input.binding_id,
                        input.hotkey_string,
                        input.companion_prefs,
                        now,
                        locked,
                    ));
                }
                Stage::Recording(id) if id == &input.binding_id => {
                    // A locked session ends on the next press. In toggle mode
                    // every press ends it, even if the recording began under a
                    // hold mode (the setting changed mid-recording) - otherwise
                    // nothing but Escape could stop it.
                    if self.is_locked() || input.mode == ShortcutActivation::Toggle {
                        return Some(self.begin_processing(input.binding_id, input.hotkey_string));
                    }
                    // The key is still held (its release will end this
                    // recording), so a repeated press means nothing.
                    debug!("Ignoring press for '{}': key is held", input.binding_id);
                }
                // v1.6.0 W1-L7: the post-process hotkey pressed while
                // ANOTHER binding's session is live never competes with it.
                // The press upgrades that session to post-processed, or (on
                // an already-post-process session) reverts it; the executor
                // resolves which. No second recording, no busy notice.
                Stage::Recording(_) if input.binding_id == POST_PROCESS_BINDING_ID => {
                    debug!("Post-process hotkey pressed during a live session; toggling its mode");
                    return Some(Effect::ToggleSessionPostProcess);
                }
                _ => {
                    debug!(
                        "Ignoring press for '{}': another binding is recording",
                        input.binding_id
                    );
                    // Swallowed, but not silently: the usage log showed
                    // cross-binding presses the operator believed were
                    // starting dictations. One info notice explains it.
                    return Some(Effect::NotifyRecordingBusy {
                        binding_id: input.binding_id.clone(),
                    });
                }
            }
        } else if hold_to_talk
            && matches!(&self.stage, Stage::Recording(id) if id == &input.binding_id)
        {
            // A release that was not deferred (one is already pending for this
            // binding): resolve it immediately rather than dropping it.
            let threshold = input.effective_hold_threshold();
            return self.finish_hold(input.binding_id, input.hotkey_string, now, threshold);
        }
        None
    }

    /// The `RELEASE_GRACE` window elapsed with no cancelling press arriving:
    /// resolve the deferred release against whatever that binding's key was
    /// holding - the live recording, or a press remembered while busy.
    fn on_grace_expired(&mut self) -> Option<Effect> {
        let pending = self.pending_release.take()?;
        match &self.stage {
            Stage::Recording(id) if *id == pending.binding_id => self.finish_hold(
                pending.binding_id,
                pending.hotkey_string,
                pending.released_at,
                pending.hold_threshold,
            ),
            Stage::Processing => {
                self.finish_pending_hold(&pending);
                None
            }
            _ => None,
        }
    }

    /// A press remembered while the pipeline was busy has been released for
    /// real, still before the drain. A completed hold has nothing left to
    /// start; a tap queues a locked session so the drain starts it - the
    /// same hold-vs-tap rule as [`CoordinatorState::finish_hold`].
    fn finish_pending_hold(&mut self, release: &PendingRelease) {
        let Some(pending) = self
            .pending_press
            .as_mut()
            .filter(|p| p.binding_id == release.binding_id)
        else {
            return;
        };
        let held = release
            .released_at
            .saturating_duration_since(pending.pressed_at);
        if held < release.hold_threshold {
            debug!(
                "Tap ({held:?}) for '{}' while busy: will start locked on when the pipeline drains",
                release.binding_id
            );
            pending.locked = true;
        } else {
            debug!(
                "Forgetting remembered press for '{}': released after a {held:?} hold while busy",
                release.binding_id
            );
            self.pending_press = None;
        }
    }

    /// The key that started the current recording has been released for real.
    /// A hold at least `threshold` long stops recording; anything shorter was a
    /// tap, which locks the session on until the next press.
    fn finish_hold(
        &mut self,
        binding_id: String,
        hotkey_string: String,
        released_at: Instant,
        threshold: Duration,
    ) -> Option<Effect> {
        let held = self
            .hold
            .as_ref()
            .map(|h| released_at.saturating_duration_since(h.pressed_at))
            // No hold bookkeeping means we cannot tell a tap from a hold;
            // stopping is the safe reading (it is what push-to-talk always did).
            .unwrap_or(Duration::MAX);
        if held >= threshold {
            return Some(self.begin_processing(binding_id, hotkey_string));
        }
        if let Some(hold) = &mut self.hold {
            debug!("Tap ({held:?}) for '{binding_id}': recording locked on until the next press");
            hold.locked = true;
        }
        None
    }

    fn on_cancel(&mut self, recording_was_active: bool) {
        self.pending_release = None;
        // An explicit cancel abandons any remembered start too - the user
        // asked for silence, not a deferred recording.
        self.pending_press = None;
        // Cancel ends the session: the command modifier cannot outlive it,
        // and no finalize drain follows a cancel, so a boxed missed-hold
        // pair is abandoned with it (KB-221).
        self.forget_command_press();
        self.command_hold_edges = None;
        self.set_command_modifier(false);
        // The session is gone: the PP badge and the in-session mode decision
        // go with it (a cancelled session runs no stop pipeline, so nothing
        // consumes the override either).
        self.set_session_post_process(None);
        // Don't reset during processing - wait for the pipeline to finish.
        if !matches!(self.stage, Stage::Processing)
            && (recording_was_active || matches!(self.stage, Stage::Recording(_)))
        {
            self.stage = Stage::Idle;
            self.hold = None;
        }
    }

    /// The companion trigger source is gone or done (phone disconnected,
    /// session cap, server stop): end the companion's own session NOW.
    /// A locked session ignores release edges by design - they model
    /// accidental key-ups of a physical key - but these edges are not key
    /// events; they are terminal "the audio source is over" signals. A
    /// release edge here would strand the recording with no incoming audio
    /// while the CompanionDisconnected notice claims it was finalized and
    /// pasted. Also drops a companion press remembered while busy: starting
    /// a locked session from a vanished phone strands the same way.
    fn on_finalize_companion(&mut self) -> Option<Effect> {
        if self
            .pending_press
            .as_ref()
            .is_some_and(|p| p.binding_id == COMPANION_BINDING_ID)
        {
            debug!("Forgetting remembered companion press: the source is gone");
            self.pending_press = None;
        }
        if self
            .pending_release
            .as_ref()
            .is_some_and(|p| p.binding_id == COMPANION_BINDING_ID)
        {
            self.pending_release = None;
        }
        match &self.stage {
            Stage::Recording(id) if id == COMPANION_BINDING_ID => Some(
                self.begin_processing(COMPANION_BINDING_ID.to_string(), "companion".to_string()),
            ),
            _ => None,
        }
    }

    fn on_processing_finished(&mut self) -> Option<Effect> {
        self.stage = Stage::Idle;
        self.hold = None;
        let Some(pending) = self.pending_press.take() else {
            // The pipeline drained with no successor: the finished session's
            // post-process override has nothing left to serve (its stop
            // pipeline already consumed it) and must not leak toward a
            // future session.
            self.set_session_post_process(None);
            return None;
        };
        debug!(
            "Pipeline drained; starting remembered press for '{}'",
            pending.binding_id
        );
        Some(self.begin_recording(
            pending.binding_id,
            pending.hotkey_string,
            pending.companion_prefs,
            pending.pressed_at,
            pending.locked,
        ))
    }

    /// Reconcile the optimistic `Stage::Recording` after the executor reports
    /// whether recording actually began (microphone access can be denied).
    fn on_start_result(&mut self, binding_id: &str, started: bool) {
        if !started && matches!(&self.stage, Stage::Recording(id) if id == binding_id) {
            self.stage = Stage::Idle;
            self.hold = None;
            // The session never existed; the modifier cannot stay armed
            // (KB-221: nor can any press/hold bookkeeping of it), and the
            // PP badge a pp start would have lit must go dark with the
            // rollback (the override too: no pipeline will read it).
            self.forget_command_press();
            self.command_hold_edges = None;
            self.set_command_modifier(false);
            self.set_session_post_process(None);
        }
    }

    /// Optimistic transition to `Recording`; rolled back via
    /// [`CoordinatorState::on_start_result`] if the effect fails to start
    /// recording for real.
    fn begin_recording(
        &mut self,
        binding_id: String,
        hotkey_string: String,
        companion_prefs: Option<crate::companion::protocol::SessionPrefs>,
        pressed_at: Instant,
        locked: bool,
    ) -> Effect {
        self.stage = Stage::Recording(binding_id.clone());
        self.hold = Some(Hold { pressed_at, locked });
        // A fresh session starts with no in-session post-process decision;
        // the executor's start arm immediately resolves the session's mode
        // (freeze-at-start: the stop pipeline reads this same value).
        self.set_session_post_process(None);
        Effect::Start {
            binding_id,
            hotkey_string,
            companion_prefs,
        }
    }

    fn begin_processing(&mut self, binding_id: String, hotkey_string: String) -> Effect {
        self.stage = Stage::Processing;
        self.hold = None;
        // The recording ended: the PP badge goes dark with it (an explicit
        // clear, not a mode write - the override below must survive for the
        // stop pipeline, which reads the session's FINAL mode).
        self.clear_pp_badge();
        // The recording ended: the in-session command modifier goes with it,
        // even if the key is still physically held. Re-engaging requires a
        // fresh press during the next live session. This is the session-end
        // badge clear: the notification rides the drain below. (KB-221: an
        // in-progress press cannot complete into a hold across the session
        // boundary either, but a COMPLETED pair survives - the stop
        // pipeline's finalize fold may still drain it.)
        self.forget_command_press();
        self.set_command_modifier(false);
        Effect::Stop {
            binding_id,
            hotkey_string,
        }
    }

    /// A press or release of the command-mode binding. The binding never
    /// touches the recording lifecycle: a press engages command
    /// interpretation only when a dictation session is live at that moment,
    /// a release always disengages, and any other situation is inert. A
    /// press with no live session returns [`Effect::NotifyCommandIdle`] so
    /// the user learns why nothing happened.
    ///
    /// KB-221: every engaged press..release pair is also boxed as wall-clock
    /// edges ([`Self::command_hold_edges`]) so the streaming path can
    /// resolve holds that fall entirely between two text-bearing snapshots.
    fn on_command_modifier(&mut self, is_pressed: bool, now: Instant) -> Option<Effect> {
        if is_pressed {
            if matches!(self.stage, Stage::Recording(_)) {
                debug!("Command modifier engaged for the live dictation session");
                self.command_press_at = Some(now);
                self.set_command_modifier(true);
                None
            } else {
                debug!("Command modifier pressed with no live dictation session; nothing happens");
                Some(Effect::NotifyCommandIdle)
            }
        } else if self.command_modifier {
            debug!("Command modifier released; dictation returns to normal");
            if let Some(press) = self.command_press_at.take() {
                self.command_hold_edges = Some((press, now));
            }
            self.set_command_modifier(false);
            None
        } else {
            None
        }
    }

    /// KB-221: take the completed press..release hold waiting for
    /// publication to the missed-hold mirror, if any. The coordinator
    /// thread moves the pair out after the command that produced it.
    fn take_command_hold_edges(&mut self) -> Option<(Instant, Instant)> {
        self.command_hold_edges.take()
    }

    /// KB-221: drop any in-progress modifier-press bookkeeping. Called on
    /// every path that ends the session: a press cannot complete into a
    /// hold across sessions. A COMPLETED pair is deliberately kept - the
    /// stop pipeline's finalize fold may still drain it.
    fn forget_command_press(&mut self) {
        self.command_press_at = None;
    }
}

/// Serialises all transcription lifecycle events through a single thread
/// to eliminate race conditions between keyboard shortcuts, signals, and
/// the async transcribe-paste pipeline. The thread is a thin shell: it
/// transports commands to the pure [`CoordinatorState`] and executes the
/// returned [`Effect`]s.
pub struct TranscriptionCoordinator {
    tx: Sender<Command>,
    /// Mirror of `Stage::Recording` for lock-free readers outside the
    /// coordinator thread (the session-state seam): buffer-aware hotkey
    /// actions ask "is a dictation recording live right now?" without
    /// sending a command and waiting for an answer. Updated by the thread
    /// after every processed command, so it lags reality by at most the
    /// channel latency.
    recording: Arc<AtomicBool>,
    /// Mirror of `CoordinatorState::command_modifier` for the interim
    /// streaming path: the session buffer asks "is the command modifier
    /// held for this live session?" on every engine snapshot, lock-free.
    command_modifier: Arc<AtomicBool>,
    /// Mirror of `CoordinatorState::pending_press` for the exclusive
    /// post-process swap: the swap runner polls "did a press arrive while
    /// the pipeline is busy?" every abort tick, lock-free, so dictation
    /// wins over post-processing without waiting on the coordinator
    /// thread. The latch itself is NOT new state: it observes the same
    /// pending_press the drain consumes.
    pending_press: Arc<AtomicBool>,
    /// Mirror of `Stage::Processing` for lock-free readers outside the
    /// coordinator thread: the cancel-shortcut handler gate asks "is the
    /// stop pipeline still working (finalize, batch, post-process, paste)?"
    /// so Escape stays alive for the whole pipeline, not just while a
    /// microphone recording is live. Published at the same points as the
    /// recording mirror.
    processing: Arc<AtomicBool>,
    /// KB-221: mirror of `CoordinatorState::command_hold_edges` for the
    /// streaming path. The coordinator thread stores a completed
    /// press..release pair into this slot (event-driven, never refreshed
    /// wholesale, so an unrelated command cannot wipe an undrained pair);
    /// the snapshot emitter and the finalize fold TAKE it - once - and the
    /// session buffer then resolves the hold against the discovering
    /// material. Cleared on cancel, on a failed start rollback, at thread
    /// exit, and when the next session's stream begins (a pair that
    /// outlived its session must not resolve against the next one's words).
    missed_hold: Arc<Mutex<Option<(Instant, Instant)>>>,
    /// v1.6.0 W1-L7: mirror of `CoordinatorState::session_post_process`
    /// for the stop pipeline. TranscribeAction::stop runs INSIDE the Stop
    /// effect on the coordinator thread, before this mirror republishes,
    /// so it always reads the final mode of the session that just ended.
    session_post_process: Arc<Mutex<Option<bool>>>,
    /// v1.6.0 W1-L7: mirror of `CoordinatorState::pp_badge` for the
    /// cycle-prompt path: true while a live post-process session's badge is
    /// lit, so cycling templates refreshes the name the badge shows only
    /// while the badge is actually on screen.
    pp_badge: Arc<AtomicBool>,
}

/// Which binding IDs drive the recording lifecycle. The command-mode
/// binding ("transcribe_commands") is NOT one of them: it is a
/// during-dictation modifier that never starts or stops a recording, so
/// its events route to [`TranscriptionCoordinator::send_command_modifier`]
/// instead of the lifecycle input path. "transcribe_companion" is the
/// phone/tablet trigger: the same lifecycle, so the one-only-session rule
/// and the cross-binding busy notice arbitrate local vs companion presses
/// exactly as they do between the two keyboard bindings.
pub fn is_transcribe_binding(id: &str) -> bool {
    id == "transcribe" || id == "transcribe_with_post_process" || id == "transcribe_companion"
}

impl TranscriptionCoordinator {
    pub fn new(app: AppHandle) -> Self {
        let (tx, rx) = mpsc::channel();
        let recording = Arc::new(AtomicBool::new(false));
        let recording_mirror = Arc::clone(&recording);
        let command_modifier = Arc::new(AtomicBool::new(false));
        let command_modifier_mirror = Arc::clone(&command_modifier);
        let pending_press = Arc::new(AtomicBool::new(false));
        let pending_press_mirror = Arc::clone(&pending_press);
        let processing = Arc::new(AtomicBool::new(false));
        let processing_mirror = Arc::clone(&processing);
        let missed_hold: Arc<Mutex<Option<(Instant, Instant)>>> = Arc::new(Mutex::new(None));
        let missed_hold_mirror = Arc::clone(&missed_hold);
        let session_post_process: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));
        let session_post_process_mirror = Arc::clone(&session_post_process);
        let pp_badge = Arc::new(AtomicBool::new(false));
        let pp_badge_mirror = Arc::clone(&pp_badge);

        thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut state = CoordinatorState::new();
                // Publish the stage mirror after every state change, so a
                // reader can never observe a stale Recording long after the
                // stage moved on (worst case: one command of lag while a
                // transition is in flight). The command-modifier mirror
                // rides the same publication point.
                let publish_state = |state: &CoordinatorState| {
                    recording_mirror.store(
                        matches!(state.stage, Stage::Recording(_)),
                        Ordering::Release,
                    );
                    processing_mirror
                        .store(matches!(state.stage, Stage::Processing), Ordering::Release);
                    command_modifier_mirror.store(state.command_modifier, Ordering::Release);
                    pending_press_mirror.store(state.pending_press.is_some(), Ordering::Release);
                    *session_post_process_mirror
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = state.session_post_process;
                    pp_badge_mirror.store(state.pp_badge, Ordering::Release);
                };
                publish_state(&state);

                loop {
                    let cmd = if let Some(deadline) = state.grace_deadline() {
                        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                            Ok(cmd) => cmd,
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                if let Some(effect) = state.on_grace_expired() {
                                    run_effect(&app, &mut state, effect);
                                }
                                publish_state(&state);
                                continue;
                            }
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        }
                    } else {
                        match rx.recv() {
                            Ok(cmd) => cmd,
                            Err(_) => break,
                        }
                    };

                    match cmd {
                        Command::Input(input) => {
                            if let Some(effect) = state.on_input(input, Instant::now()) {
                                run_effect(&app, &mut state, effect);
                            }
                        }
                        Command::Cancel {
                            recording_was_active,
                        } => {
                            state.on_cancel(recording_was_active);
                            // KB-221: no finalize drain follows a cancel, so
                            // any published missed-hold pair dies with the
                            // session (the state side was already cleared by
                            // on_cancel).
                            *missed_hold_mirror.lock().unwrap_or_else(|e| e.into_inner()) = None;
                        }
                        Command::ProcessingFinished => {
                            if let Some(effect) = state.on_processing_finished() {
                                run_effect(&app, &mut state, effect);
                            }
                        }
                        Command::CommandModifier { is_pressed } => {
                            if let Some(effect) =
                                state.on_command_modifier(is_pressed, Instant::now())
                            {
                                run_effect(&app, &mut state, effect);
                            }
                        }
                        Command::FinalizeCompanion => {
                            if let Some(effect) = state.on_finalize_companion() {
                                run_effect(&app, &mut state, effect);
                            }
                        }
                    }
                    // KB-221: publish a completed hold's edges to the
                    // mirror. Store-only-when-Some: the pair is consumed by
                    // the emitter/finalize taking it, so unrelated commands
                    // must never refresh the slot wholesale (a later pair
                    // legitimately overwrites an undrained one - the newest
                    // hold bounds the newest material).
                    if let Some(edges) = state.take_command_hold_edges() {
                        *missed_hold_mirror.lock().unwrap_or_else(|e| e.into_inner()) = Some(edges);
                    }
                    // Badge notifications the processed command enqueued
                    // (modifier engage/release, PP badge transitions, and
                    // every session-end clear).
                    for effect in state.drain_modifier_notifications() {
                        run_effect(&app, &mut state, effect);
                    }
                    for effect in state.drain_post_process_notifications() {
                        run_effect(&app, &mut state, effect);
                    }
                    publish_state(&state);
                }
                // The coordinator thread is gone (app shutdown); stop
                // advertising a live recording session, an engaged command
                // modifier, a remembered press, an undrained hold, a
                // post-process session mode, or a lit PP badge.
                recording_mirror.store(false, Ordering::Release);
                processing_mirror.store(false, Ordering::Release);
                command_modifier_mirror.store(false, Ordering::Release);
                pending_press_mirror.store(false, Ordering::Release);
                *missed_hold_mirror.lock().unwrap_or_else(|e| e.into_inner()) = None;
                *session_post_process_mirror
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = None;
                pp_badge_mirror.store(false, Ordering::Release);
                debug!("Transcription coordinator exited");
            }));
            if let Err(e) = result {
                error!("Transcription coordinator panicked: {e:?}");
            }
        });

        Self {
            tx,
            recording,
            command_modifier,
            pending_press,
            processing,
            missed_hold,
            session_post_process,
            pp_badge,
        }
    }

    /// Whether a dictation recording session is live (the coordinator is in
    /// its Recording stage: between the start effect and the stop effect).
    /// The session-state seam used by buffer-aware hotkey actions to decide
    /// between editing the dictation buffer and injecting keys.
    pub fn is_recording_session(&self) -> bool {
        self.recording.load(Ordering::Acquire)
    }

    /// Whether the stop pipeline is still working (the coordinator is in
    /// its Processing stage: after the stop effect, until the pipeline's
    /// FinishGuard reports completion). The cancel-shortcut handler gate
    /// reads this so Escape stays alive while finalize, batch
    /// transcription, post-processing, or the paste still run, even though
    /// no recording is live anymore.
    pub fn is_processing(&self) -> bool {
        self.processing.load(Ordering::Acquire)
    }

    /// Whether the command-mode binding is currently modulating the live
    /// dictation session (held during a live session). The interim
    /// streaming path consults this on every engine snapshot.
    pub fn is_command_modifier_active(&self) -> bool {
        self.command_modifier.load(Ordering::Acquire)
    }

    /// KB-221: take the completed command hold (press..release pair) that
    /// no text-bearing snapshot has observed yet, if one is both present
    /// and FRESH enough to bound the discovering snapshot's material (no
    /// more than [`MISSED_HOLD_FRESHNESS`] may separate the release from
    /// `now` - the slowest text cadence is the menu-fallback operating
    /// point at ~1.5-2.1 s plus the commit lag, so a hold older than that
    /// cannot bound this snapshot's delta; the headroom also swallows the
    /// silent-gap corner where a pair survives long after the user stopped
    /// speaking). Once-only by construction: the take is the consumption.
    /// Callers must NOT call this while the modifier is held (the live
    /// engagement path governs that material; the pair would be dropped
    /// anyway).
    pub fn take_unobserved_command_hold(&self, now: Instant) -> bool {
        let mut slot = self.missed_hold.lock().unwrap_or_else(|e| e.into_inner());
        match slot.take() {
            Some((_, released_at)) => missed_hold_pair_is_fresh(released_at, now),
            None => false,
        }
    }

    /// KB-221: drop any undrained missed-hold pair. Called when the next
    /// session's stream begins: a pair that outlived its own session (its
    /// stop pipeline never drained it) must never resolve against the next
    /// session's words.
    pub fn clear_unobserved_command_hold(&self) {
        *self.missed_hold.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Whether a dictation session is live END TO END: the Recording stage
    /// OR the Processing stage (finalize, batch transcription,
    /// post-processing, and the paste still running). The one predicate
    /// every destructive model action refuses on - tray "Unload model"
    /// (KB-220), the settings-side unload, and model delete (KB-117) - so
    /// no user action can destroy an in-flight dictation. Composes the two
    /// existing stage mirrors; no new state. The idle watcher's
    /// recording-only probe would miss the stop pipeline's batch
    /// transcription, which is exactly where a mid-session unload's damage
    /// lands (the batch fallback dies at transcribe_audio's residency
    /// check).
    pub fn is_session_live(&self) -> bool {
        self.is_recording_session() || self.is_processing()
    }

    /// Whether a transcribe press is remembered while the pipeline is busy
    /// (Stage::Processing) and will start a recording when it drains. The
    /// exclusive post-process swap polls this every abort tick: dictation
    /// wins, so a remembered press aborts the swap (kill LLM, restore
    /// voice, raw transcript out) instead of waiting out a generation.
    /// Reads the same pending_press the drain consumes, so the latch can
    /// never disagree with what the coordinator will actually fire.
    pub fn has_pending_press(&self) -> bool {
        self.pending_press.load(Ordering::Acquire)
    }

    /// v1.6.0 W1-L7: the session's FINAL post-process decision, as the stop
    /// pipeline must honor it. Some(true) - the session will post-process
    /// (the pp hotkey's own start, or an in-session upgrade that was never
    /// reverted); Some(false) - an in-session revert of a session that would
    /// otherwise have post-processed; None - no in-session decision, so the
    /// stop pipeline's legacy resolution (static binding value, companion
    /// preference, master toggle) applies. Read inside the Stop effect,
    /// before the mirror republishes, so it is the mode the just-ended
    /// session actually had.
    pub fn session_post_process_override(&self) -> Option<bool> {
        *self
            .session_post_process
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// v1.6.0 W1-L7: whether the live session's PP badge is lit. The
    /// cycle-prompt path refreshes the badge's template name only while a
    /// post-process session is actually recording; a dark badge stays dark
    /// (the cycle armed the NEXT session's template, nothing current).
    pub fn is_session_post_process_active(&self) -> bool {
        self.pp_badge.load(Ordering::Acquire)
    }

    /// Whether the Undo action may fire right now: ONLY while a recording
    /// session is live (per the operator rule above). The moment the
    /// session ends the key is inert; there is intentionally no post-paste
    /// undo.
    pub fn is_undo_active(&self) -> bool {
        self.is_recording_session()
    }

    /// Send a keyboard input event for a transcribe binding. `hold_threshold`
    /// only matters for [`ShortcutActivation::HoldOrToggle`].
    pub fn send_input(
        &self,
        binding_id: &str,
        hotkey_string: &str,
        is_pressed: bool,
        mode: ShortcutActivation,
        hold_threshold: Duration,
    ) {
        self.send(
            binding_id,
            hotkey_string,
            is_pressed,
            mode,
            hold_threshold,
            false,
            None,
        );
    }

    /// Send an external trigger (SIGUSR2, CLI flag). Always a toggle press,
    /// always exempt from debounce - see [`InputEvent::external`].
    pub fn send_external_input(&self, binding_id: &str, source: &str) {
        self.send(
            binding_id,
            source,
            true,
            ShortcutActivation::Toggle,
            Duration::ZERO,
            true,
            None,
        );
    }

    /// Forward a companion-device (phone/tablet) press or release edge for
    /// the "transcribe_companion" binding. External like the signal/CLI
    /// triggers (network edges must never be debounced - dropping one
    /// desyncs the phone's button state), but honoring the user's activation
    /// mode and hold threshold so hold-to-talk and tap-to-lock behave from
    /// the phone exactly as they do from the keyboard. The edge carries the
    /// connection's folded session prefs WITH it: the effect that commits
    /// the start applies exactly these prefs to the live slot, closing the
    /// arm-to-freeze window another phone's press could otherwise race.
    pub fn send_companion_edge(
        &self,
        app: &AppHandle,
        pressed: bool,
        prefs: crate::companion::protocol::SessionPrefs,
    ) {
        let settings = crate::settings::get_settings(app);
        self.send(
            COMPANION_BINDING_ID,
            "companion",
            pressed,
            settings.shortcut_activation,
            Duration::from_millis(settings.hold_threshold_ms),
            true,
            Some(prefs),
        );
    }

    /// Force-finalize the companion's live session: the phone dropped
    /// mid-dictation, the 15-minute cap fired, or the companion server is
    /// stopping. Unlike [`Self::send_companion_edge`] with `pressed=false`,
    /// this ends the session even when it is locked (toggle mode, or a
    /// locked hold-or-toggle session) - a locked session ignores release
    /// edges by design, so the ordinary synthesized release strands those
    /// recordings with no incoming audio while the disconnect notice claims
    /// they were finalized and pasted.
    pub fn finalize_companion_session(&self) {
        if self.tx.send(Command::FinalizeCompanion).is_err() {
            warn!("Transcription coordinator channel closed");
        }
    }

    /// Send a press/release of the command-mode binding. The binding is a
    /// during-dictation modifier: the coordinator engages command
    /// interpretation only when a session is live at the press, and the
    /// event never enters the recording lifecycle.
    pub fn send_command_modifier(&self, is_pressed: bool) {
        if self
            .tx
            .send(Command::CommandModifier { is_pressed })
            .is_err()
        {
            warn!("Transcription coordinator channel closed");
        }
    }

    #[allow(clippy::too_many_arguments)] // arity mirrors the InputEvent it wraps
    fn send(
        &self,
        binding_id: &str,
        hotkey_string: &str,
        is_pressed: bool,
        mode: ShortcutActivation,
        hold_threshold: Duration,
        external: bool,
        companion_prefs: Option<crate::companion::protocol::SessionPrefs>,
    ) {
        if self
            .tx
            .send(Command::Input(InputEvent {
                binding_id: binding_id.to_string(),
                hotkey_string: hotkey_string.to_string(),
                is_pressed,
                mode,
                hold_threshold,
                external,
                companion_prefs,
            }))
            .is_err()
        {
            warn!("Transcription coordinator channel closed");
        }
    }

    pub fn notify_cancel(&self, recording_was_active: bool) {
        if self
            .tx
            .send(Command::Cancel {
                recording_was_active,
            })
            .is_err()
        {
            warn!("Transcription coordinator channel closed");
        }
    }

    pub fn notify_processing_finished(&self) {
        if self.tx.send(Command::ProcessingFinished).is_err() {
            warn!("Transcription coordinator channel closed");
        }
    }
}

/// KB-227: whether a command-modifier badge transition may be published to
/// the overlay. The engage edge (`true`) is honest only while a live
/// streaming run exists: the streaming snapshot emitter is the modifier's
/// ONLY backend consumer, and it runs only for a stream actually in flight
/// - on ONNX/batch models (and after a failed stream start) spoken commands
/// paste as text, so an ungated chip would claim an interpretation that
/// never happens. Every disengage edge (`false` - key release, cancel,
/// failed-start rollback, session end) publishes UNCONDITIONALLY so the
/// badge can never get stuck on; the gate never blocks it.
fn command_modifier_badge_publishable(active: bool, stream_live: bool) -> bool {
    !active || stream_live
}

/// v1.6.0 W1-L7: the mode an in-session pp-hotkey press flips the session
/// to. None (no in-session decision: the session is not post-processed)
/// upgrades; an explicit mode - the pp hotkey's own session, a companion pp
/// session the start arm resolved, or a previous upgrade - reverts, and a
/// reverted session upgrades again. Pure so the toggle's truth table is
/// pinnable without an app handle.
fn toggled_session_post_process(current: Option<bool>) -> bool {
    !current.unwrap_or(false)
}

/// v1.6.0 W1-L7: whether a session that just began recording will be
/// post-processed, from the facts only the executor can see at start time:
/// the pp hotkey's own session always will; a companion session will while
/// its frozen per-session preference asks AND the master toggle is on (the
/// same off promise the pp hotkey's registration enforces). A keyboard
/// normal session will not. The resolved value is stored as the session's
/// mode (the stop pipeline's override), so the master toggle is read at
/// exactly ONE instant per session. Pure truth table, mirroring the stop
/// pipeline's legacy resolution (`actions::session_runs_post_process`) for
/// the no-override fallback case.
fn started_session_post_processes(
    pp_binding_start: bool,
    companion_session: bool,
    companion_pref: bool,
    master_enabled: bool,
) -> bool {
    pp_binding_start || (companion_session && companion_pref && master_enabled)
}

/// v1.6.0 W1-L7: the event name the overlay's PP badge listens for. The
/// frontend pins the same literal (POST_PROCESS_BADGE_EVENT in
/// src/overlay/ppBadge.ts); both sides' tests assert it, so the seam
/// cannot drift silently.
pub(crate) const POST_PROCESS_BADGE_EVENT: &str = "post-process-changed";

/// The exact JSON the overlay's `post-process-changed` listener consumes
/// (RecordingOverlay.tsx via ppBadgeStateFromPayload): `active` gates the
/// chip, `template` names it (null on the off edge and when no template is
/// selected - the chip falls back to its bare PP label, and the stop
/// pipeline surfaces the configuration gap). Pure so the payload seam is
/// pinnable on both sides.
pub(crate) fn post_process_badge_payload(
    active: bool,
    template: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({ "active": active, "template": template })
}

/// v1.6.0 W1-L7: emit one PP badge edge to the overlay. `active` lights the
/// chip naming the selected template; the off edge carries no name. Shared
/// by the coordinator's badge notifications and the selection-change
/// refresh, so every emitter resolves the name from the same settings.
pub(crate) fn emit_post_process_badge(app: &AppHandle, active: bool) {
    use tauri::Emitter;
    let template = if active {
        crate::settings::get_settings(app).selected_post_process_prompt_name()
    } else {
        None
    };
    if let Err(e) = app.emit_to(
        "recording_overlay",
        POST_PROCESS_BADGE_EVENT,
        post_process_badge_payload(active, template.as_deref()),
    ) {
        warn!("Failed to emit {POST_PROCESS_BADGE_EVENT}: {e}");
    }
}

/// v1.6.0 W1-L7 review fix: refresh a LIT PP badge after anything changed
/// what it names - the cycle hotkey, the settings dropdown, the tray prompt
/// submenu (the latter two both write through
/// set_post_process_selected_prompt), deleting the selected template (which
/// silently moves the selection to the fallback), and editing the selected
/// template (which can rename it). The badge names the ACTIVE template and
/// the stop pipeline resolves the selection at run time, so a live
/// post-process session must hear the change; a dark badge stays dark (no
/// live session is consuming templates, and the next session's start arm
/// lights it fresh).
pub(crate) fn refresh_lit_post_process_badge(app: &AppHandle) {
    if let Some(coordinator) = app.try_state::<TranscriptionCoordinator>() {
        if coordinator.is_session_post_process_active() {
            emit_post_process_badge(app, true);
        }
    }
}

fn run_effect(app: &AppHandle, state: &mut CoordinatorState, effect: Effect) {
    match effect {
        Effect::Start {
            binding_id,
            hotkey_string,
            companion_prefs,
        } => {
            // The session's prefs are applied HERE, on the coordinator's
            // single serialized thread, at the instant this start is
            // committed and BEFORE start()'s multi-millisecond preamble -
            // not read back from the shared pending slot inside
            // on_session_changed after the preamble. A concurrent press
            // from another phone is queued behind this command, so it can
            // only arm the NEXT session; the arm-to-freeze window is gone.
            if let Some(prefs) = companion_prefs {
                crate::companion::begin_session_prefs(app, prefs);
            }
            let started = start(app, &binding_id, &hotkey_string);
            state.on_start_result(&binding_id, started);
            // v1.6.0 W1-L7: a session that actually began recording gets
            // its post-process mode resolved HERE, at the single moment
            // every fact is known (the binding, the companion prefs the
            // Start effect applied just above, the master toggle), and the
            // resolved mode is
            // what the stop pipeline's override reads - the master toggle
            // is never re-read at stop, so flipping it mid-session cannot
            // make the badge lie in either direction (a lit-badge session
            // shipping after a global disable, or a dark-badge session
            // silently post-processing). Some(false) for a session that
            // will not post-process is the same no-badge state as the old
            // None, but frozen. A failed start already cleared it through
            // the rollback above.
            if started {
                state.set_session_post_process(Some(started_session_post_processes(
                    binding_id == POST_PROCESS_BINDING_ID,
                    binding_id == COMPANION_BINDING_ID,
                    crate::companion::session_prefs(app).post_process,
                    crate::settings::get_settings(app).post_process_enabled,
                )));
            }
        }
        Effect::Stop {
            binding_id,
            hotkey_string,
        } => stop(app, &binding_id, &hotkey_string),
        Effect::NotifyCommandIdle => {
            // KB-038: this used to be a bare "command-mode-no-session" emit
            // the main window alone listened for; it now rides the same
            // notice channel (log + event + card visibility) as every other
            // no-session feedback.
            crate::managers::transcription::emit_overlay_notice(
                app,
                crate::managers::transcription::NoticeCode::CommandModeNoSession,
                None,
            );
        }
        Effect::NotifyRecordingBusy { binding_id } => {
            crate::managers::transcription::emit_overlay_notice(
                app,
                crate::managers::transcription::NoticeCode::BindingBusy,
                Some(binding_id),
            );
        }
        Effect::CommandModifierChanged { active } => {
            // KB-227: gate the ENGAGE edge on the transcription manager's
            // streaming flag - the honest "this session can consume
            // commands" signal. It flips true the moment the engine stream
            // is in flight (before any snapshot arrives, so a press right
            // after stream begin still lights the badge) and is never set
            // for ONNX/batch models or after a failed start_stream. A
            // missing manager reads as "no stream": the badge stays dark
            // rather than lying. The mirror (`is_command_modifier_active`)
            // is deliberately NOT gated - a press held through the
            // stream-start window must still modulate the session once the
            // consumer appears.
            let stream_live = app
                .try_state::<Arc<crate::managers::transcription::TranscriptionManager>>()
                .is_some_and(|tm| tm.is_streaming());
            if !command_modifier_badge_publishable(active, stream_live) {
                debug!(
                    "Command modifier engaged with no live stream to interpret \
                     commands; badge suppressed (KB-227)"
                );
                return;
            }
            use tauri::Emitter;
            if let Err(e) = app.emit_to("recording_overlay", "command-modifier-changed", active) {
                warn!("Failed to emit command-modifier-changed: {e}");
            }
        }
        Effect::ToggleSessionPostProcess => {
            // Only a live recording can be toggled (on_input routes the
            // press here exclusively from the Recording stage); a stray
            // effect with no session is inert rather than fatal.
            if !matches!(state.stage, Stage::Recording(_)) {
                debug!("Post-process toggle with no live session; nothing happens");
                return;
            }
            let next = toggled_session_post_process(state.session_post_process);
            state.set_session_post_process(Some(next));
            // The badge edge rides the notification drain below; the notice
            // confirms the flip now, naming the template that will run.
            let detail = if next {
                crate::settings::get_settings(app).selected_post_process_prompt_name()
            } else {
                None
            };
            crate::managers::transcription::emit_overlay_notice(
                app,
                if next {
                    crate::managers::transcription::NoticeCode::PostProcessSessionUpgraded
                } else {
                    crate::managers::transcription::NoticeCode::PostProcessSessionDowngraded
                },
                detail,
            );
        }
        Effect::PostProcessModeChanged { active } => emit_post_process_badge(app, active),
    }
}

/// Execute a start effect; returns whether recording actually began, so the
/// state machine can roll back its optimistic transition on failure.
fn start(app: &AppHandle, binding_id: &str, hotkey_string: &str) -> bool {
    let Some(action) = ACTION_MAP.get(binding_id) else {
        warn!("No action in ACTION_MAP for '{binding_id}'");
        return false;
    };
    action.start(app, binding_id, hotkey_string);
    let recording = app
        .try_state::<Arc<AudioRecordingManager>>()
        .is_some_and(|a| a.is_recording());
    if !recording {
        debug!("Start for '{binding_id}' did not begin recording; staying idle");
    }
    recording
}

fn stop(app: &AppHandle, binding_id: &str, hotkey_string: &str) {
    let Some(action) = ACTION_MAP.get(binding_id) else {
        warn!("No action in ACTION_MAP for '{binding_id}'");
        return;
    };
    action.stop(app, binding_id, hotkey_string);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The command-mode binding is a during-dictation modifier, not a
    /// recording trigger: it must NOT route through the recording
    /// lifecycle (it can never start or stop a recording), while the
    /// dictation triggers do.
    /// T24: the pending-press latch the exclusive post-process swap polls
    /// (has_pending_press). A press that arrives while the pipeline is
    /// busy (Stage::Processing) is remembered and latches true; the drain
    /// (ProcessingFinished) consumes it and clears the latch. A cancel
    /// also clears it (the user asked for silence, not a deferred
    /// recording), and the idle state never latches. These are the pure
    /// transitions the coordinator thread mirrors.
    #[test]
    fn pending_press_latch_follows_the_remember_and_drain_transitions() {
        let mut state = CoordinatorState::new();
        let now = Instant::now();
        assert!(state.pending_press.is_none(), "idle: nothing latched");

        // Drive into Processing: start, then stop.
        assert!(matches!(
            state.on_input(toggle_input(true), now),
            Some(Effect::Start { .. })
        ));
        assert!(matches!(
            state.on_input(toggle_input(true), now + Duration::from_millis(100)),
            Some(Effect::Stop { .. })
        ));
        assert_eq!(state.stage, Stage::Processing);
        assert!(state.pending_press.is_none(), "busy but not pressed");

        // A press while busy is remembered: latch set.
        assert!(state
            .on_input(toggle_input(true), now + Duration::from_millis(200))
            .is_none());
        assert!(
            state.pending_press.is_some(),
            "press during Processing latches pending_press"
        );

        // The drain consumes it: latch cleared, the recording starts.
        assert!(matches!(
            state.on_processing_finished(),
            Some(Effect::Start { .. })
        ));
        assert!(
            state.pending_press.is_none(),
            "drain clears the latch with the press it consumed"
        );

        // Cancel while busy abandons a remembered press: latch cleared,
        // nothing starts on the drain.
        state.on_input(toggle_input(true), now + Duration::from_millis(300));
        assert!(state.stage == Stage::Processing);
        assert!(state
            .on_input(toggle_input(true), now + Duration::from_millis(400))
            .is_none());
        assert!(state.pending_press.is_some());
        state.on_cancel(false);
        assert!(
            state.pending_press.is_none(),
            "cancel abandons the remembered press"
        );
        assert!(state.on_processing_finished().is_none());
    }

    #[test]
    fn command_mode_binding_is_not_a_recording_lifecycle_binding() {
        assert!(is_transcribe_binding("transcribe"));
        assert!(is_transcribe_binding("transcribe_with_post_process"));
        assert!(
            is_transcribe_binding("transcribe_companion"),
            "the companion binding drives the same recording lifecycle as the keyboard triggers"
        );
        assert!(
            !is_transcribe_binding("transcribe_commands"),
            "the command binding is a during-dictation modifier, never a recording trigger"
        );
        assert!(!is_transcribe_binding("delete_last_word"));
        assert!(!is_transcribe_binding("undo"));
        assert!(!is_transcribe_binding("cancel"));
    }

    // ---------------------------------------------------------------------
    // Companion-device edges (phones/tablets on the LAN). Same lifecycle,
    // same one-only-session rule; network edges are external, so they are
    // never debounced (dropping one desyncs the phone's button state).
    // ---------------------------------------------------------------------

    const COMPANION_BINDING: &str = "transcribe_companion";

    fn companion_edge(pressed: bool, mode: ShortcutActivation) -> InputEvent {
        companion_edge_with_prefs(pressed, mode, None)
    }

    fn companion_edge_with_prefs(
        pressed: bool,
        mode: ShortcutActivation,
        prefs: Option<crate::companion::protocol::SessionPrefs>,
    ) -> InputEvent {
        InputEvent {
            binding_id: COMPANION_BINDING.to_string(),
            hotkey_string: "companion".to_string(),
            is_pressed: pressed,
            mode,
            hold_threshold: Duration::from_millis(300),
            external: true,
            companion_prefs: prefs,
        }
    }

    /// v1.6.0 prefs handoff (the arm-to-freeze race fix): the companion
    /// prefs a press edge carries ride INTO the Start effect it triggers,
    /// so the executor applies exactly the starting press's prefs to the
    /// live slot at the single serialized instant it commits the start -
    /// a concurrent press from another phone queues behind this command
    /// and can only arm the NEXT session. The remembered-press drain must
    /// carry the remembered press's prefs the same way, and keyboard
    /// starts carry None.
    #[test]
    fn companion_start_effects_carry_the_pressing_edges_prefs() {
        let phone_a = crate::companion::protocol::SessionPrefs {
            output_target: crate::companion::protocol::OutputTarget::Phone,
            post_process: true,
        };
        let phone_b = crate::companion::protocol::SessionPrefs {
            output_target: crate::companion::protocol::OutputTarget::Phone,
            post_process: false,
        };

        // A press with prefs A produces a Start effect carrying exactly A.
        let mut state = CoordinatorState::new();
        let effect = state.on_input(
            companion_edge_with_prefs(true, ShortcutActivation::Toggle, Some(phone_a)),
            Instant::now(),
        );
        assert!(matches!(
            effect,
            Some(Effect::Start { companion_prefs: Some(p), .. }) if p == phone_a
        ));

        // A remembered press drained after a busy pipeline starts its own
        // session with ITS prefs (B), not whatever a later press armed.
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        assert!(matches!(
            state.on_input(ptt_input(true), t0),
            Some(Effect::Start { .. })
        ));
        assert!(state
            .on_input(ptt_input(false), t0 + Duration::from_millis(100))
            .is_none());
        assert!(matches!(
            state.on_grace_expired(),
            Some(Effect::Stop { .. })
        ));
        // Pipeline busy: the companion press (prefs B) is remembered.
        assert!(state
            .on_input(
                companion_edge_with_prefs(true, ShortcutActivation::Toggle, Some(phone_b)),
                t0 + Duration::from_millis(50),
            )
            .is_none());
        let effect = state.on_processing_finished();
        assert!(matches!(
            effect,
            Some(Effect::Start { companion_prefs: Some(p), .. }) if p == phone_b
        ));

        // Keyboard starts never carry companion prefs.
        let mut state = CoordinatorState::new();
        assert!(matches!(
            state.on_input(toggle_input(true), Instant::now()),
            Some(Effect::Start {
                companion_prefs: None,
                ..
            })
        ));
    }

    /// The executor half of the prefs handoff, pinned against this
    /// committed source (the emit arms need a Tauri AppHandle a unit test
    /// cannot mount, the same convention as the badge wiring pin): the
    /// Start effect applies the carried prefs through the companion seam
    /// BEFORE start()'s preamble runs, and never reads the shared pending
    /// slot back.
    #[test]
    fn start_effect_applies_carried_prefs_before_the_preamble() {
        let source = include_str!("transcription_coordinator.rs");
        let run_effect_at = source.find("fn run_effect").expect("run_effect");
        let start_arm_at = run_effect_at
            + source[run_effect_at..]
                .find("Effect::Start {")
                .expect("the Start arm");
        let start_arm_end = start_arm_at
            + source[start_arm_at..]
                .find("Effect::Stop {")
                .expect("the arm boundary");
        let start_arm = &source[start_arm_at..start_arm_end];
        let apply_at = start_arm
            .find("begin_session_prefs(app, prefs);")
            .expect("the Start arm applies carried prefs through the companion seam");
        let preamble_at = start_arm
            .find("start(app, &binding_id")
            .expect("the start preamble call");
        assert!(
            apply_at < preamble_at,
            "carried prefs must be applied BEFORE the start preamble: applying them after reopens the arm-to-freeze window the fix closes"
        );
        assert!(
            !start_arm.contains("pending_session_prefs"),
            "the Start effect must never read the shared pending slot back: another connection can write it concurrently"
        );
    }

    #[test]
    fn companion_push_to_talk_press_and_release_drive_one_session() {
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();

        assert!(matches!(
            state.on_input(companion_edge(true, ShortcutActivation::PushToTalk), t0),
            Some(Effect::Start { .. })
        ));
        assert!(matches!(state.stage, Stage::Recording(_)));
        // Push-to-talk: the release is deferred by the same release grace a
        // physical key gets, then stops the session when the grace expires.
        assert!(state
            .on_input(
                companion_edge(false, ShortcutActivation::PushToTalk),
                t0 + Duration::from_secs(2)
            )
            .is_none());
        assert!(matches!(
            state.on_grace_expired(),
            Some(Effect::Stop { .. })
        ));
        assert_eq!(state.stage, Stage::Processing);
    }

    #[test]
    fn companion_press_while_local_binding_records_is_arbitrated_not_silent() {
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        assert!(matches!(
            state.on_input(toggle_input(true), t0),
            Some(Effect::Start { .. })
        ));

        // A phone press during the local hotkey's session is swallowed to
        // protect the live session - but NOT silently: the busy notice
        // names the companion binding so it can be forwarded to the phone.
        let effect = state.on_input(
            companion_edge(true, ShortcutActivation::PushToTalk),
            t0 + Duration::from_millis(100),
        );
        assert_eq!(
            effect,
            Some(Effect::NotifyRecordingBusy {
                binding_id: COMPANION_BINDING.to_string()
            })
        );
        // The local session is untouched.
        assert!(matches!(state.stage, Stage::Recording(_)));

        // Symmetrically, the keyboard pressing during a companion session
        // gets the same arbitration. v1.6.0 W1-L7: the NORMAL hotkey is the
        // arbitration case here - the pp hotkey pressed during a companion
        // session deliberately toggles the session's mode instead (see
        // pp_press_during_live_companion_session_toggles).
        let mut state = CoordinatorState::new();
        assert!(matches!(
            state.on_input(companion_edge(true, ShortcutActivation::Toggle), t0),
            Some(Effect::Start { .. })
        ));
        let effect = state.on_input(toggle_input(true), t0 + Duration::from_millis(50));
        assert!(matches!(
            effect,
            Some(Effect::NotifyRecordingBusy { binding_id }) if binding_id == BINDING
        ));
    }

    #[test]
    fn companion_edges_are_never_debounced() {
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();

        // Two presses 10 ms apart (inside the 30 ms debounce window): the
        // first starts, the second stops the toggle session. A debounced
        // second edge would wedge the phone's button state on.
        assert!(matches!(
            state.on_input(companion_edge(true, ShortcutActivation::Toggle), t0),
            Some(Effect::Start { .. })
        ));
        assert!(matches!(
            state.on_input(
                companion_edge(true, ShortcutActivation::Toggle),
                t0 + Duration::from_millis(10)
            ),
            Some(Effect::Stop { .. })
        ));
        assert_eq!(state.stage, Stage::Processing);
    }

    #[test]
    fn companion_hold_or_toggle_short_tap_locks_and_release_after_hold_stops() {
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();

        assert!(matches!(
            state.on_input(companion_edge(true, ShortcutActivation::HoldOrToggle), t0),
            Some(Effect::Start { .. })
        ));
        // Release before the 300 ms threshold: a tap, session locks on
        // (the release grace finds nothing deferred; a locked session
        // ignores releases).
        assert!(state
            .on_input(
                companion_edge(false, ShortcutActivation::HoldOrToggle),
                t0 + Duration::from_millis(120)
            )
            .is_none());
        assert!(state.on_grace_expired().is_none());
        assert!(matches!(state.stage, Stage::Recording(_)));

        // A second press stops the locked session.
        assert!(matches!(
            state.on_input(
                companion_edge(true, ShortcutActivation::HoldOrToggle),
                t0 + Duration::from_secs(1)
            ),
            Some(Effect::Stop { .. })
        ));
    }

    // ---------------------------------------------------------------------
    // Forced companion finalize: the synthesized "the source is over"
    // edges (phone disconnect, 15-minute cap, server stop) must end the
    // companion's session even when it is locked - a locked session ignores
    // release edges by design, so routing these through a release would
    // strand the recording with no incoming audio.
    // ---------------------------------------------------------------------

    #[test]
    fn finalize_companion_ends_a_locked_toggle_session() {
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();

        assert!(matches!(
            state.on_input(companion_edge(true, ShortcutActivation::Toggle), t0),
            Some(Effect::Start { .. })
        ));
        // The premise of the bug: a release edge is a no-op on the locked
        // toggle session.
        assert!(state
            .on_input(
                companion_edge(false, ShortcutActivation::Toggle),
                t0 + Duration::from_secs(60)
            )
            .is_none());
        assert!(matches!(state.stage, Stage::Recording(_)));

        // The forced finalize ends it: the ordinary Stop effect runs, so
        // everything captured transcribes and pastes.
        assert!(matches!(
            state.on_finalize_companion(),
            Some(Effect::Stop { .. })
        ));
        assert_eq!(state.stage, Stage::Processing);
    }

    #[test]
    fn finalize_companion_ends_a_locked_hold_or_toggle_tap_session() {
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();

        assert!(matches!(
            state.on_input(companion_edge(true, ShortcutActivation::HoldOrToggle), t0),
            Some(Effect::Start { .. })
        ));
        // Short tap: the session locks on.
        assert!(state
            .on_input(
                companion_edge(false, ShortcutActivation::HoldOrToggle),
                t0 + Duration::from_millis(120)
            )
            .is_none());
        assert!(state.on_grace_expired().is_none());
        assert!(state.is_locked());

        assert!(matches!(
            state.on_finalize_companion(),
            Some(Effect::Stop { .. })
        ));
        assert_eq!(state.stage, Stage::Processing);
    }

    #[test]
    fn finalize_companion_drops_a_remembered_companion_press() {
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();

        // A companion press remembered while the pipeline is busy would
        // start a locked session from a phone that no longer exists.
        assert!(matches!(
            state.on_input(companion_edge(true, ShortcutActivation::Toggle), t0),
            Some(Effect::Start { .. })
        ));
        assert!(matches!(
            state.on_input(
                companion_edge(true, ShortcutActivation::Toggle),
                t0 + Duration::from_millis(100)
            ),
            Some(Effect::Stop { .. })
        ));
        assert!(state
            .on_input(
                companion_edge(true, ShortcutActivation::Toggle),
                t0 + Duration::from_millis(200)
            )
            .is_none());
        assert!(state.pending_press.is_some());

        // Finalize during Processing: the remembered press goes with the
        // session, so the drain starts nothing.
        assert!(state.on_finalize_companion().is_none());
        assert!(state.pending_press.is_none());
        assert!(state.on_processing_finished().is_none());
        assert_eq!(state.stage, Stage::Idle);
    }

    #[test]
    fn finalize_companion_leaves_keyboard_sessions_alone() {
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();

        // A locked keyboard toggle session is NOT the companion's to stop.
        assert!(matches!(
            state.on_input(toggle_input(false), t0),
            Some(Effect::Start { .. })
        ));
        assert!(state.is_locked());

        assert_eq!(
            state.on_finalize_companion(),
            None,
            "the forced finalize must not stop a keyboard session"
        );
        assert!(matches!(state.stage, Stage::Recording(_)));

        // Idle and already-Processing states are equally untouched.
        let mut idle = CoordinatorState::new();
        assert!(idle.on_finalize_companion().is_none());
    }

    #[test]
    fn finalize_companion_clears_a_deferred_companion_release() {
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();

        assert!(matches!(
            state.on_input(companion_edge(true, ShortcutActivation::PushToTalk), t0),
            Some(Effect::Start { .. })
        ));
        // Deferred release inside its grace window.
        assert!(state
            .on_input(
                companion_edge(false, ShortcutActivation::PushToTalk),
                t0 + Duration::from_millis(10)
            )
            .is_none());
        assert!(state.pending_release.is_some());

        // Finalize wins: the Stop effect is immediate and no grace timer is
        // left armed to fire afterwards.
        assert!(matches!(
            state.on_finalize_companion(),
            Some(Effect::Stop { .. })
        ));
        assert!(state.pending_release.is_none());
        assert!(state.on_grace_expired().is_none());
    }

    // ---------------------------------------------------------------------
    // Undo activity window: ONLY while a recording session is live. The
    // operator rule forbids any post-session activity; a completed paste
    // arms nothing.
    // ---------------------------------------------------------------------

    /// Undo is armed exactly while the session is live: idle and ended
    /// sessions leave it inert, with no post-paste grace of any kind (the
    /// recording mirror `is_undo_active` delegates to follows this stage).
    #[test]
    fn undo_active_exactly_while_session_is_live() {
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        assert!(
            !matches!(state.stage, Stage::Recording(..)),
            "idle: undo must be inert"
        );
        assert!(matches!(
            state.on_input(toggle_input(true), t0),
            Some(Effect::Start { .. })
        ));
        assert!(matches!(state.stage, Stage::Recording(..)));
        // Stop the session: undo goes inert the same instant, no grace.
        let _ = state.on_input(toggle_input(false), t0 + Duration::from_secs(5));
        assert!(
            !matches!(state.stage, Stage::Recording(..)),
            "session ended: undo must be inert immediately"
        );
    }

    // ---------------------------------------------------------------------
    // Command-modifier state machine: press during a live session engages
    // command interpretation, release disengages, session end clears it
    // even while held, and a press with no live session does nothing.
    // ---------------------------------------------------------------------

    #[test]
    fn command_modifier_press_during_live_session_engages() {
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        assert!(matches!(
            state.on_input(toggle_input(true), t0),
            Some(Effect::Start { .. })
        ));

        assert_eq!(state.on_command_modifier(true, t0), None);
        assert!(state.command_modifier);
    }

    #[test]
    fn command_modifier_release_returns_to_normal() {
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        state.on_input(toggle_input(true), t0);
        assert_eq!(state.on_command_modifier(true, t0), None);
        assert!(state.command_modifier);

        assert_eq!(state.on_command_modifier(false, t0), None);
        assert!(!state.command_modifier);
        // The session itself is untouched by the modifier.
        assert_eq!(state.stage, Stage::Recording(BINDING.to_string()));
    }

    #[test]
    fn command_modifier_cleared_when_session_ends_while_held() {
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        state.on_input(toggle_input(true), t0);
        assert_eq!(state.on_command_modifier(true, t0), None);
        assert!(state.command_modifier);

        // The dictation finalizes while the modifier is still held: the
        // modifier is cleared cleanly, and a later release stays inert.
        assert!(matches!(
            state.on_input(toggle_input(true), t0 + Duration::from_secs(5)),
            Some(Effect::Stop { .. })
        ));
        assert!(!state.command_modifier);
        assert_eq!(
            state.on_command_modifier(false, t0 + Duration::from_secs(6)),
            None
        );
        assert!(!state.command_modifier);
    }

    // ---------------------------------------------------------------------
    // KB-221: the missed-hold outbox. A press..release pair observed while
    // a dictation session is live is boxed as wall-clock edges for the
    // streaming path, which resolves holds that fall entirely between two
    // text-bearing snapshots. The pair requires a live-session press, is
    // consumed once, and cannot complete across a session boundary.
    // ---------------------------------------------------------------------

    #[test]
    fn command_modifier_missed_hold_boxes_press_release_edges() {
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        state.on_input(toggle_input(true), t0);

        let press_at = t0 + Duration::from_millis(2_000);
        let release_at = t0 + Duration::from_millis(2_600);
        assert!(state.command_hold_edges.is_none(), "nothing boxed yet");
        state.on_command_modifier(true, press_at);
        assert!(
            state.command_hold_edges.is_none(),
            "the pair completes only on release"
        );
        state.on_command_modifier(false, release_at);
        assert_eq!(
            state.take_command_hold_edges(),
            Some((press_at, release_at))
        );
        assert!(
            state.take_command_hold_edges().is_none(),
            "the outbox is consumed exactly once"
        );
    }

    #[test]
    fn command_modifier_missed_hold_requires_a_live_session_press() {
        // An idle press never engages, so its release boxes nothing.
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        state.on_command_modifier(true, t0);
        state.on_command_modifier(false, t0 + Duration::from_millis(400));
        assert!(state.take_command_hold_edges().is_none());

        // A release with no preceding engage is equally inert.
        let mut stray = CoordinatorState::new();
        stray.on_command_modifier(false, t0);
        assert!(stray.take_command_hold_edges().is_none());
    }

    #[test]
    fn command_modifier_missed_hold_cannot_complete_across_session_end() {
        // Engage during the live session, the session finalizes BEFORE the
        // release: the press bookkeeping is dropped, so the later release
        // (already inert - the modifier was cleared) boxes no pair. A
        // completed pair, by contrast, SURVIVES begin_processing for the
        // stop pipeline's finalize fold to drain.
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        state.on_input(toggle_input(true), t0);
        state.on_command_modifier(true, t0 + Duration::from_secs(1));
        assert!(matches!(
            state.on_input(toggle_input(true), t0 + Duration::from_secs(5)),
            Some(Effect::Stop { .. })
        ));
        state.on_command_modifier(false, t0 + Duration::from_secs(6));
        assert!(
            state.take_command_hold_edges().is_none(),
            "a press cannot complete into a hold across the session boundary"
        );

        let mut completed = CoordinatorState::new();
        completed.on_input(toggle_input(true), t0);
        completed.on_command_modifier(true, t0 + Duration::from_secs(1));
        completed.on_command_modifier(false, t0 + Duration::from_millis(1_400));
        assert_eq!(
            completed.take_command_hold_edges(),
            Some((
                t0 + Duration::from_secs(1),
                t0 + Duration::from_millis(1_400)
            ))
        );
        // ...and the finalize transition itself keeps the boxed pair.
        let mut kept = CoordinatorState::new();
        kept.on_input(toggle_input(true), t0);
        kept.on_command_modifier(true, t0 + Duration::from_secs(1));
        kept.on_command_modifier(false, t0 + Duration::from_millis(1_400));
        assert!(matches!(
            kept.on_input(toggle_input(true), t0 + Duration::from_secs(5)),
            Some(Effect::Stop { .. })
        ));
        assert!(
            kept.take_command_hold_edges().is_some(),
            "a completed pair survives the stop for the finalize fold to drain"
        );
    }

    #[test]
    fn missed_hold_freshness_bounds_the_resolvable_window() {
        let released_at = Instant::now();
        assert!(missed_hold_pair_is_fresh(
            released_at,
            released_at + Duration::from_secs(4)
        ));
        assert!(!missed_hold_pair_is_fresh(
            released_at,
            released_at + Duration::from_secs(6)
        ));
    }

    /// The overlay badge tracks every real modifier transition through the
    /// notification queue: engage and release each enqueue one event, idle
    /// presses and redundant clears enqueue nothing (the badge must not
    /// flicker on no-ops).
    #[test]
    fn command_modifier_badge_notifications_track_every_transition() {
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        state.on_input(toggle_input(true), t0);

        // Engage during the live session: one badge-on notification.
        assert_eq!(state.on_command_modifier(true, t0), None);
        assert_eq!(
            state.drain_modifier_notifications(),
            vec![Effect::CommandModifierChanged { active: true }]
        );
        assert!(state.drain_modifier_notifications().is_empty(), "drained");

        // Release: one badge-off notification.
        assert_eq!(state.on_command_modifier(false, t0), None);
        assert_eq!(
            state.drain_modifier_notifications(),
            vec![Effect::CommandModifierChanged { active: false }]
        );

        // A redundant release (modifier already off) stays silent.
        assert_eq!(state.on_command_modifier(false, t0), None);
        assert!(state.drain_modifier_notifications().is_empty());

        // An idle press (session over) notifies idle, never the badge.
        assert!(matches!(
            state.on_input(toggle_input(true), t0 + Duration::from_secs(5)),
            Some(Effect::Stop { .. })
        ));
        assert!(matches!(
            state.on_command_modifier(true, t0 + Duration::from_secs(6)),
            Some(Effect::NotifyCommandIdle)
        ));
        assert!(state.drain_modifier_notifications().is_empty());
    }

    /// The session-end release clears the badge even though no release event
    /// ever arrives: finalize (begin_processing) and cancel both enqueue the
    /// badge-off notification while the modifier was engaged.
    #[test]
    fn command_modifier_badge_clears_on_session_end() {
        // Finalize path: dictation stops while the modifier is held.
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        state.on_input(toggle_input(true), t0);
        assert_eq!(state.on_command_modifier(true, t0), None);
        state.drain_modifier_notifications();
        assert!(matches!(
            state.on_input(toggle_input(true), t0 + Duration::from_secs(5)),
            Some(Effect::Stop { .. })
        ));
        assert_eq!(
            state.drain_modifier_notifications(),
            vec![Effect::CommandModifierChanged { active: false }],
            "the badge must clear when the session finalizes under a held modifier"
        );

        // Cancel path: session cancelled while held clears it the same way.
        let mut cancelled = CoordinatorState::new();
        cancelled.on_input(toggle_input(true), t0);
        assert_eq!(cancelled.on_command_modifier(true, t0), None);
        cancelled.drain_modifier_notifications();
        cancelled.on_cancel(true);
        assert_eq!(
            cancelled.drain_modifier_notifications(),
            vec![Effect::CommandModifierChanged { active: false }]
        );
    }

    /// KB-227: the run_effect badge gate. The state machine still enqueues
    /// every engage transition (the notification tests above), but the shell
    /// may only PUBLISH the engage while a live streaming run exists - the
    /// snapshot emitter, the modifier's only consumer, runs only then; on
    /// ONNX/batch models the chip must stay dark because nothing interprets
    /// commands. The pure predicate below is the exact decision the emit arm
    /// applies (the arm itself needs the Tauri app handle, which the pure
    /// harness cannot build - that is the test boundary).
    #[test]
    fn command_modifier_badge_engage_requires_a_live_stream() {
        // No stream (ONNX/batch model, or before the stream begins): the
        // engage edge is suppressed - the badge must not claim commands.
        assert!(!command_modifier_badge_publishable(true, false));
        // A stream in flight (set before the first snapshot): published.
        assert!(command_modifier_badge_publishable(true, true));
    }

    /// KB-227: every disengage edge publishes unconditionally - key release,
    /// cancel, failed-start rollback, session end - whatever the stream
    /// state, so a suppressed engage can never leave the badge stuck on.
    #[test]
    fn command_modifier_badge_disengage_publishes_unconditionally() {
        assert!(
            command_modifier_badge_publishable(false, false),
            "release/session-end clear must publish even with no stream"
        );
        assert!(command_modifier_badge_publishable(false, true));
    }

    /// A press for a different transcribe binding while one is recording is
    /// swallowed to protect the live session, but no longer silently: it
    /// returns the busy notice effect and leaves the session untouched.
    /// v1.6.0 W1-L7: the pp hotkey is the one deliberate exception (it
    /// toggles the session's mode instead), so this pin uses the companion
    /// binding as the generic other-binding press.
    #[test]
    fn cross_binding_press_while_recording_notifies_busy() {
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        assert!(matches!(
            state.on_input(toggle_input(true), t0),
            Some(Effect::Start { .. })
        ));

        // A different dictation binding presses: swallowed, and surfaced.
        assert_eq!(
            state.on_input(
                companion_edge(true, ShortcutActivation::Toggle),
                t0 + Duration::from_millis(50),
            ),
            Some(Effect::NotifyRecordingBusy {
                binding_id: COMPANION_BINDING.to_string()
            })
        );
        // The live session is untouched by the swallowed press.
        assert_eq!(state.stage, Stage::Recording(BINDING.to_string()));

        // The same binding's next press still stops the session normally.
        assert!(matches!(
            state.on_input(toggle_input(true), t0 + Duration::from_millis(100)),
            Some(Effect::Stop { .. })
        ));
    }

    // ---------------------------------------------------------------------
    // v1.6.0 W1-L7: post-process mode clarity + mid-session switching.
    //
    // The pp hotkey pressed while ANOTHER binding's session is live never
    // competes with it: the press toggles that session's post-process mode
    // (upgrade, revert, upgrade again), the badge tracks every transition,
    // and the override survives the session end for the stop pipeline to
    // honor the FINAL mode. The pp hotkey's OWN session keeps its lifecycle
    // semantics (the press stops it, as every same-binding press does).
    // ---------------------------------------------------------------------

    fn pp_toggle_input() -> InputEvent {
        toggle_input_for(POST_PROCESS_BINDING_ID, false)
    }

    /// The routing decision itself: a pp press during a live normal session
    /// produces the toggle effect - never a second Start, never a Stop of
    /// the live session, never the busy notice - and leaves the session
    /// recording under its original binding.
    #[test]
    fn pp_press_during_live_normal_session_toggles_instead_of_competing() {
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        assert!(matches!(
            state.on_input(toggle_input(true), t0),
            Some(Effect::Start { .. })
        ));

        assert_eq!(
            state.on_input(pp_toggle_input(), t0 + Duration::from_millis(500)),
            Some(Effect::ToggleSessionPostProcess),
            "a pp press during a live normal session toggles its mode"
        );
        // The live session is exactly as it was: still recording, same
        // binding, no second recording started.
        assert_eq!(state.stage, Stage::Recording(BINDING.to_string()));

        // The release of the pp hotkey is equally inert for the lifecycle.
        let release = InputEvent {
            is_pressed: false,
            ..pp_toggle_input()
        };
        assert!(state
            .on_input(release, t0 + Duration::from_millis(600))
            .is_none());
        assert_eq!(state.stage, Stage::Recording(BINDING.to_string()));
    }

    /// The toggle applies to a companion session identically: a live phone
    /// session can be upgraded (or, when its own preference already armed
    /// post-process, reverted) from the keyboard hotkey without touching
    /// the recording.
    #[test]
    fn pp_press_during_live_companion_session_toggles() {
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        assert!(matches!(
            state.on_input(companion_edge(true, ShortcutActivation::Toggle), t0),
            Some(Effect::Start { .. })
        ));

        assert_eq!(
            state.on_input(pp_toggle_input(), t0 + Duration::from_millis(400)),
            Some(Effect::ToggleSessionPostProcess)
        );
        assert_eq!(state.stage, Stage::Recording(COMPANION_BINDING.to_string()));
    }

    /// The pp hotkey's OWN session keeps its lifecycle semantics: the press
    /// stops the locked session (the only way a toggle-mode pp session
    /// ends), it does not toggle its own mode.
    #[test]
    fn pp_press_on_own_session_stops_it_instead_of_toggling() {
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        assert!(matches!(
            state.on_input(pp_toggle_input(), t0),
            Some(Effect::Start { .. })
        ));
        assert_eq!(
            state.stage,
            Stage::Recording(POST_PROCESS_BINDING_ID.to_string())
        );

        assert!(matches!(
            state.on_input(pp_toggle_input(), t0 + Duration::from_secs(2)),
            Some(Effect::Stop { .. })
        ));
        assert_eq!(state.stage, Stage::Processing);
    }

    /// The busy-pipeline window keeps the pre-v1.6.0 semantics for the pp
    /// binding: a press while the PREVIOUS session is still processing is a
    /// remembered press for the next session, not a mode toggle (the live
    /// session is gone; there is nothing to toggle).
    #[test]
    fn pp_press_during_processing_is_remembered_not_a_toggle() {
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        drive_into_processing(&mut state, t0);

        assert!(state
            .on_input(pp_toggle_input(), t0 + Duration::from_millis(200))
            .is_none());
        assert!(
            state
                .pending_press
                .as_ref()
                .is_some_and(|p| p.binding_id == POST_PROCESS_BINDING_ID),
            "the busy pp press queues the next session as it always did"
        );
        match state.on_processing_finished() {
            Some(Effect::Start { binding_id, .. }) => {
                assert_eq!(binding_id, POST_PROCESS_BINDING_ID)
            }
            other => panic!("expected a pp Start on the drain, got {other:?}"),
        }
    }

    /// The mode flip's truth table: a session with no in-session decision
    /// upgrades; an armed mode (pp start, companion pp, or a previous
    /// upgrade) reverts; a reverted session upgrades again.
    #[test]
    fn toggled_session_post_process_flips_every_state() {
        assert!(toggled_session_post_process(None), "normal -> upgraded");
        assert!(
            !toggled_session_post_process(Some(true)),
            "post-processed -> reverted"
        );
        assert!(
            toggled_session_post_process(Some(false)),
            "reverted -> upgraded again"
        );
    }

    /// The badge tracks every real mode transition through the notification
    /// queue: arming lights it once, a revert clears it once, redundant
    /// writes stay silent (the chip must not flicker on no-ops).
    #[test]
    fn pp_badge_notifications_track_every_mode_transition() {
        let mut state = CoordinatorState::new();

        // Arm (what the start arm or the toggle arm does for a pp session).
        state.set_session_post_process(Some(true));
        assert_eq!(
            state.drain_post_process_notifications(),
            vec![Effect::PostProcessModeChanged { active: true }]
        );
        assert!(
            state.drain_post_process_notifications().is_empty(),
            "drained"
        );

        // A redundant write of the same mode enqueues nothing.
        state.set_session_post_process(Some(true));
        assert!(state.drain_post_process_notifications().is_empty());

        // The in-session revert (the toggle arm's second press).
        state.set_session_post_process(Some(false));
        assert_eq!(
            state.drain_post_process_notifications(),
            vec![Effect::PostProcessModeChanged { active: false }]
        );

        // And back on (the third press).
        state.set_session_post_process(Some(true));
        assert_eq!(
            state.drain_post_process_notifications(),
            vec![Effect::PostProcessModeChanged { active: true }]
        );
    }

    /// The session-end discipline: finalize clears the badge (it must never
    /// outlive its session) while the override SURVIVES for the stop
    /// pipeline - the session's FINAL mode is what the pipeline honors.
    #[test]
    fn pp_badge_clears_on_session_end_while_the_override_survives_for_the_pipeline() {
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        state.on_input(toggle_input(true), t0);
        // The upgrade, as the toggle arm applies it.
        state.set_session_post_process(Some(true));
        state.drain_post_process_notifications();

        // Stop the session: badge off, override kept.
        assert!(matches!(
            state.on_input(toggle_input(true), t0 + Duration::from_secs(5)),
            Some(Effect::Stop { .. })
        ));
        assert_eq!(
            state.drain_post_process_notifications(),
            vec![Effect::PostProcessModeChanged { active: false }],
            "the badge must clear when the session finalizes"
        );
        assert_eq!(
            state.session_post_process,
            Some(true),
            "the override survives into the stop pipeline (the final mode)"
        );

        // A fresh session starts clean: the drain retires the finished
        // session's override (nothing may consume it anymore), and no badge
        // survives into the idle gap.
        assert!(state.on_processing_finished().is_none());
        assert_eq!(state.stage, Stage::Idle);
        assert_eq!(
            state.session_post_process, None,
            "the drain retires the finished session's in-session decision"
        );
        assert!(!state.pp_badge);
    }

    /// Cancel clears both halves: no stop pipeline follows a cancel, so
    /// nothing would consume the override, and the badge must not outlive
    /// the session the user abandoned.
    #[test]
    fn pp_state_clears_on_cancel() {
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        state.on_input(toggle_input(true), t0);
        state.set_session_post_process(Some(true));
        state.drain_post_process_notifications();

        state.on_cancel(true);
        assert_eq!(state.session_post_process, None);
        assert!(!state.pp_badge);
        assert_eq!(
            state.drain_post_process_notifications(),
            vec![Effect::PostProcessModeChanged { active: false }],
            "cancel retires the badge with the session"
        );
    }

    /// A failed start (microphone denied) rolls the optimistic transition
    /// back to idle: the badge a pp start would have lit goes dark with it,
    /// and the override cannot leak into the next session.
    #[test]
    fn pp_state_clears_on_failed_start_rollback() {
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        assert!(matches!(
            state.on_input(pp_toggle_input(), t0),
            Some(Effect::Start { .. })
        ));
        // What the start arm does for a pp-binding start that succeeded.
        state.set_session_post_process(Some(true));
        state.drain_post_process_notifications();

        state.on_start_result(POST_PROCESS_BINDING_ID, false);
        assert_eq!(state.stage, Stage::Idle);
        assert_eq!(state.session_post_process, None);
        assert!(!state.pp_badge);
        assert_eq!(
            state.drain_post_process_notifications(),
            vec![Effect::PostProcessModeChanged { active: false }],
            "the rollback retires the badge with the session that never was"
        );
    }

    /// The start-time resolution truth table: the pp hotkey's own session
    /// always post-processes; a companion session post-processes only when
    /// its frozen preference asks AND the master toggle is on; a keyboard
    /// normal session never does (without an in-session upgrade).
    #[test]
    fn started_session_post_processes_truth_table() {
        use started_session_post_processes as will_pp;
        // The pp hotkey's own session: unconditional.
        for (companion_pref, master) in [(false, false), (true, false), (true, true)] {
            assert!(will_pp(true, false, companion_pref, master));
        }
        // A keyboard normal session: never by start alone.
        for (companion_pref, master) in [(false, false), (true, false), (true, true)] {
            assert!(!will_pp(false, false, companion_pref, master));
        }
        // A companion session: the frozen preference, gated on the master.
        assert!(will_pp(false, true, true, true));
        assert!(!will_pp(false, true, true, false), "master off wins");
        assert!(!will_pp(false, true, false, true), "pref off stays off");
        assert!(!will_pp(false, true, false, false));
    }

    /// The full mid-session switching story on the pure machine: a normal
    /// session upgrades on the first pp press, reverts on the second, and
    /// the stop pipeline's final mode is the reverted one.
    #[test]
    fn mid_session_switching_upgrades_then_reverts() {
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        assert!(matches!(
            state.on_input(toggle_input(true), t0),
            Some(Effect::Start { .. })
        ));

        // First press: the toggle effect; the arm applies Some(true).
        assert_eq!(
            state.on_input(pp_toggle_input(), t0 + Duration::from_millis(500)),
            Some(Effect::ToggleSessionPostProcess)
        );
        state.set_session_post_process(Some(toggled_session_post_process(
            state.session_post_process,
        )));
        assert_eq!(state.session_post_process, Some(true));

        // Second press during the same live session: reverts.
        assert_eq!(
            state.on_input(pp_toggle_input(), t0 + Duration::from_millis(900)),
            Some(Effect::ToggleSessionPostProcess)
        );
        state.set_session_post_process(Some(toggled_session_post_process(
            state.session_post_process,
        )));
        assert_eq!(state.session_post_process, Some(false));

        // Stop: the pipeline reads Some(false) - the FINAL mode.
        assert!(matches!(
            state.on_input(toggle_input(true), t0 + Duration::from_secs(5)),
            Some(Effect::Stop { .. })
        ));
        assert_eq!(state.session_post_process, Some(false));
    }

    /// The badge payload's exact shape, pinned against the frontend mapper
    /// (ppBadgeStateFromPayload, src/overlay/ppBadge.ts): `active` gates
    /// the chip, `template` names it, and the off edge (or an on edge with
    /// no selected template) carries null - never a missing key.
    #[test]
    fn post_process_badge_payload_matches_the_frontend_contract() {
        assert_eq!(
            post_process_badge_payload(true, Some("Meeting Notes")),
            serde_json::json!({ "active": true, "template": "Meeting Notes" })
        );
        // On with no selected template: the chip renders its bare PP label.
        assert_eq!(
            post_process_badge_payload(true, None),
            serde_json::json!({ "active": true, "template": null })
        );
        // The off edge never names a template.
        assert_eq!(
            post_process_badge_payload(false, None),
            serde_json::json!({ "active": false, "template": null })
        );
    }

    /// The event name the frontend listens for, cross-pinned with the
    /// overlay module's POST_PROCESS_BADGE_EVENT (both sides assert the
    /// same literal, so renaming one without the other fails a test).
    #[test]
    fn post_process_badge_event_name_is_pinned() {
        assert_eq!(POST_PROCESS_BADGE_EVENT, "post-process-changed");
    }

    /// Review finding (W1-L7 round 2): the emit arms need a Tauri
    /// AppHandle a unit test cannot mount, so the WIRING that lights the
    /// badge on each path - the Start arm's fact list, the toggle arm's
    /// resolution and notices, and the emit's window/event/payload - is
    /// pinned against this committed source, the same convention as
    /// actions.rs's stop-pipeline pin. A dropped companion term in the
    /// Start arm, a renamed payload key, or an emit aimed at the wrong
    /// window or event fails HERE even though every pure test above stays
    /// green.
    #[test]
    fn badge_wiring_is_pinned_against_the_source() {
        let source = include_str!("transcription_coordinator.rs");

        // The Start arm: gates on `started`, feeds the resolver ALL four
        // facts (pp binding, companion binding, frozen companion pref,
        // master toggle), and stores the RESOLVED mode through the single
        // setter - unconditionally on the resolved value, so the master
        // toggle is read at exactly one instant per session and the stop
        // pipeline's override can never contradict the badge.
        let run_effect_at = source.find("fn run_effect").expect("run_effect");
        let start_arm_at = run_effect_at
            + source[run_effect_at..]
                .find("Effect::Start {")
                .expect("the Start arm");
        let start_arm_end = start_arm_at
            + source[start_arm_at..]
                .find("Effect::Stop {")
                .expect("the arm boundary");
        let start_arm = &source[start_arm_at..start_arm_end];
        for needle in [
            "if started",
            "started_session_post_processes(",
            "binding_id == POST_PROCESS_BINDING_ID",
            "binding_id == COMPANION_BINDING_ID",
            "session_prefs(app).post_process",
            "post_process_enabled",
            "state.set_session_post_process(Some(started_session_post_processes(",
        ] {
            assert!(
                start_arm.contains(needle),
                "the Start arm must keep wiring `{needle}` (badge-on for a pp or companion-pp start)"
            );
        }
        assert!(
            !start_arm.contains("&& started_session_post_processes"),
            "the mode store must not be conditional on the resolved value: a Some(false) \
freeze-at-start is what keeps a master-toggle flip mid-session from making \
the badge lie (dark-badge session silently post-processing at stop)"
        );

        // The toggle arm: resolves through the pure flip, applies it through
        // the single setter, and confirms both directions through the
        // notice channel with the template's name.
        let toggle_arm_at = run_effect_at
            + source[run_effect_at..]
                .find("Effect::ToggleSessionPostProcess =>")
                .expect("the toggle arm");
        let toggle_arm_end = toggle_arm_at
            + source[toggle_arm_at..]
                .find("Effect::PostProcessModeChanged")
                .expect("the arm boundary");
        let toggle_arm = &source[toggle_arm_at..toggle_arm_end];
        for needle in [
            "toggled_session_post_process(",
            "set_session_post_process(Some(next))",
            "NoticeCode::PostProcessSessionUpgraded",
            "NoticeCode::PostProcessSessionDowngraded",
            "selected_post_process_prompt_name()",
        ] {
            assert!(
                toggle_arm.contains(needle),
                "the toggle arm must keep wiring `{needle}` (mid-session switch + its confirmation)"
            );
        }

        // The emit: the overlay window, the pinned event name, and the
        // shared payload builder - never a hand-rolled json!.
        let emit_at = source
            .find("fn emit_post_process_badge")
            .expect("emit_post_process_badge");
        let emit_end = emit_at
            + source[emit_at..]
                .find("pub(crate) fn refresh_lit")
                .expect("the refresh helper that follows the emit fn");
        let emit_fn = &source[emit_at..emit_end];
        for needle in [
            "emit_to(",
            "\"recording_overlay\"",
            "POST_PROCESS_BADGE_EVENT",
            "post_process_badge_payload(",
        ] {
            assert!(
                emit_fn.contains(needle),
                "emit_post_process_badge must keep wiring `{needle}` (the overlay consumes it)"
            );
        }
    }

    #[test]
    fn command_modifier_press_with_no_live_session_notifies() {
        // Idle: the state stays untouched, but the press is not silent -
        // it asks for the command-mode-no-session feedback.
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        assert!(matches!(
            state.on_command_modifier(true, t0),
            Some(Effect::NotifyCommandIdle)
        ));
        assert!(!state.command_modifier);
        assert_eq!(state.stage, Stage::Idle);

        // Busy pipeline: equally inert for interpretation (the modifier
        // only modulates a LIVE session, never queues for one), with the
        // same feedback.
        let mut busy = CoordinatorState::new();
        drive_into_processing(&mut busy, t0);
        assert!(matches!(
            busy.on_command_modifier(true, t0),
            Some(Effect::NotifyCommandIdle)
        ));
        assert!(!busy.command_modifier);
        assert!(busy.on_processing_finished().is_none());
        assert_eq!(busy.stage, Stage::Idle);

        // Cancel path: a session cancelled while held also clears it.
        let mut cancelled = CoordinatorState::new();
        cancelled.on_input(toggle_input(true), t0);
        assert_eq!(cancelled.on_command_modifier(true, t0), None);
        cancelled.on_cancel(true);
        assert!(!cancelled.command_modifier);
        assert_eq!(cancelled.stage, Stage::Idle);
    }

    #[test]
    fn push_to_talk_release_while_recording_defers_release() {
        assert_eq!(
            classify_ptt_event(None, false, true, "transcribe", Some("transcribe")),
            PttAction::DeferRelease
        );
    }

    #[test]
    fn push_to_talk_press_matching_pending_release_cancels_release() {
        assert_eq!(
            classify_ptt_event(
                Some("transcribe"),
                true,
                true,
                "transcribe",
                Some("transcribe")
            ),
            PttAction::CancelRelease
        );
    }

    #[test]
    fn toggle_mode_press_and_release_pass_through() {
        assert_eq!(
            classify_ptt_event(
                Some("transcribe"),
                true,
                false,
                "transcribe",
                Some("transcribe")
            ),
            PttAction::Passthrough
        );
        assert_eq!(
            classify_ptt_event(None, false, false, "transcribe", Some("transcribe")),
            PttAction::Passthrough
        );
    }

    #[test]
    fn press_for_different_binding_than_pending_release_passes_through() {
        assert_eq!(
            classify_ptt_event(
                Some("transcribe"),
                true,
                true,
                "transcribe_with_post_process",
                Some("transcribe")
            ),
            PttAction::Passthrough
        );
    }

    #[test]
    fn press_matching_pending_release_cancels_without_recording_state() {
        assert_eq!(
            classify_ptt_event(Some("transcribe"), true, true, "transcribe", None),
            PttAction::CancelRelease
        );
    }

    // ---------------------------------------------------------------------
    // Busy-pipeline input classification.
    //
    // Toggle-style triggers (SIGUSR2, CLI flags, pedals that signal on both
    // edges) flip state on every edge. Dropping a press that arrives while
    // the previous pipeline is still processing desyncs the parity: the next
    // edge then starts a recording no one will stop, leaving the overlay
    // waiting for input with the button long released.
    // ---------------------------------------------------------------------

    #[test]
    fn toggle_press_during_processing_remembers_start() {
        assert_eq!(
            classify_busy_input(true, ShortcutActivation::Toggle, None),
            BusyAction::Remember
        );
    }

    #[test]
    fn second_toggle_press_during_processing_forgets_press() {
        assert_eq!(
            classify_busy_input(true, ShortcutActivation::Toggle, Some(Remembered::Locked)),
            BusyAction::Forget
        );
    }

    #[test]
    fn toggle_release_during_processing_is_ignored() {
        assert_eq!(
            classify_busy_input(false, ShortcutActivation::Toggle, None),
            BusyAction::Ignore
        );
        assert_eq!(
            classify_busy_input(false, ShortcutActivation::Toggle, Some(Remembered::Locked)),
            BusyAction::Ignore
        );
    }

    #[test]
    fn hold_modes_classify_busy_inputs_by_pending_state() {
        let cases = [
            (true, None, BusyAction::Remember),
            (true, Some(Remembered::Held), BusyAction::Ignore),
            (true, Some(Remembered::Locked), BusyAction::Forget),
            (false, None, BusyAction::Ignore),
            (false, Some(Remembered::Held), BusyAction::Ignore),
            (false, Some(Remembered::Locked), BusyAction::Ignore),
        ];

        for mode in [
            ShortcutActivation::PushToTalk,
            ShortcutActivation::HoldOrToggle,
        ] {
            for (is_pressed, remembered, expected) in cases {
                assert_eq!(classify_busy_input(is_pressed, mode, remembered), expected);
            }
        }
    }

    /// Toggle parity across a busy window: an odd number of presses remembers
    /// one start, each further press flips the remembered press off/on again.
    #[test]
    fn toggle_presses_alternate_remember_and_forget_while_busy() {
        let mut remembered = None;
        for expected in [
            BusyAction::Remember,
            BusyAction::Forget,
            BusyAction::Remember,
        ] {
            let action = classify_busy_input(true, ShortcutActivation::Toggle, remembered);
            assert_eq!(action, expected);
            remembered = (action == BusyAction::Remember).then_some(Remembered::Locked);
        }
        assert!(remembered.is_some());
    }

    // ---------------------------------------------------------------------
    // Sequence-level regression coverage for issue #1539.
    //
    // Under X11 key auto-repeat, holding a push-to-talk key does not emit one
    // long press. It emits the initial press followed by a stream of
    // synthesized release/press pairs, then a single genuine release on key-up.
    // Before the fix, every synthesized release passed straight through and
    // stopped recording, so holding the key "rapidly toggled" recording on and
    // off. The fix defers each release for a short grace window and cancels it
    // when the matching auto-repeat press arrives.
    //
    // The unit tests above assert the classifiers in isolation. The harness
    // below drives the real `CoordinatorState` through whole event sequences
    // - the same `on_input` / `on_grace_expired` handlers the coordinator
    // thread runs - so a burst can be exercised deterministically without a
    // Tauri AppHandle or real timers, and the tests can never drift from the
    // production transitions.
    // ---------------------------------------------------------------------

    const BINDING: &str = "transcribe";

    #[derive(Clone, Copy)]
    enum Ev {
        /// A key-down event (real initial press or a synthesized auto-repeat press).
        Press,
        /// A key-up event (synthesized auto-repeat release or the genuine key-up).
        Release,
        /// The `RELEASE_GRACE` window elapsed with no cancelling press arriving.
        Grace,
    }

    struct DriveResult {
        starts: u32,
        stops: u32,
        stage: Stage,
    }

    fn ptt_input(is_pressed: bool) -> InputEvent {
        InputEvent {
            binding_id: BINDING.to_string(),
            hotkey_string: BINDING.to_string(),
            is_pressed,
            mode: ShortcutActivation::PushToTalk,
            hold_threshold: Duration::ZERO,
            external: false,
            companion_prefs: None,
        }
    }

    /// Feeds an event sequence to a real [`CoordinatorState`] the way the
    /// coordinator thread would; effects are counted instead of executed.
    fn drive(events: &[Ev]) -> DriveResult {
        let mut state = CoordinatorState::new();
        let mut clock = Instant::now();
        let mut starts = 0u32;
        let mut stops = 0u32;

        for ev in events {
            // Auto-repeat events arrive a few ms apart, well inside DEBOUNCE.
            clock += Duration::from_millis(5);

            let effect = match ev {
                Ev::Grace => state.on_grace_expired(),
                Ev::Press | Ev::Release => {
                    state.on_input(ptt_input(matches!(ev, Ev::Press)), clock)
                }
            };
            match effect {
                Some(Effect::Start { .. }) => starts += 1,
                Some(Effect::Stop { .. }) => stops += 1,
                // The PTT drive never meets these (single binding, no
                // modifier events), but the match must stay exhaustive as
                // the Effect set grows.
                Some(Effect::NotifyCommandIdle)
                | Some(Effect::NotifyRecordingBusy { .. })
                | Some(Effect::CommandModifierChanged { .. })
                | Some(Effect::ToggleSessionPostProcess)
                | Some(Effect::PostProcessModeChanged { .. })
                | None => {}
            }
        }

        DriveResult {
            starts,
            stops,
            stage: state.stage,
        }
    }

    /// Initial press plus several synthesized release/press pairs, as X11 emits
    /// while a push-to-talk key is held down.
    fn autorepeat_burst() -> Vec<Ev> {
        let mut events = vec![Ev::Press];
        for _ in 0..6 {
            events.push(Ev::Release);
            events.push(Ev::Press);
        }
        events
    }

    /// Regression for #1539: a burst of X11 auto-repeat release/press pairs must
    /// not stop recording. Before the fix the first synthesized release stopped
    /// recording immediately (stops == 1, stage left Recording), which produced
    /// the rapid on/off toggling. With the fix the releases are coalesced and
    /// recording stays continuously active for the whole burst.
    #[test]
    fn x11_autorepeat_burst_does_not_toggle_recording() {
        let result = drive(&autorepeat_burst());
        assert_eq!(result.starts, 1, "recording should start exactly once");
        assert_eq!(
            result.stops, 0,
            "synthesized auto-repeat releases must not stop recording mid-burst"
        );
        assert_eq!(
            result.stage,
            Stage::Recording(BINDING.to_string()),
            "recording must remain active across the entire auto-repeat burst"
        );
    }

    /// Complements the burst test: once the key is genuinely released and the
    /// grace window elapses with no re-press, recording stops exactly once. This
    /// proves the debounce only coalesces synthesized releases and does not wedge
    /// the coordinator or swallow the real key-up.
    #[test]
    fn genuine_release_after_grace_stops_recording_once() {
        let mut events = autorepeat_burst();
        events.push(Ev::Release); // genuine key-up
        events.push(Ev::Grace); // grace window elapses, no cancelling press
        let result = drive(&events);
        assert_eq!(result.starts, 1, "recording should start exactly once");
        assert_eq!(
            result.stops, 1,
            "a genuine release should stop recording exactly once"
        );
        assert_eq!(result.stage, Stage::Processing);
    }

    // ---------------------------------------------------------------------
    // Sequence-level coverage of the busy-pipeline and cancel paths, driven
    // through the real machine.
    // ---------------------------------------------------------------------

    /// PTT press while the pipeline is busy is remembered and starts recording
    /// once the pipeline drains.
    #[test]
    fn press_during_processing_starts_after_drain() {
        let mut state = CoordinatorState::new();
        let now = Instant::now();

        let effect = state.on_input(ptt_input(true), now);
        assert!(matches!(effect, Some(Effect::Start { .. })));

        let effect = state.on_input(ptt_input(false), now + Duration::from_millis(100));
        assert!(effect.is_none(), "release should be deferred, not fired");

        let effect = state.on_grace_expired();
        assert!(matches!(effect, Some(Effect::Stop { .. })));

        let effect = state.on_input(ptt_input(true), now + Duration::from_millis(200));
        assert!(effect.is_none(), "busy pipeline must remember, not start");

        let effect = state.on_processing_finished();
        assert!(
            matches!(effect, Some(Effect::Start { .. })),
            "remembered press should start once the pipeline drains"
        );
    }

    /// Two toggle presses inside one busy window net to no-op: nothing starts
    /// when the pipeline drains (toggle parity).
    #[test]
    fn toggle_presses_during_processing_net_noop_after_drain() {
        let mut state = CoordinatorState::new();
        let now = Instant::now();

        let effect = state.on_input(ptt_input(true), now);
        assert!(matches!(effect, Some(Effect::Start { .. })));
        let effect = state.on_input(ptt_input(false), now + Duration::from_millis(100));
        assert!(effect.is_none());
        let effect = state.on_grace_expired();
        assert!(matches!(effect, Some(Effect::Stop { .. })));

        let toggle = |state: &mut CoordinatorState, at: Instant| {
            state.on_input(
                InputEvent {
                    binding_id: BINDING.to_string(),
                    hotkey_string: BINDING.to_string(),
                    is_pressed: true,
                    mode: ShortcutActivation::Toggle,
                    hold_threshold: Duration::ZERO,
                    external: true,
                    companion_prefs: None,
                },
                at,
            )
        };

        let effect = toggle(&mut state, now + Duration::from_millis(200));
        assert!(effect.is_none());
        let effect = toggle(&mut state, now + Duration::from_millis(300));
        assert!(effect.is_none());

        let effect = state.on_processing_finished();
        assert!(
            effect.is_none(),
            "even number of busy toggle presses must not start recording"
        );
        assert_eq!(state.stage, Stage::Idle);
    }

    /// Cancel while processing abandons a remembered press: the pipeline drains
    /// to idle and nothing starts.
    #[test]
    fn cancel_during_processing_drops_remembered_press() {
        let mut state = CoordinatorState::new();
        let now = Instant::now();

        let effect = state.on_input(ptt_input(true), now);
        assert!(matches!(effect, Some(Effect::Start { .. })));
        let effect = state.on_input(ptt_input(false), now + Duration::from_millis(100));
        assert!(effect.is_none());
        let effect = state.on_grace_expired();
        assert!(matches!(effect, Some(Effect::Stop { .. })));

        let effect = state.on_input(ptt_input(true), now + Duration::from_millis(200));
        assert!(effect.is_none());

        state.on_cancel(false);
        assert_eq!(
            state.stage,
            Stage::Processing,
            "cancel must not reset mid-processing - the pipeline still finishes"
        );

        let effect = state.on_processing_finished();
        assert!(
            effect.is_none(),
            "cancelled session must not spawn a deferred recording"
        );
        assert_eq!(state.stage, Stage::Idle);
    }

    fn toggle_input(external: bool) -> InputEvent {
        toggle_input_for(BINDING, external)
    }

    fn toggle_input_for(binding_id: &str, external: bool) -> InputEvent {
        InputEvent {
            binding_id: binding_id.to_string(),
            hotkey_string: binding_id.to_string(),
            is_pressed: true,
            mode: ShortcutActivation::Toggle,
            hold_threshold: Duration::ZERO,
            companion_prefs: None,
            external,
        }
    }

    /// Start and stop one toggle recording so the machine sits in `Processing`.
    fn drive_into_processing(state: &mut CoordinatorState, now: Instant) {
        let effect = state.on_input(toggle_input(true), now);
        assert!(matches!(effect, Some(Effect::Start { .. })));
        let effect = state.on_input(toggle_input(true), now + Duration::from_millis(100));
        assert!(matches!(effect, Some(Effect::Stop { .. })));
        assert_eq!(state.stage, Stage::Processing);
    }

    const OTHER_BINDING: &str = "transcribe_with_post_process";

    /// Only one press can be pending. Once a binding has claimed it, a toggle
    /// for a different binding is ignored (as it is while recording) instead of
    /// replacing the remembered press, so the pending binding's parity holds:
    /// two transcribe toggles still net to no-op.
    #[test]
    fn different_binding_does_not_replace_pending_press() {
        let mut state = CoordinatorState::new();
        let now = Instant::now();
        drive_into_processing(&mut state, now);

        let at = |ms| now + Duration::from_millis(ms);
        assert!(state.on_input(toggle_input(true), at(200)).is_none());
        assert!(state
            .on_input(toggle_input_for(OTHER_BINDING, true), at(300))
            .is_none());
        assert!(state.on_input(toggle_input(true), at(400)).is_none());

        let effect = state.on_processing_finished();
        assert!(
            effect.is_none(),
            "two transcribe toggles net to no-op; the ignored post-process toggle must not start"
        );
        assert_eq!(state.stage, Stage::Idle);
    }

    /// The binding that claimed the pending press is the one that starts on
    /// drain, regardless of other bindings toggled in between.
    #[test]
    fn drain_starts_the_pending_binding_not_a_later_one() {
        let mut state = CoordinatorState::new();
        let now = Instant::now();
        drive_into_processing(&mut state, now);

        let at = |ms| now + Duration::from_millis(ms);
        assert!(state.on_input(toggle_input(true), at(200)).is_none());
        assert!(state
            .on_input(toggle_input_for(OTHER_BINDING, true), at(300))
            .is_none());

        match state.on_processing_finished() {
            Some(Effect::Start { binding_id, .. }) => assert_eq!(binding_id, BINDING),
            other => panic!("expected Start for '{BINDING}', got {other:?}"),
        }
    }

    /// External triggers fire on every edge by design (e.g. SIGUSR2 sent on
    /// both key press and release). Two edges inside the debounce window must
    /// both be honoured, or the parity desyncs and recording wedges on.
    #[test]
    fn external_edges_inside_debounce_window_are_not_dropped() {
        let mut state = CoordinatorState::new();
        let now = Instant::now();

        let effect = state.on_input(toggle_input(true), now);
        assert!(matches!(effect, Some(Effect::Start { .. })));

        let effect = state.on_input(toggle_input(true), now + Duration::from_millis(5));
        assert!(
            matches!(effect, Some(Effect::Stop { .. })),
            "second external edge inside DEBOUNCE must stop the recording"
        );
        assert_eq!(state.stage, Stage::Processing);
    }

    /// Physical keyboard presses keep the debounce: a repeat inside the window
    /// is still dropped and recording stays active.
    #[test]
    fn keyboard_press_inside_debounce_window_is_still_dropped() {
        let mut state = CoordinatorState::new();
        let now = Instant::now();

        let effect = state.on_input(toggle_input(false), now);
        assert!(matches!(effect, Some(Effect::Start { .. })));

        let effect = state.on_input(toggle_input(false), now + Duration::from_millis(5));
        assert!(
            effect.is_none(),
            "keyboard repeat inside DEBOUNCE must be debounced"
        );
        assert_eq!(state.stage, Stage::Recording(BINDING.to_string()));
    }

    /// If the start effect fails to begin recording (e.g. microphone access
    /// denied), the optimistic transition rolls back to idle.
    #[test]
    fn failed_start_rolls_back_to_idle() {
        let mut state = CoordinatorState::new();

        let effect = state.on_input(ptt_input(true), Instant::now());
        assert!(matches!(effect, Some(Effect::Start { .. })));

        state.on_start_result(BINDING, false);
        assert_eq!(state.stage, Stage::Idle);
    }

    // ---------------------------------------------------------------------
    // Hold-or-toggle (the combined mode from #147) and the two legacy modes,
    // driven through the real machine on a synthetic clock. Recording starts
    // on key-down in every mode; the tests pin how each mode ends it.
    // ---------------------------------------------------------------------

    const HOLD_THRESHOLD: Duration = Duration::from_millis(300);

    fn input(mode: ShortcutActivation, is_pressed: bool) -> InputEvent {
        InputEvent {
            binding_id: BINDING.to_string(),
            hotkey_string: BINDING.to_string(),
            is_pressed,
            mode,
            hold_threshold: HOLD_THRESHOLD,
            external: false,
            companion_prefs: None,
        }
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Hold-or-toggle: a key held past the threshold is push-to-talk - the
    /// (deferred) release stops recording.
    #[test]
    fn hold_or_toggle_long_hold_stops_on_release() {
        let mode = ShortcutActivation::HoldOrToggle;
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();

        assert!(matches!(
            state.on_input(input(mode, true), t0),
            Some(Effect::Start { .. })
        ));
        assert!(state.on_input(input(mode, false), t0 + ms(800)).is_none());
        assert!(
            matches!(state.on_grace_expired(), Some(Effect::Stop { .. })),
            "an 800ms hold must stop when its release grace elapses"
        );
        assert_eq!(state.stage, Stage::Processing);
    }

    /// Hold-or-toggle: a tap keeps recording (locked on); the next press stops.
    #[test]
    fn hold_or_toggle_tap_locks_recording_until_next_press() {
        let mode = ShortcutActivation::HoldOrToggle;
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();

        assert!(matches!(
            state.on_input(input(mode, true), t0),
            Some(Effect::Start { .. })
        ));
        assert!(state.on_input(input(mode, false), t0 + ms(120)).is_none());
        assert!(
            state.on_grace_expired().is_none(),
            "a 120ms tap must not stop recording"
        );
        assert_eq!(state.stage, Stage::Recording(BINDING.to_string()));
        assert!(state.is_locked());

        // Seconds later the user presses again to finish.
        assert!(matches!(
            state.on_input(input(mode, true), t0 + ms(5000)),
            Some(Effect::Stop { .. })
        ));
        assert_eq!(state.stage, Stage::Processing);
        // The release of that stopping press lands in the busy window and is
        // ignored, so nothing is remembered for the drain.
        assert!(state.on_input(input(mode, false), t0 + ms(5080)).is_none());
        assert!(state.on_processing_finished().is_none());
        assert_eq!(state.stage, Stage::Idle);
    }

    /// Hold-or-toggle: a locked session ignores stray releases - only a press
    /// ends it.
    #[test]
    fn hold_or_toggle_locked_session_ignores_release() {
        let mode = ShortcutActivation::HoldOrToggle;
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();

        state.on_input(input(mode, true), t0);
        state.on_input(input(mode, false), t0 + ms(100));
        assert!(state.on_grace_expired().is_none());
        assert!(state.is_locked());

        assert!(state.on_input(input(mode, false), t0 + ms(900)).is_none());
        assert!(
            state.grace_deadline().is_none(),
            "no release may be deferred once locked"
        );
        assert_eq!(state.stage, Stage::Recording(BINDING.to_string()));
    }

    /// Hold-or-toggle: while the key is genuinely held, extra presses do not
    /// stop the recording (that is the release's job).
    #[test]
    fn hold_or_toggle_press_while_held_is_ignored() {
        let mode = ShortcutActivation::HoldOrToggle;
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();

        state.on_input(input(mode, true), t0);
        assert!(state.on_input(input(mode, true), t0 + ms(400)).is_none());
        assert_eq!(state.stage, Stage::Recording(BINDING.to_string()));
        assert!(!state.is_locked());
    }

    /// Hold-or-toggle under X11 auto-repeat: the synthesized release/press
    /// pairs must not be misread as taps. The hold is measured from the
    /// original key-down to the genuine key-up.
    #[test]
    fn hold_or_toggle_autorepeat_burst_is_one_long_hold() {
        let mode = ShortcutActivation::HoldOrToggle;
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        let mut clock = t0;

        assert!(matches!(
            state.on_input(input(mode, true), clock),
            Some(Effect::Start { .. })
        ));
        // ~600ms of auto-repeat pairs a few ms apart.
        for _ in 0..60 {
            clock += ms(5);
            assert!(state.on_input(input(mode, false), clock).is_none());
            clock += ms(5);
            assert!(state.on_input(input(mode, true), clock).is_none());
            assert!(
                state.grace_deadline().is_none(),
                "auto-repeat press must cancel the deferred release"
            );
        }
        assert!(!state.is_locked(), "no tap may be classified mid-burst");

        clock += ms(5);
        assert!(state.on_input(input(mode, false), clock).is_none());
        assert!(
            matches!(state.on_grace_expired(), Some(Effect::Stop { .. })),
            "the genuine release after a ~600ms hold must stop recording"
        );
    }

    /// Hold-or-toggle: a press remembered during the busy window is measured
    /// from the real key-down, so a hold that straddles the drain still counts
    /// as a hold when it is released shortly after recording actually starts.
    #[test]
    fn hold_or_toggle_remembered_press_measures_hold_from_real_key_down() {
        let mode = ShortcutActivation::HoldOrToggle;
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();

        // Previous session: hold, release, stop → Processing.
        state.on_input(input(mode, true), t0);
        state.on_input(input(mode, false), t0 + ms(800));
        assert!(matches!(
            state.on_grace_expired(),
            Some(Effect::Stop { .. })
        ));

        // Pressed again while busy; still held when the pipeline drains 700ms later.
        assert!(state.on_input(input(mode, true), t0 + ms(1000)).is_none());
        assert!(matches!(
            state.on_processing_finished(),
            Some(Effect::Start { .. })
        ));
        // Released 100ms after recording began - but 800ms after key-down.
        assert!(state.on_input(input(mode, false), t0 + ms(1800)).is_none());
        assert!(
            matches!(state.on_grace_expired(), Some(Effect::Stop { .. })),
            "held 800ms overall: must stop, not lock"
        );
    }

    /// Toggle: releases never stop, the next press does. (Toggle is the
    /// combined machine with the session locked from the start.)
    #[test]
    fn toggle_mode_ignores_release_and_stops_on_next_press() {
        let mode = ShortcutActivation::Toggle;
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();

        assert!(matches!(
            state.on_input(input(mode, true), t0),
            Some(Effect::Start { .. })
        ));
        assert!(state.is_locked());
        assert!(state.on_input(input(mode, false), t0 + ms(100)).is_none());
        assert!(
            state.grace_deadline().is_none(),
            "toggle never defers releases"
        );
        assert!(state.on_input(input(mode, false), t0 + ms(3000)).is_none());
        assert!(matches!(
            state.on_input(input(mode, true), t0 + ms(4000)),
            Some(Effect::Stop { .. })
        ));
    }

    /// Push-to-talk: even a very short press stops on release - there is no
    /// tap-to-lock in this mode (hold threshold of zero).
    #[test]
    fn push_to_talk_short_press_still_stops_on_release() {
        let mode = ShortcutActivation::PushToTalk;
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();

        assert!(matches!(
            state.on_input(input(mode, true), t0),
            Some(Effect::Start { .. })
        ));
        assert!(state.on_input(input(mode, false), t0 + ms(40)).is_none());
        assert!(matches!(
            state.on_grace_expired(),
            Some(Effect::Stop { .. })
        ));
    }

    /// Cancel (Escape) during a locked hold-or-toggle session resets cleanly so
    /// the next press starts a fresh recording rather than stopping a dead one.
    #[test]
    fn hold_or_toggle_cancel_clears_locked_session() {
        let mode = ShortcutActivation::HoldOrToggle;
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();

        state.on_input(input(mode, true), t0);
        state.on_input(input(mode, false), t0 + ms(100));
        assert!(state.on_grace_expired().is_none());
        assert!(state.is_locked());

        state.on_cancel(true);
        assert_eq!(state.stage, Stage::Idle);
        assert!(!state.is_locked());
        assert!(matches!(
            state.on_input(input(mode, true), t0 + ms(2000)),
            Some(Effect::Start { .. })
        ));
    }

    /// Switching to toggle while an unlocked hold recording is running must not
    /// strand it: in toggle mode a press always stops.
    #[test]
    fn toggle_press_stops_recording_started_as_hold() {
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();

        state.on_input(input(ShortcutActivation::HoldOrToggle, true), t0);
        assert!(!state.is_locked());
        assert!(matches!(
            state.on_input(input(ShortcutActivation::Toggle, true), t0 + ms(2000)),
            Some(Effect::Stop { .. })
        ));
    }

    // Hold-vs-tap classification while the previous transcription is busy.
    fn hold_or_toggle_into_processing(state: &mut CoordinatorState, t0: Instant) {
        let mode = ShortcutActivation::HoldOrToggle;
        assert!(matches!(
            state.on_input(input(mode, true), t0),
            Some(Effect::Start { .. })
        ));
        assert!(state.on_input(input(mode, false), t0 + ms(800)).is_none());
        assert!(matches!(
            state.on_grace_expired(),
            Some(Effect::Stop { .. })
        ));
        assert_eq!(state.stage, Stage::Processing);
    }

    #[test]
    fn hold_or_toggle_tap_during_processing_queues_locked_start() {
        let mode = ShortcutActivation::HoldOrToggle;
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        hold_or_toggle_into_processing(&mut state, t0);

        assert!(state.on_input(input(mode, true), t0 + ms(1000)).is_none());
        assert!(state.on_input(input(mode, false), t0 + ms(1100)).is_none());
        assert!(state.on_grace_expired().is_none());
        assert!(state.is_locked(), "a busy tap should queue a locked start");
        assert!(matches!(
            state.on_processing_finished(),
            Some(Effect::Start { .. })
        ));
        assert!(state.is_locked());
    }

    #[test]
    fn hold_or_toggle_completed_hold_during_processing_nets_noop() {
        let mode = ShortcutActivation::HoldOrToggle;
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        hold_or_toggle_into_processing(&mut state, t0);

        assert!(state.on_input(input(mode, true), t0 + ms(1000)).is_none());
        assert!(state.on_input(input(mode, false), t0 + ms(1600)).is_none());
        assert!(state.on_grace_expired().is_none());
        assert!(!state.is_locked());

        assert!(
            state.on_processing_finished().is_none(),
            "a 600ms hold that ended before the drain has nothing left to start"
        );
        assert_eq!(state.stage, Stage::Idle);
    }

    #[test]
    fn hold_or_toggle_two_taps_during_processing_net_noop() {
        let mode = ShortcutActivation::HoldOrToggle;
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        hold_or_toggle_into_processing(&mut state, t0);

        assert!(state.on_input(input(mode, true), t0 + ms(1000)).is_none());
        assert!(state.on_input(input(mode, false), t0 + ms(1100)).is_none());
        assert!(state.on_grace_expired().is_none());
        assert!(state.is_locked());

        assert!(state.on_input(input(mode, true), t0 + ms(1500)).is_none());
        assert!(
            !state.is_locked(),
            "the second tap's press forgets the queued tap"
        );
        assert!(state.on_input(input(mode, false), t0 + ms(1600)).is_none());
        assert!(state.grace_deadline().is_none());

        assert!(state.on_processing_finished().is_none());
        assert_eq!(state.stage, Stage::Idle);
    }

    #[test]
    fn ptt_tap_inside_busy_window_nets_noop() {
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        hold_or_toggle_into_processing(&mut state, t0);

        assert!(state.on_input(ptt_input(true), t0 + ms(1000)).is_none());
        assert!(state.on_input(ptt_input(false), t0 + ms(1040)).is_none());
        assert!(state.on_grace_expired().is_none());
        assert!(state.on_processing_finished().is_none());
    }

    /// The pipeline drains inside the 50ms grace of a busy tap: recording
    /// starts first (unlocked, from the real key-down), and the grace then
    /// resolves against the live recording, locking it as the tap it was.
    #[test]
    fn hold_or_toggle_drain_inside_busy_release_grace_still_classifies_tap() {
        let mode = ShortcutActivation::HoldOrToggle;
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        hold_or_toggle_into_processing(&mut state, t0);

        assert!(state.on_input(input(mode, true), t0 + ms(1000)).is_none());
        assert!(state.on_input(input(mode, false), t0 + ms(1100)).is_none());
        assert!(matches!(
            state.on_processing_finished(),
            Some(Effect::Start { .. })
        ));
        assert!(!state.is_locked());

        assert!(state.on_grace_expired().is_none());
        assert!(state.is_locked(), "the deferred 100ms release is a tap");
    }

    /// X11 auto-repeat while busy, key still held at the drain: recording
    /// starts measured from the first press, not from the last synthesized
    /// press before the drain. Released 400ms after the real key-down but
    /// only ~100ms after the drain - a hold, so it must stop rather than lock.
    #[test]
    fn hold_or_toggle_autorepeat_burst_straddling_drain_measures_from_first_press() {
        let mode = ShortcutActivation::HoldOrToggle;
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        hold_or_toggle_into_processing(&mut state, t0);

        let mut clock = t0 + ms(1000);
        assert!(state.on_input(input(mode, true), clock).is_none());
        for _ in 0..30 {
            clock += ms(5);
            assert!(state.on_input(input(mode, false), clock).is_none());
            clock += ms(5);
            assert!(state.on_input(input(mode, true), clock).is_none());
            assert!(state.grace_deadline().is_none());
        }

        // Drain at ~t0 + 1300ms with the key still down.
        assert!(matches!(
            state.on_processing_finished(),
            Some(Effect::Start { .. })
        ));
        assert!(!state.is_locked());

        for _ in 0..10 {
            clock += ms(5);
            assert!(state.on_input(input(mode, false), clock).is_none());
            clock += ms(5);
            assert!(state.on_input(input(mode, true), clock).is_none());
        }
        assert_eq!(clock, t0 + ms(1400));
        assert!(state.on_input(input(mode, false), clock).is_none());
        assert!(
            matches!(state.on_grace_expired(), Some(Effect::Stop { .. })),
            "held 400ms since the real key-down: must stop, not lock"
        );
        assert_eq!(state.stage, Stage::Processing);
    }

    // ---------------------------------------------------------------------
    // Toggle auto-restart reproduction (v1.0.0 regression report 3).
    //
    // Sequence: tap starts dictation, tap stops it, the async pipeline
    // transcribes and pastes, and a few seconds later a recording starts
    // again by itself. Original root cause, confirmed against the code
    // paths:
    //
    // 1. The paste chord is synthesized with enigo (Cmd down, V click, Cmd
    //    up after ~100ms) and posted through CGEventPost, so it re-enters
    //    the same session event tap the shortcut backend listens on.
    // 2. Upstream handy-keys 0.3.4 did not filter host-synthesized events
    //    and fired the modifier-only command binding ("command_left") on
    //    the leading Cmd press, exactly like a physical key.
    // 3. Back then transcribe_commands was a recording trigger routed into
    //    this coordinator; the stage was Processing, so the press was
    //    remembered (classify_busy_input -> Remember), the ~100ms synthetic
    //    release classified as a tap and locked it, and the drain started
    //    it: recording began again with no key touched.
    //
    // Two structural fixes since: the synthesized-event marker makes the
    // tap ignore enigo-injected events entirely, single-modifier bindings
    // are hold-gated for 400ms, and the command binding no longer routes
    // into the recording lifecycle AT ALL (it is a during-dictation
    // modifier; a press with no live session is inert). The test below
    // drives the re-entry edges through the ONLY path they can still take
    // (the command-modifier command) and pins that nothing restarts.
    // ---------------------------------------------------------------------

    /// The operator sequence with the re-entered chord delivered as the
    /// command-modifier edges (all the binding can produce now): nothing
    /// is remembered, the drain lands on idle, and no recording restarts.
    #[test]
    fn paste_chord_reentry_cannot_rearm_a_start_after_the_redesign() {
        let mode = ShortcutActivation::HoldOrToggle;
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();

        // Tap 1: starts dictation, locks it on.
        assert!(matches!(
            state.on_input(input(mode, true), t0),
            Some(Effect::Start { .. })
        ));
        assert!(state.on_input(input(mode, false), t0 + ms(120)).is_none());
        assert!(state.on_grace_expired().is_none());
        assert!(state.is_locked());

        // Tap 2: stops and finalizes -> Processing.
        assert!(matches!(
            state.on_input(input(mode, true), t0 + ms(3000)),
            Some(Effect::Stop { .. })
        ));
        assert_eq!(state.stage, Stage::Processing);

        // The paste chord re-enters the tap ~1.2s later (preview delay) or
        // right after transcription: command down, V click, command up
        // ~100ms later. Even if both edges reached the coordinator they
        // are command-modifier presses with no live session: inert.
        state.on_command_modifier(true, t0 + ms(4200));
        state.on_command_modifier(false, t0 + ms(4300));
        assert!(
            !state.command_modifier,
            "a modifier press with no live session must not engage"
        );

        // The pipeline drains: nothing remembered, nothing restarts.
        assert!(
            state.on_processing_finished().is_none(),
            "the drain must land on idle; the chord edges carry no lifecycle weight"
        );
        assert_eq!(state.stage, Stage::Idle);
    }

    /// The same operator sequence with the fix in place: the synthesized
    /// chord never reaches the coordinator (filtered at the tap, and
    /// hold-gated even if it did), so the drain lands on idle and nothing
    /// restarts.
    #[test]
    fn clean_drain_after_toggle_stop_does_not_restart() {
        let mode = ShortcutActivation::HoldOrToggle;
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();

        assert!(matches!(
            state.on_input(input(mode, true), t0),
            Some(Effect::Start { .. })
        ));
        assert!(state.on_input(input(mode, false), t0 + ms(120)).is_none());
        assert!(state.on_grace_expired().is_none());

        assert!(matches!(
            state.on_input(input(mode, true), t0 + ms(3000)),
            Some(Effect::Stop { .. })
        ));

        // Seconds pass: transcription, preview, paste. No coordinator
        // inputs arrive, because the chord is filtered at the tap.
        assert!(
            state.on_processing_finished().is_none(),
            "nothing remembered: the drain must land on idle"
        );
        assert_eq!(state.stage, Stage::Idle);
    }
}
