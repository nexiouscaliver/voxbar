//! Handy-keys based keyboard shortcut implementation
//!
//! This module provides an alternative to Tauri's global-shortcut plugin
//! using the handy-keys library for more control over keyboard events.
//!
//! ## Side-specific modifiers
//!
//! macOS modifier bindings are side-strict: `command_left` fires only on
//! the left command key, `command_right` only on the right one, and the
//! capture UI records the side that was physically pressed (the wire
//! values `command_right` / `shift_right` / `option_right` /
//! `control_right` alongside the left ones). The vendored handy-keys fork
//! maps both macOS keycode families for right-side modifiers: classic
//! kVK_ codes from external keyboards and the device-dependent codes
//! (0x7C / 0x6A / 0x6B) from built-in Apple keyboards. One hardware
//! collapse remains and is not fixable in software: some external
//! keyboards deliver right command with the left command keycode (0x37).
//! Existing bindings stored as left variants keep working as left-strict.
//!
//! Non-macOS platforms keep the crate's existing behavior: the Windows
//! and Linux backends already report side-specific modifier events
//! natively, so side-strict matching works there without extra mapping.
//!
//! ## Hold-to-activate
//!
//! A binding that is exactly one modifier key (no main key) activates
//! only after that modifier has been held ~400ms with no other key event
//! in between; chord usage (Cmd+C and friends) releases far faster and
//! can never trigger, and any other key during the window cancels the
//! pending activation. Combos (`ctrl_left+fn`, `option+space`) fire on
//! the press as before. This applies on every platform (the arbitration
//! lives in the platform-agnostic manager).
//!
//! ## Architecture
//!
//! The implementation uses a dedicated manager thread that owns the `HotkeyManager`:
//!
//! ```text
//! ┌─────────────────┐     commands      ┌──────────────────────┐
//! │   Main Thread   │ ───────────────▶ │   Manager Thread     │
//! │                 │   (via channel)   │                      │
//! │ - register()    │                   │ - owns HotkeyManager │
//! │ - unregister()  │                   │ - polls for events   │
//! └─────────────────┘                   │ - dispatches actions │
//!                                       └──────────────────────┘
//! ```
//!
//! This design ensures thread-safety since `HotkeyManager` is only accessed
//! from a single thread. Commands (register/unregister) are sent via an mpsc
//! channel and responses are synchronously awaited.
//!
//! ## Recording Mode
//!
//! For UI key capture, a separate `KeyboardListener` is created on-demand and
//! polled from a dedicated recording thread. Events are emitted to the frontend
//! via Tauri's event system.

use handy_keys::{Hotkey, HotkeyId, HotkeyManager, HotkeyState, KeyboardListener};
use log::{debug, error, info};
use serde::Serialize;
use specta::Type;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use tauri::{AppHandle, Emitter, Manager};

use crate::settings::{self, get_settings, ShortcutBinding};

use super::handler::handle_shortcut_event;

/// Commands that can be sent to the hotkey manager thread
enum ManagerCommand {
    Register {
        binding_id: String,
        hotkey_string: String,
        response: Sender<Result<(), String>>,
    },
    Unregister {
        binding_id: String,
        response: Sender<Result<(), String>>,
    },
    Shutdown,
}

/// State for the handy-keys shortcut manager
pub struct HandyKeysState {
    /// Channel to send commands to the manager thread (wrapped in Mutex for Sync)
    command_sender: Mutex<Sender<ManagerCommand>>,
    /// Handle to the manager thread (wrapped in Mutex for Sync, allows proper join on drop)
    thread_handle: Mutex<Option<JoinHandle<()>>>,
    /// Recording listener for UI key capture (only active during recording)
    recording_listener: Mutex<Option<KeyboardListener>>,
    /// Flag indicating if we're in recording mode
    is_recording: AtomicBool,
    /// The binding ID being recorded (if any)
    recording_binding_id: Mutex<Option<String>>,
    /// Flag to stop recording loop
    recording_running: Arc<AtomicBool>,
}

/// Key event sent to frontend during recording mode
#[derive(Debug, Clone, Serialize, Type)]
pub struct FrontendKeyEvent {
    /// Currently pressed modifier keys
    pub modifiers: Vec<String>,
    /// The key that was pressed (if any)
    pub key: Option<String>,
    /// Whether this is a key down event
    pub is_key_down: bool,
    /// The full hotkey string (e.g., "option+space")
    pub hotkey_string: String,
}

/// How long `HandyKeysState::new` waits for the manager thread to report
/// whether its `HotkeyManager` came up. Manager creation is fast (an event
/// tap / evdev scan), so anything close to this bound means the thread is
/// wedged or starved, which must surface as an init failure rather than a
/// hotkey-dead app whose only trace is a log line.
const MANAGER_STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

impl HandyKeysState {
    /// Create a new HandyKeysState
    ///
    /// Fails when the manager thread could not create its `HotkeyManager`
    /// (for example Linux without `input`-group membership, or macOS before
    /// Accessibility is granted). Without this propagation every later
    /// register fails "disconnected" while `init_shortcuts` still reported
    /// success, leaving a hotkey-dead app whose only explanation lived in
    /// the log file.
    pub fn new(app: AppHandle) -> Result<Self, String> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<ManagerCommand>();
        let (startup_tx, startup_rx) = mpsc::channel::<Result<(), String>>();

        // Start the manager thread
        let app_clone = app.clone();
        let thread_handle = thread::spawn(move || {
            Self::manager_thread(cmd_rx, startup_tx, app_clone);
        });

        // The vendored manager's error text is the actionable part (it names
        // the exact permission fix), so it is passed through verbatim.
        Self::await_manager_startup(startup_rx, MANAGER_STARTUP_TIMEOUT)?;

        Ok(Self {
            command_sender: Mutex::new(cmd_tx),
            thread_handle: Mutex::new(Some(thread_handle)),
            recording_listener: Mutex::new(None),
            is_recording: AtomicBool::new(false),
            recording_binding_id: Mutex::new(None),
            recording_running: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Wait for the manager thread's startup result with a bounded patience.
    /// Pure channel plumbing, separated from `new` so the propagation
    /// contract is unit-testable without an app or real input backends.
    fn await_manager_startup(
        startup_rx: Receiver<Result<(), String>>,
        timeout: std::time::Duration,
    ) -> Result<(), String> {
        match startup_rx.recv_timeout(timeout) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(format!("Failed to create hotkey manager: {}", e)),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(format!(
                "Hotkey manager did not start within {} seconds",
                timeout.as_secs()
            )),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err("Hotkey manager thread exited before reporting readiness".into())
            }
        }
    }

    /// The main manager thread - owns the HotkeyManager and processes commands
    fn manager_thread(
        cmd_rx: Receiver<ManagerCommand>,
        startup_tx: Sender<Result<(), String>>,
        app: AppHandle,
    ) {
        info!("handy-keys manager thread started");

        // Create the HotkeyManager in this thread, then report the outcome to
        // the waiting constructor so a dead manager becomes an init error
        // instead of a silently broken registration channel.
        let manager = match HotkeyManager::new_with_blocking() {
            Ok(m) => {
                let _ = startup_tx.send(Ok(()));
                m
            }
            Err(e) => {
                error!("Failed to create HotkeyManager: {}", e);
                let _ = startup_tx.send(Err(e.to_string()));
                return;
            }
        }
        // A single-modifier binding (e.g. command_right) is also the lead
        // key of every chord, so it only activates after a deliberate
        // ~400ms hold with no other key in between; combos like
        // ctrl_left+fn keep firing on the press. See the vendored crate's
        // manager for the arbitration rules.
        .with_single_modifier_hold(handy_keys::SINGLE_MODIFIER_HOLD_THRESHOLD);

        // Maps binding IDs to HotkeyIds and hotkey strings
        let mut binding_to_hotkey: HashMap<String, HotkeyId> = HashMap::new();
        let mut hotkey_to_binding: HashMap<HotkeyId, (String, String)> = HashMap::new(); // (binding_id, hotkey_string)

        loop {
            // Check for hotkey events (non-blocking)
            while let Some(event) = manager.try_recv() {
                if let Some((binding_id, hotkey_string)) = hotkey_to_binding.get(&event.id) {
                    debug!(
                        "handy-keys event: binding={}, hotkey={}, state={:?}",
                        binding_id, hotkey_string, event.state
                    );
                    let is_pressed = event.state == HotkeyState::Pressed;
                    handle_shortcut_event(&app, binding_id, hotkey_string, is_pressed);
                }
            }

            // Check for commands (non-blocking with timeout)
            match cmd_rx.recv_timeout(std::time::Duration::from_millis(10)) {
                Ok(cmd) => match cmd {
                    ManagerCommand::Register {
                        binding_id,
                        hotkey_string,
                        response,
                    } => {
                        let result = Self::do_register(
                            &manager,
                            &mut binding_to_hotkey,
                            &mut hotkey_to_binding,
                            &binding_id,
                            &hotkey_string,
                        );
                        let _ = response.send(result);
                    }
                    ManagerCommand::Unregister {
                        binding_id,
                        response,
                    } => {
                        let result = Self::do_unregister(
                            &manager,
                            &mut binding_to_hotkey,
                            &mut hotkey_to_binding,
                            &binding_id,
                        );
                        let _ = response.send(result);
                    }
                    ManagerCommand::Shutdown => {
                        info!("handy-keys manager thread shutting down");
                        break;
                    }
                },
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // No command, continue
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    info!("Command channel disconnected, shutting down");
                    break;
                }
            }
        }

        info!("handy-keys manager thread stopped");
    }

    /// Register a hotkey
    fn do_register(
        manager: &HotkeyManager,
        binding_to_hotkey: &mut HashMap<String, HotkeyId>,
        hotkey_to_binding: &mut HashMap<HotkeyId, (String, String)>,
        binding_id: &str,
        hotkey_string: &str,
    ) -> Result<(), String> {
        let hotkey: Hotkey = hotkey_string
            .parse()
            .map_err(|e| format!("Failed to parse hotkey '{}': {}", hotkey_string, e))?;

        let id = manager
            .register(hotkey)
            .map_err(|e| format!("Failed to register hotkey: {}", e))?;

        binding_to_hotkey.insert(binding_id.to_string(), id);
        hotkey_to_binding.insert(id, (binding_id.to_string(), hotkey_string.to_string()));

        debug!(
            "Registered handy-keys shortcut: {} -> {:?}",
            binding_id, hotkey
        );
        Ok(())
    }

    /// Unregister a hotkey
    fn do_unregister(
        manager: &HotkeyManager,
        binding_to_hotkey: &mut HashMap<String, HotkeyId>,
        hotkey_to_binding: &mut HashMap<HotkeyId, (String, String)>,
        binding_id: &str,
    ) -> Result<(), String> {
        if let Some(id) = binding_to_hotkey.remove(binding_id) {
            manager
                .unregister(id)
                .map_err(|e| format!("Failed to unregister hotkey: {}", e))?;
            hotkey_to_binding.remove(&id);
            debug!("Unregistered handy-keys shortcut: {}", binding_id);
        }
        Ok(())
    }

    /// Register a shortcut binding
    pub fn register(&self, binding: &ShortcutBinding) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        self.command_sender
            .lock()
            .map_err(|_| "Failed to lock command_sender")?
            .send(ManagerCommand::Register {
                binding_id: binding.id.clone(),
                hotkey_string: binding.current_binding.clone(),
                response: tx,
            })
            .map_err(|_| "Failed to send register command")?;

        rx.recv()
            .map_err(|_| "Failed to receive register response")?
    }

    /// Unregister a shortcut binding
    pub fn unregister(&self, binding: &ShortcutBinding) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        self.command_sender
            .lock()
            .map_err(|_| "Failed to lock command_sender")?
            .send(ManagerCommand::Unregister {
                binding_id: binding.id.clone(),
                response: tx,
            })
            .map_err(|_| "Failed to send unregister command")?;

        rx.recv()
            .map_err(|_| "Failed to receive unregister response")?
    }

    /// Start recording mode for a specific binding
    pub fn start_recording(&self, app: &AppHandle, binding_id: String) -> Result<(), String> {
        if self.is_recording.load(Ordering::SeqCst) {
            return Err("Already recording".into());
        }

        // Create a new keyboard listener for recording
        let listener = KeyboardListener::new()
            .map_err(|e| format!("Failed to create keyboard listener: {}", e))?;

        {
            let mut recording = self
                .recording_listener
                .lock()
                .map_err(|_| "Failed to lock recording_listener")?;
            *recording = Some(listener);
        }
        {
            let mut binding = self
                .recording_binding_id
                .lock()
                .map_err(|_| "Failed to lock recording_binding_id")?;
            *binding = Some(binding_id);
        }

        self.is_recording.store(true, Ordering::SeqCst);
        self.recording_running.store(true, Ordering::SeqCst);

        // Start a thread to emit key events to the frontend
        let app_clone = app.clone();
        let recording_running = Arc::clone(&self.recording_running);
        thread::spawn(move || {
            Self::recording_loop(app_clone, recording_running);
        });

        debug!("Started handy-keys recording mode");
        Ok(())
    }

    /// Recording loop - emits key events to frontend during recording
    fn recording_loop(app: AppHandle, running: Arc<AtomicBool>) {
        while running.load(Ordering::SeqCst) {
            let event = {
                let state = match app.try_state::<HandyKeysState>() {
                    Some(s) => s,
                    None => break,
                };
                let listener = state.recording_listener.lock().ok();
                listener.as_ref().and_then(|l| l.as_ref()?.try_recv())
            };

            if let Some(key_event) = event {
                // Convert to frontend-friendly format
                let frontend_event = FrontendKeyEvent {
                    modifiers: modifiers_to_strings(key_event.modifiers),
                    key: key_event.key.map(|k| k.to_string().to_lowercase()),
                    is_key_down: key_event.is_key_down,
                    hotkey_string: key_event
                        .as_hotkey()
                        .map(|h| h.to_handy_string())
                        .unwrap_or_default(),
                };

                // Emit to frontend
                if let Err(e) = app.emit("handy-keys-event", &frontend_event) {
                    error!("Failed to emit key event: {}", e);
                }
            } else {
                thread::sleep(std::time::Duration::from_millis(10));
            }
        }

        debug!("Recording loop ended");
    }

    /// Stop recording mode
    pub fn stop_recording(&self) -> Result<(), String> {
        self.is_recording.store(false, Ordering::SeqCst);
        self.recording_running.store(false, Ordering::SeqCst);

        {
            let mut recording = self
                .recording_listener
                .lock()
                .map_err(|_| "Failed to lock recording_listener")?;
            *recording = None;
        }
        {
            let mut binding = self
                .recording_binding_id
                .lock()
                .map_err(|_| "Failed to lock recording_binding_id")?;
            *binding = None;
        }

        debug!("Stopped handy-keys recording mode");
        Ok(())
    }
}

impl Drop for HandyKeysState {
    fn drop(&mut self) {
        // Signal recording to stop
        self.recording_running.store(false, Ordering::SeqCst);
        self.is_recording.store(false, Ordering::SeqCst);

        // Send shutdown command
        if let Ok(sender) = self.command_sender.lock() {
            let _ = sender.send(ManagerCommand::Shutdown);
        }

        // Wait for the manager thread to finish
        if let Ok(mut handle) = self.thread_handle.lock() {
            if let Some(h) = handle.take() {
                let _ = h.join();
            }
        }
    }
}

/// Convert handy-keys Modifiers to a list of strings.
///
/// Side-aware on every platform: when exactly one side of a modifier group
/// is held the name carries the side (`command_left`, `shift_right`), and
/// only a genuinely two-sided hold reports the plain name. The recorder's
/// committed value still comes from the event's `hotkey_string`, which
/// records the side that was actually pressed.
fn modifiers_to_strings(modifiers: handy_keys::Modifiers) -> Vec<String> {
    use handy_keys::Modifiers as M;
    let mut result = Vec::new();

    fn side_name(
        modifiers: M,
        left: M,
        right: M,
        left_name: &'static str,
        right_name: &'static str,
        plain_name: &'static str,
    ) -> Option<&'static str> {
        let has_left = modifiers.contains(left);
        let has_right = modifiers.contains(right);
        match (has_left, has_right) {
            (true, false) => Some(left_name),
            (false, true) => Some(right_name),
            (true, true) => Some(plain_name),
            (false, false) => None,
        }
    }

    if let Some(name) = side_name(
        modifiers,
        M::CTRL_LEFT,
        M::CTRL_RIGHT,
        "ctrl_left",
        "ctrl_right",
        "ctrl",
    ) {
        result.push(name.to_string());
    }
    #[cfg(target_os = "macos")]
    let (opt_left, opt_right, opt_plain) = ("option_left", "option_right", "option");
    #[cfg(not(target_os = "macos"))]
    let (opt_left, opt_right, opt_plain) = ("alt_left", "alt_right", "alt");
    if let Some(name) = side_name(
        modifiers,
        M::OPT_LEFT,
        M::OPT_RIGHT,
        opt_left,
        opt_right,
        opt_plain,
    ) {
        result.push(name.to_string());
    }
    if let Some(name) = side_name(
        modifiers,
        M::SHIFT_LEFT,
        M::SHIFT_RIGHT,
        "shift_left",
        "shift_right",
        "shift",
    ) {
        result.push(name.to_string());
    }
    #[cfg(target_os = "macos")]
    let (cmd_left, cmd_right, cmd_plain) = ("command_left", "command_right", "command");
    #[cfg(not(target_os = "macos"))]
    let (cmd_left, cmd_right, cmd_plain) = ("super_left", "super_right", "super");
    if let Some(name) = side_name(
        modifiers,
        M::CMD_LEFT,
        M::CMD_RIGHT,
        cmd_left,
        cmd_right,
        cmd_plain,
    ) {
        result.push(name.to_string());
    }
    if modifiers.contains(M::FN) {
        result.push("fn".to_string());
    }

    result
}

/// Validate a shortcut string for the HandyKeys implementation.
/// HandyKeys is more permissive: allows modifier-only combos and the fn key.
pub fn validate_shortcut(raw: &str) -> Result<(), String> {
    if raw.trim().is_empty() {
        return Err("Shortcut cannot be empty".into());
    }
    // HandyKeys accepts modifier-only, key-only, and modifier+key combos
    // Just verify the string is parseable
    raw.parse::<Hotkey>()
        .map(|_| ())
        .map_err(|e| format!("Invalid shortcut for VoxBar Keys: {}", e))
}

/// Aggregate per-binding registration failures from init into one verdict.
///
/// A backend where every attempted registration failed is hotkey-dead, so it
/// must surface as an error (the caller rolls back to the Tauri backend and
/// persists that choice). A partially working backend is kept: swapping it
/// away would also lose the bindings that did register. Pure over the
/// collected outcomes so the threshold is unit-testable.
fn aggregate_registration_failures(
    attempted: usize,
    failures: Vec<(String, String)>,
) -> Result<(), String> {
    if attempted > 0 && failures.len() == attempted {
        let joined = failures
            .iter()
            .map(|(id, e)| format!("{}: {}", id, e))
            .collect::<Vec<_>>()
            .join("; ");
        Err(format!(
            "every shortcut registration failed under VoxBar Keys ({})",
            joined
        ))
    } else {
        Ok(())
    }
}

/// Whether the recording lifecycle may reconcile the cancel shortcut on this
/// platform/backend pair. Pure over (binding, platform, backend) so the
/// decision table is unit-testable on any host.
///
/// - Linux + Tauri stays disabled exactly as today: dynamic registration
///   through the global-shortcut plugin is the instability this guard was
///   added for (tray menu remains the abort there).
/// - Linux + HandyKeys is enabled: registration travels the vendored
///   manager's command channel, the same machinery `change_binding` already
///   uses at runtime on Linux, so the cancel key arms only while a session
///   is live. It is deliberately NOT registered statically at init: a
///   registered non-hold-gated hotkey joins the manager's blocking set
///   (vendor manager.rs `register`), so a statically registered Escape
///   would be swallowed system-wide in every other app.
/// - Other platforms: both backends reconcile dynamically, unchanged.
// Only the Linux reconcile path calls this; the decision table stays
// compiled (and unit-tested) everywhere.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn cancel_reconcile_enabled(
    binding_id: &str,
    is_linux: bool,
    backend: settings::KeyboardImplementation,
) -> bool {
    if binding_id != "cancel" {
        return false;
    }
    if !is_linux {
        return true;
    }
    backend == settings::KeyboardImplementation::HandyKeys
}

/// Initialize handy-keys shortcuts
pub fn init_shortcuts(app: &AppHandle) -> Result<(), String> {
    let state = HandyKeysState::new(app.clone())?;

    let default_bindings = settings::get_default_settings().bindings;
    let user_settings = settings::load_or_create_app_settings(app);

    // Register all bindings except cancel (which is dynamic)
    let mut attempted = 0usize;
    let mut failures: Vec<(String, String)> = Vec::new();
    for (id, default_binding) in default_bindings {
        if id == "cancel" {
            continue;
        }

        let binding = user_settings
            .bindings
            .get(&id)
            .cloned()
            .unwrap_or(default_binding);

        // Skip bindings that are unbound or disabled by their feature toggle.
        if !super::binding_is_active(&user_settings, &id, &binding) {
            continue;
        }

        attempted += 1;
        if let Err(e) = state.register(&binding) {
            error!(
                "Failed to register handy-keys shortcut {} during init: {}",
                id, e
            );
            failures.push((id, e));
        }
    }

    // A backend that registered nothing is hotkey-dead (for example every
    // binding rejected because the manager's backend lost its device
    // access); report it so the caller's rollback to Tauri runs.
    aggregate_registration_failures(attempted, failures)?;

    app.manage(state);
    info!("handy-keys shortcuts initialized");
    Ok(())
}

/// Register a shortcut
pub fn register_shortcut(app: &AppHandle, binding: ShortcutBinding) -> Result<(), String> {
    // Bare-key guard mirrors binding_is_active for the paths that register
    // directly (feature-toggle flips) instead of going through
    // change_binding's validation.
    if binding.id != "cancel" && super::is_bare_key_binding(&binding.current_binding) {
        return Err(format!(
            "treating '{}' as unbound: {}",
            binding.current_binding,
            super::bare_key_rejection(&binding.current_binding)
        ));
    }
    let state = app
        .try_state::<HandyKeysState>()
        .ok_or("HandyKeysState not initialized")?;
    state.register(&binding)
}

/// Unregister a shortcut
pub fn unregister_shortcut(app: &AppHandle, binding: ShortcutBinding) -> Result<(), String> {
    let state = app
        .try_state::<HandyKeysState>()
        .ok_or("HandyKeysState not initialized")?;
    state.unregister(&binding)
}

/// Start key recording mode
#[tauri::command]
#[specta::specta]
pub fn start_handy_keys_recording(app: AppHandle, binding_id: String) -> Result<(), String> {
    let settings = get_settings(&app);
    if settings.keyboard_implementation != settings::KeyboardImplementation::HandyKeys {
        return Err("handy-keys is not the active keyboard implementation".into());
    }

    // While Secure Input is active the tap receives no KeyDown/KeyUp, so the
    // recorder would silently capture just the modifier and overwrite the
    // binding with it (issue #1578). Refuse instead; the frontend maps this
    // marker to a localized explanation, and the noted impact makes the
    // warning banner appear with the full story.
    if crate::secure_input::is_enabled_now() {
        crate::secure_input::note_recorder_blocked(&app);
        return Err("secure-input-active".into());
    }

    let state = app
        .try_state::<HandyKeysState>()
        .ok_or("HandyKeysState not initialized")?;

    // Suspend every registered shortcut so a combo that overlaps an existing
    // binding can't fire it (or have its keys swallowed) mid-capture.
    super::suspend_all_shortcuts(&app);

    let result = state.start_recording(&app, binding_id);
    if result.is_err() {
        super::resume_all_shortcuts(&app);
    }
    result
}

/// Stop key recording mode
#[tauri::command]
#[specta::specta]
pub fn stop_handy_keys_recording(app: AppHandle) -> Result<(), String> {
    let settings = get_settings(&app);
    if settings.keyboard_implementation != settings::KeyboardImplementation::HandyKeys {
        return Err("handy-keys is not the active keyboard implementation".into());
    }

    let state = app
        .try_state::<HandyKeysState>()
        .ok_or("HandyKeysState not initialized")?;

    // Restore shortcuts from settings regardless of how recording ended.
    // A commit has already registered the new binding via change_binding;
    // re-registering it here fails cleanly and is ignored.
    let result = state.stop_recording();
    super::resume_all_shortcuts(&app);
    result
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::HandyKeysState;
    use super::MANAGER_STARTUP_TIMEOUT;
    use super::{aggregate_registration_failures, cancel_reconcile_enabled};
    use crate::settings::KeyboardImplementation;

    // ------------------------------------------------------------------
    // Manager-startup failure propagation (mocked result channel)
    // ------------------------------------------------------------------

    /// A manager thread that fails to create its HotkeyManager reports the
    /// vendored crate's actionable text over the startup channel, and the
    /// constructor surfaces it verbatim (prefixed only with context).
    #[test]
    fn manager_startup_failure_propagates_the_backend_message() {
        let (tx, rx) = mpsc::channel();
        let vendored_text = "permission denied opening 7 device node(s) under /dev/input. \
             Reading keyboard events requires read access to these nodes: add your user to \
             the 'input' group (sudo usermod -aG input $USER) and log out and back in";
        tx.send(Err(vendored_text.to_string())).unwrap();

        let err = HandyKeysState::await_manager_startup(rx, MANAGER_STARTUP_TIMEOUT).unwrap_err();
        assert!(
            err.contains("Failed to create hotkey manager"),
            "context prefix missing: {err}"
        );
        assert!(
            err.contains("sudo usermod -aG input $USER"),
            "the vendored fix-it text must survive verbatim: {err}"
        );
    }

    #[test]
    fn manager_startup_success_passes_through() {
        let (tx, rx) = mpsc::channel();
        tx.send(Ok(())).unwrap();
        assert!(HandyKeysState::await_manager_startup(rx, MANAGER_STARTUP_TIMEOUT).is_ok());
    }

    /// A wedged or starved manager thread must not hang app startup forever,
    /// and a thread that dies before reporting must fail rather than pass.
    #[test]
    fn manager_startup_timeout_and_disconnect_are_errors() {
        let (_tx, rx) = mpsc::channel::<Result<(), String>>();
        let err = HandyKeysState::await_manager_startup(rx, Duration::from_millis(10)).unwrap_err();
        assert!(err.contains("did not start within"), "{err}");

        let (tx, rx) = mpsc::channel::<Result<(), String>>();
        drop(tx);
        let err = HandyKeysState::await_manager_startup(rx, MANAGER_STARTUP_TIMEOUT).unwrap_err();
        assert!(err.contains("exited before reporting"), "{err}");
    }

    // ------------------------------------------------------------------
    // Registration-failure aggregation
    // ------------------------------------------------------------------

    #[test]
    fn all_registrations_failing_makes_init_fail() {
        let err = aggregate_registration_failures(
            2,
            vec![
                ("transcribe".into(), "disconnected".into()),
                ("cancel".into(), "disconnected".into()),
            ],
        )
        .unwrap_err();
        assert!(err.contains("every shortcut registration failed"), "{err}");
        assert!(err.contains("transcribe"), "{err}");
    }

    #[test]
    fn partially_working_backends_are_kept() {
        assert!(
            aggregate_registration_failures(2, vec![("undo".into(), "conflict".into())]).is_ok()
        );
        assert!(aggregate_registration_failures(0, vec![]).is_ok());
    }

    // ------------------------------------------------------------------
    // Cancel-shortcut reconciliation decision over (binding, platform,
    // backend)
    // ------------------------------------------------------------------

    #[test]
    fn cancel_reconciliation_follows_the_platform_backend_table() {
        for backend in [
            KeyboardImplementation::Tauri,
            KeyboardImplementation::HandyKeys,
        ] {
            // Non-Linux keeps the long-standing dynamic behavior on both
            // backends.
            assert!(
                cancel_reconcile_enabled("cancel", false, backend),
                "non-Linux {backend:?} must reconcile"
            );
        }
        // Linux: only HandyKeys; the Tauri plugin's dynamic path stays
        // disabled (the instability the guard exists for).
        assert!(cancel_reconcile_enabled(
            "cancel",
            true,
            KeyboardImplementation::HandyKeys
        ));
        assert!(!cancel_reconcile_enabled(
            "cancel",
            true,
            KeyboardImplementation::Tauri
        ));
        // Only the cancel binding is ever reconciled dynamically.
        assert!(!cancel_reconcile_enabled(
            "transcribe",
            false,
            KeyboardImplementation::HandyKeys
        ));
    }
}
