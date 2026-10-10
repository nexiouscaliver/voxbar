//! Platform-agnostic hotkey manager built on top of KeyboardListener

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::listener::{BlockingHotkeys, KeyboardListener};
use crate::types::{Hotkey, HotkeyEvent, HotkeyId, HotkeyState, KeyEvent, Modifiers};

/// A single-modifier hotkey press waiting out its hold-to-activate window.
struct PendingHold {
    id: HotkeyId,
    /// The exact side bit that was pressed (e.g. `CMD_RIGHT`), so
    /// auto-repeat of the same modifier key keeps the hold alive while
    /// every other key event cancels it.
    side_bit: Modifiers,
    pressed_at: Instant,
    /// This hold's own window: the hotkey's per-binding override when it
    /// has one, otherwise the manager-wide default. Carried per pending
    /// hold (not read back from the maps) so a later policy change can
    /// never retroactively stretch or shrink a window already running.
    threshold: Duration,
}

/// What an incoming key event means for a pending hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HoldArbitration {
    /// An auto-repeat press of the very modifier being held: the window
    /// keeps running (Windows/Linux emit repeats for held modifier keys;
    /// macOS does not).
    Keep,
    /// The held modifier itself was released before the window elapsed:
    /// the activation simply never happens.
    ReleaseEarly,
    /// Any other key event: the user is using the modifier in a chord, so
    /// the standalone activation is cancelled.
    Cancel,
}

/// Pure decision: how does `event` affect a pending hold started by
/// `side_bit` going down?
fn arbitrate_hold(side_bit: Modifiers, event: &KeyEvent) -> HoldArbitration {
    match event.changed_modifier {
        Some(bit) if bit == side_bit => {
            if event.is_key_down {
                HoldArbitration::Keep
            } else {
                HoldArbitration::ReleaseEarly
            }
        }
        _ => HoldArbitration::Cancel,
    }
}

/// Internal state shared between the manager and the processing thread
struct ManagerState {
    hotkeys: HashMap<HotkeyId, Hotkey>,
    next_id: u32,
    /// Track which hotkeys are currently pressed
    pressed_hotkeys: HashSet<HotkeyId>,
    /// Opt-in hold-to-activate window for single-modifier hotkeys
    /// (see [`HotkeyManager::with_single_modifier_hold`]). The default
    /// every hotkey falls back to; individual hotkeys may carry their own
    /// window in `hold_overrides`.
    single_modifier_hold: Option<Duration>,
    /// Per-hotkey hold windows stamped at registration
    /// (see [`HotkeyManager::register_with_hold`]). A hotkey present here
    /// is hold-gated with ITS window even when the manager-wide policy is
    /// disabled; the gate itself still only applies to single-modifier
    /// hotkeys.
    hold_overrides: HashMap<HotkeyId, Duration>,
    /// Single-modifier presses waiting out that window.
    pending_holds: Vec<PendingHold>,
}

impl ManagerState {
    fn new() -> Self {
        Self {
            hotkeys: HashMap::new(),
            next_id: 0,
            pressed_hotkeys: HashSet::new(),
            single_modifier_hold: None,
            hold_overrides: HashMap::new(),
            pending_holds: Vec::new(),
        }
    }

    /// The hold window that gates `id`: its per-binding override when it
    /// has one, otherwise the manager-wide default. `None` means the
    /// hotkey is not hold-gated.
    fn hold_threshold(&self, id: &HotkeyId) -> Option<Duration> {
        self.hold_overrides
            .get(id)
            .copied()
            .or(self.single_modifier_hold)
    }

    /// Whether the hotkey registered under `id` is currently hold-gated.
    fn hold_gated(&self, id: &HotkeyId) -> bool {
        self.hold_threshold(id).is_some()
            && self
                .hotkeys
                .get(id)
                .is_some_and(|hotkey| hotkey.is_single_modifier())
    }

    /// Register `hotkey` under the manager-wide hold policy (`hold` =
    /// `None`) or a per-binding window (`hold` = `Some`). Shared core of
    /// [`HotkeyManager::register`] and [`HotkeyManager::register_with_hold`].
    fn register_hotkey(&mut self, hotkey: Hotkey, hold: Option<Duration>) -> HotkeyId {
        let id = HotkeyId(self.next_id);
        self.next_id += 1;
        self.hotkeys.insert(id, hotkey);
        if let Some(threshold) = hold {
            self.hold_overrides.insert(id, threshold);
        }
        id
    }

    /// Unregister `hotkey` by ID: drops the hotkey, its per-binding hold
    /// override, any pending hold, and its pressed bookkeeping. Returns
    /// the hotkey so the caller can update the blocking set. Shared core
    /// of [`HotkeyManager::unregister`].
    fn unregister_hotkey(&mut self, id: &HotkeyId) -> Option<Hotkey> {
        let hotkey = self.hotkeys.remove(id)?;
        // A pending hold for an unregistered hotkey must not activate later.
        self.pending_holds.retain(|pending| pending.id != *id);
        self.pressed_hotkeys.remove(id);
        // The override belongs to the registration, not the chord: drop it
        // so a later registration of the same chord starts from the
        // manager-wide policy again.
        self.hold_overrides.remove(id);
        Some(hotkey)
    }

    /// Time until the next pending hold's window elapses, so the event
    /// loop can poll promptly instead of waiting for the next key event.
    fn next_wake(&self) -> Option<Duration> {
        self.pending_holds
            .iter()
            .map(|pending| {
                pending
                    .threshold
                    .saturating_sub(pending.pressed_at.elapsed())
            })
            .min()
    }

    /// Process a key event and return any matching hotkey events
    fn process_event(&mut self, event: &KeyEvent) -> Vec<HotkeyEvent> {
        self.process_event_at(event, Instant::now())
    }

    /// Clock-injectable core of [`ManagerState::process_event`].
    fn process_event_at(&mut self, event: &KeyEvent, now: Instant) -> Vec<HotkeyEvent> {
        let mut results = Vec::new();

        // Arbitrate pending holds against every incoming event: anything
        // that is not an auto-repeat of the held modifier itself cancels
        // (chord usage) or resolves (early release) the pending
        // activation. Canceled holds never activated, so they emit
        // nothing.
        if !self.pending_holds.is_empty() {
            self.pending_holds
                .retain(|pending| arbitrate_hold(pending.side_bit, event) == HoldArbitration::Keep);
        }

        if event.is_key_down {
            // Check for hotkeys that should be pressed
            let to_press: Vec<HotkeyId> = self
                .hotkeys
                .iter()
                .filter(|(&id, hotkey)| {
                    hotkey.modifiers.matches(event.modifiers)
                        && hotkey.key == event.key
                        && !self.pressed_hotkeys.contains(&id)
                })
                .map(|(&id, _)| id)
                .collect();

            for id in to_press {
                let hold_gated = self.hold_gated(&id) && event.changed_modifier.is_some();
                if hold_gated {
                    // Defer activation behind the hold window. Auto-repeat
                    // re-presses keep the earliest press time, so the
                    // window is measured from the first physical press.
                    if !self.pending_holds.iter().any(|pending| pending.id == id) {
                        // A hold-gated hotkey has no key, so the matching
                        // event is a modifier-only event whose
                        // changed_modifier is the side actually pressed.
                        // The window snapshot is taken now: this hotkey's
                        // override when it has one, else the manager-wide
                        // default (hold_gated guarantees one exists).
                        self.pending_holds.push(PendingHold {
                            id,
                            side_bit: event.changed_modifier.unwrap(),
                            pressed_at: now,
                            threshold: self
                                .hold_threshold(&id)
                                .expect("hold-gated implies a threshold"),
                        });
                    }
                } else {
                    // Either the hold window is disabled, or this is not a
                    // single-modifier hotkey (or a degenerate event without
                    // a changed modifier): press immediately, as upstream
                    // 0.3.4 always did.
                    self.pressed_hotkeys.insert(id);
                    results.push(HotkeyEvent {
                        id,
                        state: HotkeyState::Pressed,
                    });
                }
            }
        } else {
            // Check for hotkeys that should be released
            // A hotkey is released when its key is released, or — for modifier
            // events — when the modifiers no longer match. A modifier event
            // (key == None) whose modifiers still match must not release a
            // modifier-only hotkey (e.g. tapping Shift while a Cmd-only hotkey
            // is held).
            let to_release: Vec<HotkeyId> = self
                .hotkeys
                .iter()
                .filter(|(&id, hotkey)| {
                    self.pressed_hotkeys.contains(&id)
                        && ((event.key.is_some() && hotkey.key == event.key)
                            || (event.key.is_none() && !hotkey.modifiers.matches(event.modifiers)))
                })
                .map(|(&id, _)| id)
                .collect();

            for id in to_release {
                self.pressed_hotkeys.remove(&id);
                results.push(HotkeyEvent {
                    id,
                    state: HotkeyState::Released,
                });
            }
        }

        results
    }

    /// Resolve pending holds whose window has elapsed at `now`: each
    /// survivor activates (emits `Pressed`). Called after every processed
    /// event and whenever the shortened poll times out. Every pending
    /// carries its own window (per-binding override or manager default),
    /// so a short override resolves beside a long default in one pass.
    fn process_tick(&mut self, now: Instant) -> Vec<HotkeyEvent> {
        if self.pending_holds.is_empty() {
            return Vec::new();
        }
        let mut results = Vec::new();
        let mut index = 0;
        while index < self.pending_holds.len() {
            let pending = &self.pending_holds[index];
            if now.duration_since(pending.pressed_at) >= pending.threshold {
                let pending = self.pending_holds.remove(index);
                // The hotkey may have been unregistered while pending, or
                // already activated by a disable-flush.
                if self.hotkeys.contains_key(&pending.id)
                    && !self.pressed_hotkeys.contains(&pending.id)
                {
                    self.pressed_hotkeys.insert(pending.id);
                    results.push(HotkeyEvent {
                        id: pending.id,
                        state: HotkeyState::Pressed,
                    });
                }
            } else {
                index += 1;
            }
        }
        results
    }
}

/// Platform-agnostic Hotkey Manager
///
/// This manager wraps a `KeyboardListener` and filters events against
/// registered hotkeys, emitting `HotkeyEvent`s when matches occur.
///
/// Registered hotkeys are blocked from reaching other applications.
/// Note: On Linux, blocking requires write access to `/dev/uinput`
/// (non-blocked keystrokes are re-injected through it).
pub struct HotkeyManager {
    state: Arc<Mutex<ManagerState>>,
    event_receiver: Receiver<HotkeyEvent>,
    _thread_handle: Option<JoinHandle<()>>,
    running: Arc<std::sync::atomic::AtomicBool>,
    /// Shared set of hotkeys to block
    blocking_hotkeys: Option<BlockingHotkeys>,
}

impl HotkeyManager {
    /// Create a new HotkeyManager (non-blocking mode)
    ///
    /// On macOS, this will check for accessibility permissions and fail if not granted.
    pub fn new() -> Result<Self> {
        let listener = KeyboardListener::new()?;

        let (tx, rx) = mpsc::channel();
        let state = Arc::new(Mutex::new(ManagerState::new()));
        let running = Arc::new(std::sync::atomic::AtomicBool::new(true));

        let thread_state = Arc::clone(&state);
        let thread_running = Arc::clone(&running);

        let handle = thread::spawn(move || {
            Self::event_loop(listener, thread_state, tx, thread_running);
        });

        Ok(Self {
            state,
            event_receiver: rx,
            _thread_handle: Some(handle),
            running,
            blocking_hotkeys: None,
        })
    }

    /// Create a new HotkeyManager with blocking support
    ///
    /// On macOS, this will check for accessibility permissions and fail if not granted.
    /// Registered hotkeys will be blocked from reaching other applications.
    ///
    /// Note: On Linux, blocking requires write access to `/dev/uinput`
    /// and this fails with an actionable error without it.
    pub fn new_with_blocking() -> Result<Self> {
        let blocking_hotkeys: BlockingHotkeys = Arc::new(Mutex::new(HashSet::new()));
        let listener = KeyboardListener::new_with_blocking(blocking_hotkeys.clone())?;

        let (tx, rx) = mpsc::channel();
        let state = Arc::new(Mutex::new(ManagerState::new()));
        let running = Arc::new(std::sync::atomic::AtomicBool::new(true));

        let thread_state = Arc::clone(&state);
        let thread_running = Arc::clone(&running);

        let handle = thread::spawn(move || {
            Self::event_loop(listener, thread_state, tx, thread_running);
        });

        Ok(Self {
            state,
            event_receiver: rx,
            _thread_handle: Some(handle),
            running,
            blocking_hotkeys: Some(blocking_hotkeys),
        })
    }

    /// Event processing loop
    fn event_loop(
        listener: KeyboardListener,
        state: Arc<Mutex<ManagerState>>,
        sender: Sender<HotkeyEvent>,
        running: Arc<std::sync::atomic::AtomicBool>,
    ) {
        const RECV_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);

        while running.load(std::sync::atomic::Ordering::SeqCst) {
            // While a hold window is running out, shorten the poll so the
            // activation fires promptly instead of waiting for the next
            // key event.
            let timeout = state
                .lock()
                .ok()
                .and_then(|state| state.next_wake())
                .map(|wake| wake.min(RECV_TIMEOUT))
                .unwrap_or(RECV_TIMEOUT);

            match listener.recv_timeout(timeout) {
                Ok(key_event) => {
                    if let Ok(mut state) = state.lock() {
                        let mut hotkey_events = state.process_event(&key_event);
                        hotkey_events.extend(state.process_tick(Instant::now()));
                        for event in hotkey_events {
                            if sender.send(event).is_err() {
                                // Receiver dropped, exit
                                return;
                            }
                        }
                    }
                }
                Err(crate::error::Error::Timeout) => {
                    // The poll window elapsed: resolve any hold whose
                    // activation time has come.
                    if let Ok(mut state) = state.lock() {
                        for event in state.process_tick(Instant::now()) {
                            if sender.send(event).is_err() {
                                return;
                            }
                        }
                    }
                }
                Err(_) => {
                    // Listener disconnected, exit
                    return;
                }
            }
        }
    }

    /// Enable hold-to-activate for single-modifier hotkeys
    /// ([`Hotkey::is_single_modifier`]): such a hotkey only fires after its
    /// modifier has been held for `threshold` with no other key event in
    /// between. Chord usage (Cmd+C and friends) releases far faster than
    /// the threshold and can never trigger, and any other key event during
    /// the window cancels the pending activation.
    ///
    /// This is the manager-wide DEFAULT window. An individual hotkey can
    /// carry its own window at registration time via
    /// [`HotkeyManager::register_with_hold`]; the per-binding window wins
    /// for that hotkey (and gates it even when this manager-wide policy is
    /// disabled).
    ///
    /// Hold-gated hotkeys are never *blocked* from the OS: swallowing the
    /// modifier at press time (before the hold resolves) would break every
    /// normal chord built on that modifier.
    ///
    /// Multi-modifier and keyed combos are unaffected and keep firing on
    /// the press, as before.
    pub fn with_single_modifier_hold(self, threshold: Duration) -> Self {
        self.set_single_modifier_hold(Some(threshold));
        self
    }

    /// Disable (or re-enable) the manager-wide hold-to-activate window.
    /// Disabling drops any pending activations (they never fired, so
    /// nothing is released). Per-binding overrides registered through
    /// [`HotkeyManager::register_with_hold`] survive: they never depended
    /// on the manager-wide policy.
    pub fn set_single_modifier_hold(&self, threshold: Option<Duration>) {
        if let Ok(mut state) = self.state.lock() {
            state.single_modifier_hold = threshold;
            if threshold.is_none() {
                state.pending_holds.clear();
            }
        }
    }

    /// Register a hotkey under the manager-wide hold policy and return its
    /// unique ID.
    ///
    /// Returns an error if the hotkey is already registered.
    pub fn register(&self, hotkey: Hotkey) -> Result<HotkeyId> {
        self.register_with_hold(hotkey, None)
    }

    /// Register a hotkey with a PER-BINDING hold-to-activate window and
    /// return its unique ID. `Some(threshold)` stamps this one hotkey with
    /// its own window, overriding (and independent of) the manager-wide
    /// [`HotkeyManager::with_single_modifier_hold`] default; `None` is
    /// exactly [`HotkeyManager::register`].
    ///
    /// The override only means anything for a single-modifier hotkey
    /// ([`Hotkey::is_single_modifier`]): a keyed or multi-modifier combo
    /// accepts and stores it but still fires on the press, as it always
    /// did. Cancel-on-any-other-key applies inside the window exactly as
    /// with the default, so chords built on the bound modifier still
    /// cannot fire it.
    ///
    /// Returns an error if the hotkey is already registered.
    pub fn register_with_hold(&self, hotkey: Hotkey, hold: Option<Duration>) -> Result<HotkeyId> {
        let mut state = self.state.lock().map_err(|_| Error::MutexPoisoned)?;

        // Check if already registered
        for (id, existing) in &state.hotkeys {
            if existing == &hotkey {
                return Err(Error::HotkeyAlreadyRegistered(format!(
                    "{} (id: {:?})",
                    hotkey, id
                )));
            }
        }

        let id = state.register_hotkey(hotkey, hold);

        // Add to blocking set. Hold-gated single-modifier hotkeys are
        // deliberately excluded: their modifier key must keep reaching the
        // OS while the hold window runs, or every chord built on that
        // modifier (Cmd+C, Shift+Tab, ...) would lose its modifier. The
        // per-binding override counts: an overridden hotkey is hold-gated
        // even without the manager-wide policy.
        if let Some(blocking_hotkeys) = &self.blocking_hotkeys {
            if !state.hold_gated(&id) {
                if let Ok(mut blocking) = blocking_hotkeys.lock() {
                    blocking.insert(hotkey);
                }
            }
        }

        Ok(id)
    }

    /// Unregister a hotkey by its ID
    ///
    /// Returns an error if the hotkey ID is not found.
    pub fn unregister(&self, id: HotkeyId) -> Result<()> {
        let mut state = self.state.lock().map_err(|_| Error::MutexPoisoned)?;

        // Drops the hotkey, any pending hold for it, its pressed
        // bookkeeping, and its per-binding hold override in one pass.
        let hotkey = state.unregister_hotkey(&id);
        if hotkey.is_none() {
            return Err(Error::HotkeyNotFound(id));
        }

        // Remove from blocking set
        if let Some(blocking_hotkeys) = &self.blocking_hotkeys {
            if let Some(hotkey) = hotkey {
                if let Ok(mut blocking) = blocking_hotkeys.lock() {
                    blocking.remove(&hotkey);
                }
            }
        }

        Ok(())
    }

    /// Get the hotkey definition associated with an ID
    ///
    /// Returns `None` if the ID is not found.
    pub fn get_hotkey(&self, id: HotkeyId) -> Option<Hotkey> {
        let state = self.state.lock().ok()?;
        state.hotkeys.get(&id).copied()
    }

    /// Blocking receive for hotkey events
    ///
    /// Blocks until a hotkey event is received or the event loop stops.
    pub fn recv(&self) -> Result<HotkeyEvent> {
        self.event_receiver
            .recv()
            .map_err(|_| Error::EventLoopNotRunning)
    }

    /// Non-blocking receive for hotkey events
    ///
    /// Returns `Some(event)` if an event is available, `None` otherwise.
    pub fn try_recv(&self) -> Option<HotkeyEvent> {
        match self.event_receiver.try_recv() {
            Ok(event) => Some(event),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => None,
        }
    }

    /// Get the number of currently registered hotkeys
    pub fn hotkey_count(&self) -> usize {
        let state = if let Ok(s) = self.state.lock() {
            s
        } else {
            return 0;
        };
        state.hotkeys.len()
    }
}

impl Drop for HotkeyManager {
    fn drop(&mut self) {
        self.running
            .store(false, std::sync::atomic::Ordering::SeqCst);
        // Join the thread to ensure clean shutdown
        if let Some(handle) = self._thread_handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Key, Modifiers};

    fn make_key_event(modifiers: Modifiers, key: Option<Key>, is_key_down: bool) -> KeyEvent {
        KeyEvent {
            modifiers,
            key,
            is_key_down,
            changed_modifier: None,
        }
    }

    fn make_modifier_event(
        modifiers: Modifiers,
        is_key_down: bool,
        changed: Modifiers,
    ) -> KeyEvent {
        KeyEvent {
            modifiers,
            key: None,
            is_key_down,
            changed_modifier: Some(changed),
        }
    }

    mod manager_state {
        use super::*;

        #[test]
        fn register_and_lookup_hotkey() {
            let mut state = ManagerState::new();
            let hotkey = Hotkey::new(Modifiers::CMD, Key::K).unwrap();

            let id = HotkeyId(state.next_id);
            state.next_id += 1;
            state.hotkeys.insert(id, hotkey);

            assert_eq!(state.hotkeys.get(&id), Some(&hotkey));
            assert_eq!(state.hotkeys.len(), 1);
        }

        #[test]
        fn hotkey_press_generates_event() {
            let mut state = ManagerState::new();
            let hotkey = Hotkey::new(Modifiers::CMD, Key::K).unwrap();
            let id = HotkeyId(0);
            state.hotkeys.insert(id, hotkey);

            // Simulate Cmd+K key down (event uses side-specific modifier)
            let event = make_key_event(Modifiers::CMD_LEFT, Some(Key::K), true);
            let results = state.process_event(&event);

            assert_eq!(results.len(), 1);
            assert_eq!(results[0].id, id);
            assert_eq!(results[0].state, HotkeyState::Pressed);
            assert!(state.pressed_hotkeys.contains(&id));
        }

        #[test]
        fn hotkey_release_generates_event() {
            let mut state = ManagerState::new();
            let hotkey = Hotkey::new(Modifiers::CMD, Key::K).unwrap();
            let id = HotkeyId(0);
            state.hotkeys.insert(id, hotkey);

            // Press first
            let event = make_key_event(Modifiers::CMD_LEFT, Some(Key::K), true);
            state.process_event(&event);

            // Then release the key
            let event = make_key_event(Modifiers::CMD_LEFT, Some(Key::K), false);
            let results = state.process_event(&event);

            assert_eq!(results.len(), 1);
            assert_eq!(results[0].id, id);
            assert_eq!(results[0].state, HotkeyState::Released);
            assert!(!state.pressed_hotkeys.contains(&id));
        }

        #[test]
        fn no_duplicate_press_events() {
            let mut state = ManagerState::new();
            let hotkey = Hotkey::new(Modifiers::CMD, Key::K).unwrap();
            let id = HotkeyId(0);
            state.hotkeys.insert(id, hotkey);

            // Press once
            let event = make_key_event(Modifiers::CMD_LEFT, Some(Key::K), true);
            let results = state.process_event(&event);
            assert_eq!(results.len(), 1);

            // Press again (key repeat) - should not generate another event
            let results = state.process_event(&event);
            assert_eq!(results.len(), 0);
        }

        #[test]
        fn modifier_release_triggers_hotkey_release() {
            let mut state = ManagerState::new();
            let hotkey = Hotkey::new(Modifiers::CMD, Key::K).unwrap();
            let id = HotkeyId(0);
            state.hotkeys.insert(id, hotkey);

            // Press Cmd+K
            let event = make_key_event(Modifiers::CMD_LEFT, Some(Key::K), true);
            state.process_event(&event);
            assert!(state.pressed_hotkeys.contains(&id));

            // Release Cmd (while K is still held) - modifier event
            let event = make_modifier_event(Modifiers::empty(), false, Modifiers::CMD_LEFT);
            let results = state.process_event(&event);

            assert_eq!(results.len(), 1);
            assert_eq!(results[0].state, HotkeyState::Released);
            assert!(!state.pressed_hotkeys.contains(&id));
        }

        #[test]
        fn wrong_modifiers_dont_trigger() {
            let mut state = ManagerState::new();
            let hotkey = Hotkey::new(Modifiers::CMD, Key::K).unwrap();
            state.hotkeys.insert(HotkeyId(0), hotkey);

            // Press Shift+K instead of Cmd+K
            let event = make_key_event(Modifiers::SHIFT_LEFT, Some(Key::K), true);
            let results = state.process_event(&event);

            assert_eq!(results.len(), 0);
        }

        #[test]
        fn modifier_only_hotkey() {
            let mut state = ManagerState::new();
            let hotkey = Hotkey::new(Modifiers::CMD | Modifiers::SHIFT, None).unwrap();
            let id = HotkeyId(0);
            state.hotkeys.insert(id, hotkey);

            // Press Cmd+Shift (no key) — events use side-specific modifiers
            let event = make_modifier_event(
                Modifiers::CMD_LEFT | Modifiers::SHIFT_LEFT,
                true,
                Modifiers::SHIFT_LEFT,
            );
            let results = state.process_event(&event);

            assert_eq!(results.len(), 1);
            assert_eq!(results[0].state, HotkeyState::Pressed);
        }

        #[test]
        fn multiple_hotkeys_same_key() {
            let mut state = ManagerState::new();

            // Cmd+K and Ctrl+K
            let hotkey1 = Hotkey::new(Modifiers::CMD, Key::K).unwrap();
            let hotkey2 = Hotkey::new(Modifiers::CTRL, Key::K).unwrap();
            let id1 = HotkeyId(0);
            let id2 = HotkeyId(1);
            state.hotkeys.insert(id1, hotkey1);
            state.hotkeys.insert(id2, hotkey2);

            // Press Cmd+K
            let event = make_key_event(Modifiers::CMD_LEFT, Some(Key::K), true);
            let results = state.process_event(&event);

            assert_eq!(results.len(), 1);
            assert_eq!(results[0].id, id1);

            // Press Ctrl+K (release Cmd first)
            state.pressed_hotkeys.clear();
            let event = make_key_event(Modifiers::CTRL_LEFT, Some(Key::K), true);
            let results = state.process_event(&event);

            assert_eq!(results.len(), 1);
            assert_eq!(results[0].id, id2);
        }

        #[test]
        fn key_only_hotkey() {
            let mut state = ManagerState::new();
            let hotkey = Hotkey::new(Modifiers::empty(), Key::F1).unwrap();
            let id = HotkeyId(0);
            state.hotkeys.insert(id, hotkey);

            // Press F1 with no modifiers
            let event = make_key_event(Modifiers::empty(), Some(Key::F1), true);
            let results = state.process_event(&event);

            assert_eq!(results.len(), 1);
            assert_eq!(results[0].state, HotkeyState::Pressed);

            // F1 with modifiers should NOT trigger
            state.pressed_hotkeys.clear();
            let event = make_key_event(Modifiers::CMD_LEFT, Some(Key::F1), true);
            let results = state.process_event(&event);

            assert_eq!(results.len(), 0);
        }

        #[test]
        fn modifier_only_hotkey_not_released_by_unrelated_modifier() {
            let mut state = ManagerState::new();
            let hotkey = Hotkey::new(Modifiers::CMD, None).unwrap();
            let id = HotkeyId(0);
            state.hotkeys.insert(id, hotkey);

            // Cmd down — hotkey pressed
            let event = make_modifier_event(Modifiers::CMD_LEFT, true, Modifiers::CMD_LEFT);
            let results = state.process_event(&event);
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].state, HotkeyState::Pressed);

            // Shift down while Cmd held — no state change
            let event = make_modifier_event(
                Modifiers::CMD_LEFT | Modifiers::SHIFT_LEFT,
                true,
                Modifiers::SHIFT_LEFT,
            );
            assert_eq!(state.process_event(&event).len(), 0);

            // Shift up — Cmd is still held and still matches, so the hotkey
            // must NOT be released
            let event = make_modifier_event(Modifiers::CMD_LEFT, false, Modifiers::SHIFT_LEFT);
            assert_eq!(state.process_event(&event).len(), 0);
            assert!(state.pressed_hotkeys.contains(&id));

            // Cmd up — now it releases
            let event = make_modifier_event(Modifiers::empty(), false, Modifiers::CMD_LEFT);
            let results = state.process_event(&event);
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].state, HotkeyState::Released);
            assert!(!state.pressed_hotkeys.contains(&id));
        }

        #[test]
        fn modifier_only_hotkey_releases_on_own_modifier_release() {
            let mut state = ManagerState::new();
            let hotkey = Hotkey::new(Modifiers::CMD, None).unwrap();
            let id = HotkeyId(0);
            state.hotkeys.insert(id, hotkey);

            let event = make_modifier_event(Modifiers::CMD_LEFT, true, Modifiers::CMD_LEFT);
            state.process_event(&event);
            assert!(state.pressed_hotkeys.contains(&id));

            let event = make_modifier_event(Modifiers::empty(), false, Modifiers::CMD_LEFT);
            let results = state.process_event(&event);
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].state, HotkeyState::Released);
        }

        #[test]
        fn compound_modifier_only_hotkey_releases_on_partial_release() {
            let mut state = ManagerState::new();
            let hotkey = Hotkey::new(Modifiers::CMD | Modifiers::SHIFT, None).unwrap();
            let id = HotkeyId(0);
            state.hotkeys.insert(id, hotkey);

            // Cmd down, then Shift down — pressed once both are held
            let event = make_modifier_event(Modifiers::CMD_LEFT, true, Modifiers::CMD_LEFT);
            assert_eq!(state.process_event(&event).len(), 0);
            let event = make_modifier_event(
                Modifiers::CMD_LEFT | Modifiers::SHIFT_LEFT,
                true,
                Modifiers::SHIFT_LEFT,
            );
            let results = state.process_event(&event);
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].state, HotkeyState::Pressed);

            // Releasing either modifier breaks the match — released
            let event = make_modifier_event(Modifiers::SHIFT_LEFT, false, Modifiers::CMD_LEFT);
            let results = state.process_event(&event);
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].state, HotkeyState::Released);
        }

        #[test]
        fn keyed_hotkey_not_released_by_unrelated_key_release() {
            let mut state = ManagerState::new();
            let hotkey = Hotkey::new(Modifiers::CMD, Key::K).unwrap();
            let id = HotkeyId(0);
            state.hotkeys.insert(id, hotkey);

            let event = make_key_event(Modifiers::CMD_LEFT, Some(Key::K), true);
            state.process_event(&event);
            assert!(state.pressed_hotkeys.contains(&id));

            // Release of a different key while Cmd+K is held — no release
            let event = make_key_event(Modifiers::CMD_LEFT, Some(Key::J), false);
            assert_eq!(state.process_event(&event).len(), 0);
            assert!(state.pressed_hotkeys.contains(&id));
        }

        #[test]
        fn side_specific_hotkey_matches_correct_side() {
            let mut state = ManagerState::new();
            // Register CtrlRight+Space
            let hotkey = Hotkey::new(Modifiers::CTRL_RIGHT, Key::Space).unwrap();
            let id = HotkeyId(0);
            state.hotkeys.insert(id, hotkey);

            // Left ctrl should not trigger
            let event = make_key_event(Modifiers::CTRL_LEFT, Some(Key::Space), true);
            assert_eq!(state.process_event(&event).len(), 0);

            // Right ctrl should trigger
            let event = make_key_event(Modifiers::CTRL_RIGHT, Some(Key::Space), true);
            let results = state.process_event(&event);
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].state, HotkeyState::Pressed);
        }

        #[test]
        fn compound_hotkey_matches_either_side() {
            let mut state = ManagerState::new();
            let hotkey = Hotkey::new(Modifiers::CMD, Key::K).unwrap();
            let id = HotkeyId(0);
            state.hotkeys.insert(id, hotkey);

            // Left Cmd triggers
            let event = make_key_event(Modifiers::CMD_LEFT, Some(Key::K), true);
            let results = state.process_event(&event);
            assert_eq!(results.len(), 1);

            // Release
            state.pressed_hotkeys.clear();

            // Right Cmd also triggers
            let event = make_key_event(Modifiers::CMD_RIGHT, Some(Key::K), true);
            let results = state.process_event(&event);
            assert_eq!(results.len(), 1);
        }

        // -----------------------------------------------------------------
        // Hold-to-activate for single-modifier hotkeys
        // -----------------------------------------------------------------

        const HOLD: Duration = Duration::from_millis(400);

        fn hold_state() -> ManagerState {
            let mut state = ManagerState::new();
            state.single_modifier_hold = Some(HOLD);
            state
        }

        /// Press a modifier key (side-specific), as FlagsChanged delivers it.
        fn modifier_down(modifiers: Modifiers, changed: Modifiers) -> KeyEvent {
            make_modifier_event(modifiers, true, changed)
        }

        fn modifier_up(modifiers: Modifiers, changed: Modifiers) -> KeyEvent {
            make_modifier_event(modifiers, false, changed)
        }

        #[test]
        fn single_modifier_press_activates_only_after_hold() {
            let mut state = hold_state();
            let hotkey = Hotkey::new(Modifiers::CMD_RIGHT, None).unwrap();
            let id = HotkeyId(0);
            state.hotkeys.insert(id, hotkey);

            let t0 = Instant::now();
            assert!(
                state
                    .process_event_at(
                        &modifier_down(Modifiers::CMD_RIGHT, Modifiers::CMD_RIGHT),
                        t0
                    )
                    .is_empty(),
                "press must not activate immediately"
            );
            assert!(
                state
                    .process_tick(t0 + Duration::from_millis(399))
                    .is_empty(),
                "activation must wait out the full window"
            );
            let events = state.process_tick(t0 + HOLD);
            assert_eq!(events.len(), 1, "the elapsed hold activates");
            assert_eq!(events[0].id, id);
            assert_eq!(events[0].state, HotkeyState::Pressed);

            // The later release behaves like any hotkey release.
            let events =
                state.process_event(&modifier_up(Modifiers::empty(), Modifiers::CMD_RIGHT));
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].state, HotkeyState::Released);
        }

        #[test]
        fn chord_release_before_hold_never_activates() {
            let mut state = hold_state();
            let hotkey = Hotkey::new(Modifiers::CMD_LEFT, None).unwrap();
            state.hotkeys.insert(HotkeyId(0), hotkey);

            let t0 = Instant::now();
            state.process_event_at(&modifier_down(Modifiers::CMD_LEFT, Modifiers::CMD_LEFT), t0);
            // Cmd+C: the C key cancels, the fast Cmd release resolves early.
            state.process_event_at(
                &make_key_event(Modifiers::CMD_LEFT, Some(Key::C), true),
                t0 + Duration::from_millis(30),
            );
            state.process_event_at(
                &make_key_event(Modifiers::CMD_LEFT, Some(Key::C), false),
                t0 + Duration::from_millis(80),
            );
            state.process_event_at(
                &modifier_up(Modifiers::empty(), Modifiers::CMD_LEFT),
                t0 + Duration::from_millis(100),
            );

            assert!(
                state.process_tick(t0 + Duration::from_secs(5)).is_empty(),
                "a released chord must never activate"
            );
        }

        /// The exact shape of an app-synthesized paste chord (Cmd down, V
        /// click, Cmd up ~100ms later) as it re-enters the tap: it must
        /// never activate a single-modifier command binding. Regression
        /// guard for the VoxBar toggle auto-restart.
        #[test]
        fn synthesized_chord_shape_never_activates() {
            let mut state = hold_state();
            let hotkey = Hotkey::new(Modifiers::CMD_LEFT, None).unwrap();
            state.hotkeys.insert(HotkeyId(0), hotkey);

            let t0 = Instant::now();
            state.process_event_at(&modifier_down(Modifiers::CMD_LEFT, Modifiers::CMD_LEFT), t0);
            state.process_event_at(
                &make_key_event(Modifiers::CMD_LEFT, Some(Key::V), true),
                t0 + Duration::from_millis(5),
            );
            state.process_event_at(
                &make_key_event(Modifiers::CMD_LEFT, Some(Key::V), false),
                t0 + Duration::from_millis(60),
            );
            state.process_event_at(
                &modifier_up(Modifiers::empty(), Modifiers::CMD_LEFT),
                t0 + Duration::from_millis(100),
            );

            assert!(
                state.process_tick(t0 + Duration::from_secs(5)).is_empty(),
                "the injected paste chord must not fire the modifier-only binding"
            );
        }

        #[test]
        fn any_other_key_event_cancels_pending_hold() {
            let mut state = hold_state();
            let hotkey = Hotkey::new(Modifiers::SHIFT_RIGHT, None).unwrap();
            state.hotkeys.insert(HotkeyId(0), hotkey);

            let t0 = Instant::now();
            state.process_event_at(
                &modifier_down(Modifiers::SHIFT_RIGHT, Modifiers::SHIFT_RIGHT),
                t0,
            );
            // An unrelated modifier press mid-hold is also "another key".
            state.process_event_at(
                &modifier_down(
                    Modifiers::SHIFT_RIGHT | Modifiers::CMD_LEFT,
                    Modifiers::CMD_LEFT,
                ),
                t0 + Duration::from_millis(50),
            );

            assert!(
                state.process_tick(t0 + HOLD).is_empty(),
                "the chord extension must cancel the pending activation"
            );
        }

        #[test]
        fn autorepeat_keeps_earliest_press_time() {
            let mut state = hold_state();
            let hotkey = Hotkey::new(Modifiers::CMD_RIGHT, None).unwrap();
            state.hotkeys.insert(HotkeyId(0), hotkey);

            let t0 = Instant::now();
            state.process_event_at(
                &modifier_down(Modifiers::CMD_RIGHT, Modifiers::CMD_RIGHT),
                t0,
            );
            // Auto-repeat presses of the same modifier keep the window
            // running instead of restarting or multiplying it.
            for _ in 0..5 {
                state.process_event_at(
                    &modifier_down(Modifiers::CMD_RIGHT, Modifiers::CMD_RIGHT),
                    t0 + Duration::from_millis(200),
                );
            }
            assert_eq!(state.pending_holds.len(), 1);
            assert!(
                state
                    .process_tick(t0 + Duration::from_millis(399))
                    .is_empty(),
                "window is measured from the first press"
            );
            assert_eq!(state.process_tick(t0 + HOLD).len(), 1);
        }

        #[test]
        fn keyed_and_multi_modifier_hotkeys_fire_immediately_under_hold() {
            let mut state = hold_state();
            let combo = Hotkey::new(Modifiers::CTRL_LEFT | Modifiers::FN, None).unwrap();
            let keyed = Hotkey::new(Modifiers::CMD, Key::K).unwrap();
            state.hotkeys.insert(HotkeyId(0), combo);
            state.hotkeys.insert(HotkeyId(1), keyed);

            // ctrl_left+fn (the operator's transcribe combo) is two modifier
            // groups: it fires as soon as both are held.
            let events = state.process_event(&modifier_down(
                Modifiers::CTRL_LEFT | Modifiers::FN,
                Modifiers::FN,
            ));
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].id, HotkeyId(0));

            state.pressed_hotkeys.clear();

            // Cmd+K fires on the key press, unchanged.
            let events =
                state.process_event(&make_key_event(Modifiers::CMD_RIGHT, Some(Key::K), true));
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].id, HotkeyId(1));
        }

        #[test]
        fn without_hold_policy_upstream_behavior_is_unchanged() {
            let mut state = ManagerState::new();
            let hotkey = Hotkey::new(Modifiers::CMD_RIGHT, None).unwrap();
            state.hotkeys.insert(HotkeyId(0), hotkey);

            let events =
                state.process_event(&modifier_down(Modifiers::CMD_RIGHT, Modifiers::CMD_RIGHT));
            assert_eq!(events.len(), 1, "no hold policy: press fires immediately");
            assert_eq!(events[0].state, HotkeyState::Pressed);
        }

        #[test]
        fn unregistering_drops_pending_hold() {
            let mut state = hold_state();
            let hotkey = Hotkey::new(Modifiers::CMD_RIGHT, None).unwrap();
            state.hotkeys.insert(HotkeyId(0), hotkey);

            let t0 = Instant::now();
            state.process_event_at(
                &modifier_down(Modifiers::CMD_RIGHT, Modifiers::CMD_RIGHT),
                t0,
            );
            state.hotkeys.remove(&HotkeyId(0));

            assert!(
                state.process_tick(t0 + HOLD).is_empty(),
                "an unregistered hotkey must not activate"
            );
        }

        #[test]
        fn next_wake_reports_earliest_deadline() {
            let mut state = hold_state();
            assert_eq!(state.next_wake(), None, "no pending holds: no wake needed");

            let t0 = Instant::now();
            state.pending_holds.push(PendingHold {
                id: HotkeyId(0),
                side_bit: Modifiers::CMD_RIGHT,
                pressed_at: t0,
                threshold: HOLD,
            });
            let wake = state.next_wake().expect("pending hold schedules a wake");
            assert!(wake <= HOLD, "wake is at most the remaining window");
        }

        // -----------------------------------------------------------------
        // Per-binding hold windows: `register_with_hold` stamps one hotkey
        // with its own threshold instead of the manager-wide default. The
        // timing matrix below pins the short window the host app binds for
        // command mode (~150 ms) against every way a press can resolve.
        // -----------------------------------------------------------------

        /// The command-mode window the host app registers (see the host's
        /// `COMMAND_MODE_HOLD_THRESHOLD`); anything below HOLD exercises
        /// the per-binding machinery.
        const SHORT_HOLD: Duration = Duration::from_millis(150);

        /// State with the manager-wide 400 ms policy ON and one
        /// single-modifier hotkey (left command) overridden to SHORT_HOLD.
        fn short_hold_state() -> ManagerState {
            let mut state = hold_state();
            state.register_hotkey(
                Hotkey::new(Modifiers::CMD_LEFT, None).unwrap(),
                Some(SHORT_HOLD),
            );
            state
        }

        /// press-hold-engage: an overridden hotkey activates when ITS
        /// window elapses, not when the manager-wide default does. The
        /// press itself still never activates immediately.
        #[test]
        fn per_binding_hold_engages_at_its_own_threshold() {
            let mut state = short_hold_state();
            let t0 = Instant::now();

            assert!(
                state
                    .process_event_at(&modifier_down(Modifiers::CMD_LEFT, Modifiers::CMD_LEFT), t0)
                    .is_empty(),
                "the press must defer behind the hold window"
            );
            assert!(
                state
                    .process_tick(t0 + Duration::from_millis(149))
                    .is_empty(),
                "one millisecond short of the short window: still pending"
            );
            let events = state.process_tick(t0 + SHORT_HOLD);
            assert_eq!(
                events.len(),
                1,
                "the short window elapses at 150 ms, not 400 ms"
            );
            assert_eq!(events[0].id, HotkeyId(0));
            assert_eq!(events[0].state, HotkeyState::Pressed);

            // The later release behaves like any hotkey release.
            let events = state.process_event(&modifier_up(Modifiers::empty(), Modifiers::CMD_LEFT));
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].state, HotkeyState::Released);
        }

        /// press-tap-cancel: releasing the held modifier before even the
        /// short window elapses resolves the hold early and it never
        /// activates.
        #[test]
        fn short_window_tap_release_never_activates() {
            let mut state = short_hold_state();
            let t0 = Instant::now();
            state.process_event_at(&modifier_down(Modifiers::CMD_LEFT, Modifiers::CMD_LEFT), t0);
            state.process_event_at(
                &modifier_up(Modifiers::empty(), Modifiers::CMD_LEFT),
                t0 + Duration::from_millis(80),
            );

            assert!(
                state.process_tick(t0 + Duration::from_secs(5)).is_empty(),
                "a tap shorter than the short window must never activate"
            );
        }

        /// press-other-key-cancel: ANY other key event inside the short
        /// window cancels the pending activation, so a chord built on the
        /// bound modifier still cannot fire it. Both a plain letter key
        /// and a second modifier count as "another key".
        #[test]
        fn short_window_other_key_event_cancels_pending_hold() {
            // A letter key inside the window (the accidental Cmd+C shape).
            let mut state = short_hold_state();
            let t0 = Instant::now();
            state.process_event_at(&modifier_down(Modifiers::CMD_LEFT, Modifiers::CMD_LEFT), t0);
            state.process_event_at(
                &make_key_event(Modifiers::CMD_LEFT, Some(Key::C), true),
                t0 + Duration::from_millis(50),
            );
            assert!(
                state.process_tick(t0 + Duration::from_secs(5)).is_empty(),
                "a chord key inside the short window must cancel the activation"
            );

            // A second modifier inside the window is equally "another key".
            let mut state = short_hold_state();
            state.process_event_at(&modifier_down(Modifiers::CMD_LEFT, Modifiers::CMD_LEFT), t0);
            state.process_event_at(
                &modifier_down(
                    Modifiers::CMD_LEFT | Modifiers::SHIFT_LEFT,
                    Modifiers::SHIFT_LEFT,
                ),
                t0 + Duration::from_millis(50),
            );
            assert!(
                state.process_tick(t0 + Duration::from_secs(5)).is_empty(),
                "a second modifier inside the short window must cancel the activation"
            );
        }

        /// per-binding threshold: the override moves exactly one hotkey's
        /// window. A sibling single-modifier hotkey registered without an
        /// override keeps the manager-wide default.
        #[test]
        fn override_leaves_the_manager_wide_window_intact() {
            let mut state = hold_state();
            state.register_hotkey(
                Hotkey::new(Modifiers::CMD_LEFT, None).unwrap(),
                Some(SHORT_HOLD),
            );
            state.register_hotkey(Hotkey::new(Modifiers::CMD_RIGHT, None).unwrap(), None);

            let t0 = Instant::now();
            state.process_event_at(
                &modifier_down(Modifiers::CMD_RIGHT, Modifiers::CMD_RIGHT),
                t0,
            );
            assert!(
                state.process_tick(t0 + SHORT_HOLD).is_empty(),
                "the default-window hotkey must not inherit the short window"
            );
            let events = state.process_tick(t0 + HOLD);
            assert_eq!(
                events.len(),
                1,
                "the default-window hotkey activates at the manager-wide threshold"
            );
            assert_eq!(events[0].id, HotkeyId(1));
        }

        /// An override is self-sufficient: it gates its hotkey even when
        /// no manager-wide hold policy is configured at all.
        #[test]
        fn override_gates_without_a_manager_wide_policy() {
            let mut state = ManagerState::new();
            state.register_hotkey(
                Hotkey::new(Modifiers::CMD_LEFT, None).unwrap(),
                Some(SHORT_HOLD),
            );

            let t0 = Instant::now();
            assert!(
                state
                    .process_event_at(&modifier_down(Modifiers::CMD_LEFT, Modifiers::CMD_LEFT), t0)
                    .is_empty(),
                "the override alone must defer the press"
            );
            assert_eq!(
                state.process_tick(t0 + SHORT_HOLD).len(),
                1,
                "the override alone must run the window"
            );
        }

        /// The hold gate only exists for single-modifier hotkeys: an
        /// override stamped on a keyed combo is inert and the combo still
        /// fires on the press, exactly as before.
        #[test]
        fn override_on_a_keyed_hotkey_is_inert() {
            let mut state = ManagerState::new();
            state.register_hotkey(
                Hotkey::new(Modifiers::CMD_LEFT, Key::K).unwrap(),
                Some(SHORT_HOLD),
            );

            let events =
                state.process_event(&make_key_event(Modifiers::CMD_LEFT, Some(Key::K), true));
            assert_eq!(
                events.len(),
                1,
                "combos fire on the press regardless of any hold override"
            );
        }

        /// Mixed pending holds with different thresholds: `next_wake`
        /// reports the earliest deadline across ALL windows so a short
        /// override still fires promptly beside a long default.
        #[test]
        fn next_wake_spans_per_binding_thresholds() {
            let mut state = hold_state();
            state.register_hotkey(
                Hotkey::new(Modifiers::CMD_LEFT, None).unwrap(),
                Some(SHORT_HOLD),
            );
            state.register_hotkey(Hotkey::new(Modifiers::CMD_RIGHT, None).unwrap(), None);
            let t0 = Instant::now();
            state.pending_holds.push(PendingHold {
                id: HotkeyId(0),
                side_bit: Modifiers::CMD_LEFT,
                pressed_at: t0,
                threshold: SHORT_HOLD,
            });
            state.pending_holds.push(PendingHold {
                id: HotkeyId(1),
                side_bit: Modifiers::CMD_RIGHT,
                pressed_at: t0,
                threshold: HOLD,
            });

            let wake = state.next_wake().expect("pending holds schedule a wake");
            assert!(
                wake <= SHORT_HOLD,
                "the short window must drive the poll cadence, got {wake:?}"
            );
        }

        /// Unregistering a hotkey drops its override with it: a later
        /// registration of the same chord starts from the manager-wide
        /// policy again (state-level core of `HotkeyManager::unregister`).
        #[test]
        fn unregistering_hotkey_drops_its_override() {
            let mut state = short_hold_state();
            assert_eq!(state.hold_threshold(&HotkeyId(0)), Some(SHORT_HOLD));

            state.unregister_hotkey(&HotkeyId(0));

            assert!(state.hold_overrides.is_empty(), "no orphaned override");
            assert!(
                !state.hold_gated(&HotkeyId(0)),
                "a stale id must not gate anything"
            );
            // Without the manager-wide policy the stale id resolves to no
            // window at all; with it, the id falls back to the default
            // exactly like a fresh registration would.
            let mut bare = ManagerState::new();
            bare.register_hotkey(
                Hotkey::new(Modifiers::CMD_LEFT, None).unwrap(),
                Some(SHORT_HOLD),
            );
            bare.unregister_hotkey(&HotkeyId(0));
            assert_eq!(bare.hold_threshold(&HotkeyId(0)), None);
        }
    }
}
