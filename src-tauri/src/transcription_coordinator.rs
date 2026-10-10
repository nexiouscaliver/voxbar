use crate::actions::ACTION_MAP;
use crate::managers::audio::AudioRecordingManager;
use crate::settings::ShortcutActivation;
use log::{debug, error, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};

const DEBOUNCE: Duration = Duration::from_millis(30);
const RELEASE_GRACE: Duration = Duration::from_millis(50);

/// The binding id of the phone/tablet companion trigger. Every companion
/// edge (live presses from the phone, synthesized finalize edges from the
/// server) routes the lifecycle through this one id.
const COMPANION_BINDING_ID: &str = "transcribe_companion";

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
    /// outlive the session that armed it.
    CommandModifierChanged { active: bool },
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
        // Cancel ends the session: the command modifier cannot outlive it.
        self.set_command_modifier(false);
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
        let pending = self.pending_press.take()?;
        debug!(
            "Pipeline drained; starting remembered press for '{}'",
            pending.binding_id
        );
        Some(self.begin_recording(
            pending.binding_id,
            pending.hotkey_string,
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
            // The session never existed; the modifier cannot stay armed.
            self.set_command_modifier(false);
        }
    }

    /// Optimistic transition to `Recording`; rolled back via
    /// [`CoordinatorState::on_start_result`] if the effect fails to start
    /// recording for real.
    fn begin_recording(
        &mut self,
        binding_id: String,
        hotkey_string: String,
        pressed_at: Instant,
        locked: bool,
    ) -> Effect {
        self.stage = Stage::Recording(binding_id.clone());
        self.hold = Some(Hold { pressed_at, locked });
        Effect::Start {
            binding_id,
            hotkey_string,
        }
    }

    fn begin_processing(&mut self, binding_id: String, hotkey_string: String) -> Effect {
        self.stage = Stage::Processing;
        self.hold = None;
        // The recording ended: the in-session command modifier goes with it,
        // even if the key is still physically held. Re-engaging requires a
        // fresh press during the next live session. This is the session-end
        // badge clear: the notification rides the drain below.
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
    fn on_command_modifier(&mut self, is_pressed: bool) -> Option<Effect> {
        if is_pressed {
            if matches!(self.stage, Stage::Recording(_)) {
                debug!("Command modifier engaged for the live dictation session");
                self.set_command_modifier(true);
                None
            } else {
                debug!("Command modifier pressed with no live dictation session; nothing happens");
                Some(Effect::NotifyCommandIdle)
            }
        } else if self.command_modifier {
            debug!("Command modifier released; dictation returns to normal");
            self.set_command_modifier(false);
            None
        } else {
            None
        }
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
                        } => state.on_cancel(recording_was_active),
                        Command::ProcessingFinished => {
                            if let Some(effect) = state.on_processing_finished() {
                                run_effect(&app, &mut state, effect);
                            }
                        }
                        Command::CommandModifier { is_pressed } => {
                            if let Some(effect) = state.on_command_modifier(is_pressed) {
                                run_effect(&app, &mut state, effect);
                            }
                        }
                        Command::FinalizeCompanion => {
                            if let Some(effect) = state.on_finalize_companion() {
                                run_effect(&app, &mut state, effect);
                            }
                        }
                    }
                    // Badge notifications the processed command enqueued
                    // (modifier engage/release and every session-end clear).
                    for effect in state.drain_modifier_notifications() {
                        run_effect(&app, &mut state, effect);
                    }
                    publish_state(&state);
                }
                // The coordinator thread is gone (app shutdown); stop
                // advertising a live recording session, an engaged command
                // modifier, or a remembered press.
                recording_mirror.store(false, Ordering::Release);
                processing_mirror.store(false, Ordering::Release);
                command_modifier_mirror.store(false, Ordering::Release);
                pending_press_mirror.store(false, Ordering::Release);
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
        );
    }

    /// Forward a companion-device (phone/tablet) press or release edge for
    /// the "transcribe_companion" binding. External like the signal/CLI
    /// triggers (network edges must never be debounced - dropping one
    /// desyncs the phone's button state), but honoring the user's activation
    /// mode and hold threshold so hold-to-talk and tap-to-lock behave from
    /// the phone exactly as they do from the keyboard.
    pub fn send_companion_edge(&self, app: &AppHandle, pressed: bool) {
        let settings = crate::settings::get_settings(app);
        self.send(
            COMPANION_BINDING_ID,
            "companion",
            pressed,
            settings.shortcut_activation,
            Duration::from_millis(settings.hold_threshold_ms),
            true,
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

    fn send(
        &self,
        binding_id: &str,
        hotkey_string: &str,
        is_pressed: bool,
        mode: ShortcutActivation,
        hold_threshold: Duration,
        external: bool,
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

fn run_effect(app: &AppHandle, state: &mut CoordinatorState, effect: Effect) {
    match effect {
        Effect::Start {
            binding_id,
            hotkey_string,
        } => {
            let started = start(app, &binding_id, &hotkey_string);
            state.on_start_result(&binding_id, started);
        }
        Effect::Stop {
            binding_id,
            hotkey_string,
        } => stop(app, &binding_id, &hotkey_string),
        Effect::NotifyCommandIdle => {
            use tauri::Emitter;
            if let Err(e) = app.emit("command-mode-no-session", ()) {
                warn!("Failed to emit command-mode-no-session: {e}");
            }
        }
        Effect::NotifyRecordingBusy { binding_id } => {
            crate::managers::transcription::emit_overlay_notice(
                app,
                crate::managers::transcription::NoticeCode::BindingBusy,
                Some(binding_id),
            );
        }
        Effect::CommandModifierChanged { active } => {
            use tauri::Emitter;
            if let Err(e) = app.emit_to("recording_overlay", "command-modifier-changed", active) {
                warn!("Failed to emit command-modifier-changed: {e}");
            }
        }
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
        InputEvent {
            binding_id: COMPANION_BINDING.to_string(),
            hotkey_string: "companion".to_string(),
            is_pressed: pressed,
            mode,
            hold_threshold: Duration::from_millis(300),
            external: true,
        }
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
        // gets the same arbitration.
        let mut state = CoordinatorState::new();
        assert!(matches!(
            state.on_input(companion_edge(true, ShortcutActivation::Toggle), t0),
            Some(Effect::Start { .. })
        ));
        let effect = state.on_input(
            toggle_input_for(OTHER_BINDING, true),
            t0 + Duration::from_millis(50),
        );
        assert!(matches!(
            effect,
            Some(Effect::NotifyRecordingBusy { binding_id }) if binding_id == OTHER_BINDING
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

        assert_eq!(state.on_command_modifier(true), None);
        assert!(state.command_modifier);
    }

    #[test]
    fn command_modifier_release_returns_to_normal() {
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        state.on_input(toggle_input(true), t0);
        assert_eq!(state.on_command_modifier(true), None);
        assert!(state.command_modifier);

        assert_eq!(state.on_command_modifier(false), None);
        assert!(!state.command_modifier);
        // The session itself is untouched by the modifier.
        assert_eq!(state.stage, Stage::Recording(BINDING.to_string()));
    }

    #[test]
    fn command_modifier_cleared_when_session_ends_while_held() {
        let mut state = CoordinatorState::new();
        let t0 = Instant::now();
        state.on_input(toggle_input(true), t0);
        assert_eq!(state.on_command_modifier(true), None);
        assert!(state.command_modifier);

        // The dictation finalizes while the modifier is still held: the
        // modifier is cleared cleanly, and a later release stays inert.
        assert!(matches!(
            state.on_input(toggle_input(true), t0 + Duration::from_secs(5)),
            Some(Effect::Stop { .. })
        ));
        assert!(!state.command_modifier);
        assert_eq!(state.on_command_modifier(false), None);
        assert!(!state.command_modifier);
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
        assert_eq!(state.on_command_modifier(true), None);
        assert_eq!(
            state.drain_modifier_notifications(),
            vec![Effect::CommandModifierChanged { active: true }]
        );
        assert!(state.drain_modifier_notifications().is_empty(), "drained");

        // Release: one badge-off notification.
        assert_eq!(state.on_command_modifier(false), None);
        assert_eq!(
            state.drain_modifier_notifications(),
            vec![Effect::CommandModifierChanged { active: false }]
        );

        // A redundant release (modifier already off) stays silent.
        assert_eq!(state.on_command_modifier(false), None);
        assert!(state.drain_modifier_notifications().is_empty());

        // An idle press (session over) notifies idle, never the badge.
        assert!(matches!(
            state.on_input(toggle_input(true), t0 + Duration::from_secs(5)),
            Some(Effect::Stop { .. })
        ));
        assert!(matches!(
            state.on_command_modifier(true),
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
        assert_eq!(state.on_command_modifier(true), None);
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
        assert_eq!(cancelled.on_command_modifier(true), None);
        cancelled.drain_modifier_notifications();
        cancelled.on_cancel(true);
        assert_eq!(
            cancelled.drain_modifier_notifications(),
            vec![Effect::CommandModifierChanged { active: false }]
        );
    }

    /// A press for a different transcribe binding while one is recording is
    /// swallowed to protect the live session, but no longer silently: it
    /// returns the busy notice effect and leaves the session untouched.
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
                toggle_input_for("transcribe_with_post_process", false),
                t0 + Duration::from_millis(50)
            ),
            Some(Effect::NotifyRecordingBusy {
                binding_id: "transcribe_with_post_process".to_string()
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

    #[test]
    fn command_modifier_press_with_no_live_session_notifies() {
        // Idle: the state stays untouched, but the press is not silent -
        // it asks for the command-mode-no-session feedback.
        let mut state = CoordinatorState::new();
        assert!(matches!(
            state.on_command_modifier(true),
            Some(Effect::NotifyCommandIdle)
        ));
        assert!(!state.command_modifier);
        assert_eq!(state.stage, Stage::Idle);

        // Busy pipeline: equally inert for interpretation (the modifier
        // only modulates a LIVE session, never queues for one), with the
        // same feedback.
        let mut busy = CoordinatorState::new();
        let t0 = Instant::now();
        drive_into_processing(&mut busy, t0);
        assert!(matches!(
            busy.on_command_modifier(true),
            Some(Effect::NotifyCommandIdle)
        ));
        assert!(!busy.command_modifier);
        assert!(busy.on_processing_finished().is_none());
        assert_eq!(busy.stage, Stage::Idle);

        // Cancel path: a session cancelled while held also clears it.
        let mut cancelled = CoordinatorState::new();
        cancelled.on_input(toggle_input(true), t0);
        assert_eq!(cancelled.on_command_modifier(true), None);
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
        state.on_command_modifier(true);
        state.on_command_modifier(false);
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
