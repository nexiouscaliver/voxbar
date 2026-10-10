//! Keyboard shortcut management module
//!
//! This module provides a unified interface for keyboard shortcuts with
//! multiple backend implementations:
//!
//! - `tauri`: Uses Tauri's built-in global-shortcut plugin
//! - `handy_keys`: Uses the handy-keys library for more control
//!
//! The active implementation is determined by the `keyboard_implementation`
//! setting and can be changed at runtime.

mod handler;
pub mod handy_keys;
pub mod tauri_impl;

use log::{debug, error, info, warn};
use serde::Serialize;
use specta::Type;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Emitter, Manager};

use crate::llm_client::{PostProcessModelError, TestConnectionResult};
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use crate::settings::APPLE_INTELLIGENCE_DEFAULT_MODEL_ID;
use crate::settings::{
    self, get_settings, AutoSubmitKey, CachedModelList, ChineseScript, ClipboardHandling,
    KeyboardImplementation, LLMPrompt, NumberFormat, OverlayPosition, OverlayStyle, PasteMethod,
    ShortcutActivation, ShortcutBinding, SoundTheme, Theme, TypingTool, UpdatePolicy, VadBackend,
    APPLE_INTELLIGENCE_PROVIDER_ID, LOCAL_LLM_PROVIDER_ID,
};
use crate::tray;

// Note: Commands are accessed via shortcut::handy_keys:: in lib.rs

/// Whether a binding string is a "bare key": a single non-modifier key
/// with no modifier held (for example `z`, or `escape`).
///
/// Global action bindings must never be bare: the hotkey would fire on
/// every press of that key in every app (the shipped `undo = z` case
/// injected Cmd+Z into whatever was focused on every typed z). The one
/// exemption is the `cancel` binding, which is intentionally a bare
/// Escape and only fires while a recording is live.
pub fn is_bare_key_binding(raw: &str) -> bool {
    // `::handy_keys` because this module also declares a `handy_keys`
    // submodule that shadows the extern crate name in scope here.
    match raw.trim().parse::<::handy_keys::Hotkey>() {
        Ok(hotkey) => hotkey.modifiers.is_empty() && hotkey.key.is_some(),
        // Unparseable strings are rejected by the per-implementation
        // validators; they are not this rule's concern.
        Err(_) => false,
    }
}

/// The bare-key rule as an error message, shared by the change-binding
/// rejection and the register-level guards so every surface names the same
/// rule.
pub fn bare_key_rejection(raw: &str) -> String {
    format!(
        "Shortcut '{}' needs at least one modifier key (for example 'option+z', not 'z'): a bare key would fire on every press of that key in any app",
        raw.trim()
    )
}

/// Whether a binding should currently hold a registration: it must be bound
/// to a key, its feature's master toggle (when it has one) must be on, and
/// it must not be a bare key.
///
/// Bindings that ship unbound (the assignable editing actions, command mode)
/// stay unregistered, and therefore inert, until the operator binds a key in
/// Settings even though their master toggles default to on.
///
/// Stored bare-key action bindings (accepted by earlier versions) are
/// treated as unbound here with a warning: safer than firing the action on
/// every press of that key. The value stays in settings so the operator can
/// see it and rebind deliberately; re-recording it through the UI is
/// rejected by `change_binding`.
pub fn binding_is_active(
    settings: &crate::settings::AppSettings,
    id: &str,
    binding: &ShortcutBinding,
) -> bool {
    if binding.current_binding.trim().is_empty() {
        return false;
    }
    if id != "cancel" && is_bare_key_binding(&binding.current_binding) {
        warn!(
            "Binding '{}' ('{}') is a single key with no modifier; treating it as unbound. \
             Rebind it in Settings with at least one modifier key.",
            id, binding.current_binding
        );
        return false;
    }
    match id {
        "transcribe_with_post_process" => settings.post_process_enabled,
        // Same master toggle as the post-process dictation key: cycling
        // templates while the layer is off can never matter, and the
        // unbound default keeps stock installs inert either way.
        "cycle_post_process_prompt" => settings.post_process_enabled,
        "delete_last_word" => settings.delete_last_word_enabled,
        "undo" => settings.undo_enabled,
        "transcribe_commands" => settings.command_mode_enabled,
        _ => true,
    }
}

/// KB-159: whether the chord requested for `id` collides with another
/// binding that both holds the same chord and is currently active (bound
/// plus its feature toggle on). A duplicate whose toggle is off owns no
/// registration, so it does not block the rebind; `id` itself is excluded.
/// Returns the conflicting binding's id so callers can name the owner.
/// KB-194: both change_binding branches run this up front - the cancel
/// branch before its registration swap, the generic branch before the old
/// chord is unregistered (while the recorder is armed the suspend has
/// already cleared every registration, so only this scan can see the
/// conflict; the deferred register/resume failure is debug-level).
fn binding_conflicts_with_active_binding(
    settings: &crate::settings::AppSettings,
    id: &str,
    chord: &str,
) -> Option<String> {
    settings.bindings.iter().find_map(|(other_id, other)| {
        (other_id != id
            && other.current_binding.trim() == chord.trim()
            && binding_is_active(settings, other_id, other))
        .then(|| other_id.clone())
    })
}

/// Initialize shortcuts using the configured implementation
pub fn init_shortcuts(app: &AppHandle) {
    let user_settings = settings::load_or_create_app_settings(app);

    // Check which implementation to use
    match user_settings.keyboard_implementation {
        KeyboardImplementation::Tauri => {
            tauri_impl::init_shortcuts(app);
            // The Tauri backend goes through global-hotkey, which on Linux is
            // X11-only: on a Wayland session hotkeys work solely through
            // XWayland and are dead in native Wayland apps. Say so once per
            // run instead of leaving a hotkey-dead app whose only trace is a
            // log line (the setup instructions for handy_keys live in the
            // Ubuntu troubleshooting guide).
            #[cfg(target_os = "linux")]
            if crate::utils::is_wayland() {
                log::warn!(
                    "Wayland session with the Tauri keyboard backend: global-hotkey is \
                     X11-only, so hotkeys may not reach native Wayland apps. Consider \
                     VoxBar Keys (handy_keys) with /dev/uinput access; see \
                     docs/troubleshooting/ubuntu-26-04-gnome-wayland/README.md"
                );
                crate::managers::transcription::emit_overlay_notice(
                    app,
                    crate::managers::transcription::NoticeCode::WaylandTauriHotkeys,
                    None,
                );
            }
        }
        KeyboardImplementation::HandyKeys => {
            if let Err(e) = handy_keys::init_shortcuts(app) {
                error!("Failed to initialize handy-keys shortcuts: {}", e);
                // Fall back to Tauri implementation and persist this fallback
                warn!("Falling back to Tauri global shortcut implementation and saving fallback to settings");

                // Update settings to persist the fallback so we don't retry HandyKeys on next launch
                let mut settings = settings::get_settings(app);
                settings.keyboard_implementation = KeyboardImplementation::Tauri;
                settings::write_settings(app, settings);

                tauri_impl::init_shortcuts(app);
            }
        }
    }
}

/// Whether the recording lifecycle currently wants the cancel shortcut.
/// Written synchronously by start/stop, so it always holds the latest request.
static CANCEL_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Whether the cancel shortcut is actually registered with the backend.
/// The lock also serializes reconciliation passes. Tracked on every platform
/// now: on Linux the guard is the backend choice (see
/// [`handy_keys::cancel_reconcile_enabled`]), not the platform itself.
static CANCEL_REGISTERED: std::sync::Mutex<bool> = std::sync::Mutex::new(false);

/// Register the cancel shortcut (called when recording starts)
pub fn register_cancel_shortcut(app: &AppHandle) {
    // Track recording lifecycle independently of the current implementation so
    // switching implementations mid-recording cannot leave stale fallback state.
    crate::secure_input::register_cancel_fallback(app);

    CANCEL_REQUESTED.store(true, Ordering::SeqCst);
    schedule_cancel_reconcile(app);
}

/// Unregister the cancel shortcut (called when recording stops)
pub fn unregister_cancel_shortcut(app: &AppHandle) {
    crate::secure_input::unregister_cancel_fallback(app);

    CANCEL_REQUESTED.store(false, Ordering::SeqCst);
    schedule_cancel_reconcile(app);
}

/// Apply the requested cancel state off the calling thread. Start and stop run
/// on the shortcut handler thread, which must not block on registration, so the
/// work is spawned. Spawned tasks may run in any order (a very short recording
/// can see its unregister task run before its register task), so each pass
/// applies the latest request instead of the action that scheduled it.
fn schedule_cancel_reconcile(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        reconcile_cancel_shortcut(&app);
    });
}

fn reconcile_cancel_shortcut(app: &AppHandle) {
    // On Linux the guard is the backend: the Tauri global-shortcut plugin's
    // dynamic registration is the documented instability, so with Tauri
    // active the cancel shortcut stays off (the tray menu remains the abort
    // there). With handy_keys active, registration travels the vendored
    // manager's channel - the same machinery binding changes already use at
    // runtime - so the cancel key arms only while a session is live.
    #[cfg(target_os = "linux")]
    {
        let backend = get_settings(app).keyboard_implementation;
        if !handy_keys::cancel_reconcile_enabled("cancel", true, backend) {
            let mut registered = CANCEL_REGISTERED.lock().unwrap_or_else(|e| e.into_inner());
            // A backend switch away from handy_keys can leave a stale
            // registration flag; the actual shortcut is unregistered by the
            // implementation switch itself.
            *registered = false;
            return;
        }
    }

    {
        let mut registered = CANCEL_REGISTERED.lock().unwrap_or_else(|e| e.into_inner());
        let requested = CANCEL_REQUESTED.load(Ordering::SeqCst);
        if requested == *registered {
            return;
        }

        let Some(cancel_binding) = get_settings(app).bindings.get("cancel").cloned() else {
            return;
        };

        if requested {
            match register_shortcut(app, cancel_binding) {
                Ok(()) => *registered = true,
                Err(e) => error!("Failed to register cancel shortcut: {}", e),
            }
        } else {
            match unregister_shortcut(app, cancel_binding) {
                Ok(()) => *registered = false,
                Err(e) => error!("Failed to unregister cancel shortcut: {}", e),
            }
        }
    }
}

/// KB-036: switching keyboard implementations drops the cancel key's
/// registration (the switch replaces the backend's whole registration table;
/// `unregister_all_shortcuts` and the re-init both skip the "cancel" id as
/// "dynamically registered"), but neither CANCEL_REGISTERED nor a reconcile
/// pass ran on that path - so a recording straddling the switch kept a stale
/// flag and, depending on direction, a dead or orphaned cancel key until
/// something else fired. Reset the flag so the next reconcile re-arms the
/// key under the NEW implementation; the reconcile itself no-ops when no
/// live recording requested it (CANCEL_REQUESTED false).
fn rearm_cancel_after_implementation_switch(app: &AppHandle) {
    *CANCEL_REGISTERED.lock().unwrap_or_else(|e| e.into_inner()) = false;
    schedule_cancel_reconcile(app);
}

/// Register a shortcut using the appropriate implementation
pub fn register_shortcut(app: &AppHandle, binding: ShortcutBinding) -> Result<(), String> {
    let settings = get_settings(app);
    match settings.keyboard_implementation {
        KeyboardImplementation::Tauri => tauri_impl::register_shortcut(app, binding),
        KeyboardImplementation::HandyKeys => handy_keys::register_shortcut(app, binding),
    }
}

/// The registration a cancel rebind must retire: the PREVIOUS binding's
/// hotkey, when it held one. The reconcile pass reads the binding from
/// settings - by then the NEW string - so it can never unregister the old
/// registration itself; without this retirement a mid-session rebind
/// leaves the old key registered (and its key events consumed
/// system-wide) until app restart. None means the previous binding was
/// unbound, so nothing was ever registered for it.
fn cancel_rebind_retirement(previous: &ShortcutBinding) -> Option<ShortcutBinding> {
    (!previous.current_binding.trim().is_empty()).then(|| previous.clone())
}

/// The memory-gate safety margin as storable: 1-4 MB is neither off (0)
/// nor a usable margin, and normalizes to 0 - the same rule the settings
/// loader enforces on stale stored values, applied at the write boundary
/// so a value written here can never be silently rewritten on next load.
fn normalize_headroom_mb(headroom_mb: u64) -> u64 {
    if (1..=4).contains(&headroom_mb) {
        0
    } else {
        headroom_mb
    }
}

/// Unregister a shortcut using the appropriate implementation
pub fn unregister_shortcut(app: &AppHandle, binding: ShortcutBinding) -> Result<(), String> {
    let settings = get_settings(app);
    match settings.keyboard_implementation {
        KeyboardImplementation::Tauri => tauri_impl::unregister_shortcut(app, binding),
        KeyboardImplementation::HandyKeys => handy_keys::unregister_shortcut(app, binding),
    }
}

// ============================================================================
// Binding Management Commands
// ============================================================================

#[derive(Serialize, Type)]
pub struct BindingResponse {
    success: bool,
    binding: Option<ShortcutBinding>,
    error: Option<String>,
}

#[tauri::command]
#[specta::specta]
pub fn change_binding(
    app: AppHandle,
    id: String,
    binding: String,
) -> Result<BindingResponse, String> {
    let mut settings = settings::get_settings(&app);

    // Get the binding to modify, or create it from defaults if it doesn't exist
    let binding_to_modify = match settings.bindings.get(&id) {
        Some(binding) => binding.clone(),
        None => {
            // Try to get the default binding for this id
            let default_settings = settings::get_default_settings();
            match default_settings.bindings.get(&id) {
                Some(default_binding) => {
                    warn!(
                        "Binding '{}' not found in settings, creating from defaults",
                        id
                    );
                    default_binding.clone()
                }
                None => {
                    let error_msg = format!("Binding with id '{}' not found in defaults", id);
                    warn!("change_binding error: {}", error_msg);
                    return Ok(BindingResponse {
                        success: false,
                        binding: None,
                        error: Some(error_msg),
                    });
                }
            }
        }
    };

    // Bindings that ship unbound (empty default) may be cleared back to
    // nothing, e.g. by reset_binding; every other shortcut must keep a value.
    let optional_binding = binding_to_modify.default_binding.trim().is_empty();
    if binding.trim().is_empty() && !optional_binding {
        return Err("Binding cannot be empty".to_string());
    }

    // Bare-key rule: a global action on a single unmodified key fires on
    // every press of that key in every app. The cancel binding is exempt
    // (bare Escape by design, only armed while recording).
    if id != "cancel" && is_bare_key_binding(&binding) {
        return Err(bare_key_rejection(&binding));
    }

    // If this is the cancel binding, update the settings and reconcile the
    // dynamic registration. The cancel key is armed only while a recording
    // is live, so a mid-session rebind must swap the armed hotkey: the
    // registration under the previous string is dropped here (the reconcile
    // below only knows the new string) and, if a recording is still live,
    // re-armed under the new one. Skipping the drop would leave the old key
    // registered and consumed system-wide until app restart.
    if id == "cancel" {
        if let Some(mut b) = settings.bindings.get(&id).cloned() {
            // Validate before any registration is touched, so a bad string
            // cannot strip the cancel key from a live recording.
            if let Err(e) =
                validate_shortcut_for_implementation(&binding, settings.keyboard_implementation)
            {
                warn!("change_binding validation error: {}", e);
                return Err(e);
            }

            // KB-159: reject a chord another ACTIVE binding owns before
            // anything is touched. Below, the previous registration is
            // retired and success is returned BEFORE the reconcile actually
            // registers the new chord - a conflict made that deferred
            // register fail (log-only), leaving the recording with a dead
            // cancel key while Settings reported the rebind as saved.
            if let Some(owner) = binding_conflicts_with_active_binding(&settings, &id, &binding) {
                let error_msg = format!(
                    "Shortcut '{}' is already in use by the '{}' action: pick a different combination",
                    binding.trim(),
                    owner
                );
                warn!("change_binding conflict error: {}", error_msg);
                return Err(error_msg);
            }

            let previous = b.clone();
            b.current_binding = binding;
            settings.bindings.insert(id.clone(), b.clone());
            settings::write_settings(&app, settings);

            if let Some(retired) = cancel_rebind_retirement(&previous) {
                if let Err(e) = unregister_shortcut(&app, retired) {
                    error!("Failed to unregister previous cancel shortcut: {}", e);
                }
            }
            // Force the next reconcile pass to register rather than assume
            // the (now dropped) registration still satisfies the request.
            *CANCEL_REGISTERED.lock().unwrap_or_else(|e| e.into_inner()) = false;

            crate::secure_input::reconcile_fallback(&app);
            schedule_cancel_reconcile(&app);
            return Ok(BindingResponse {
                success: true,
                binding: Some(b.clone()),
                error: None,
            });
        }
    }

    // KB-194: the same chord-conflict pre-check the cancel branch runs
    // (KB-159), here in the generic branch and BEFORE the old registration
    // is dropped. While the recorder UI is armed, suspend_all_shortcuts has
    // already unregistered everything, so the register below commits onto a
    // chord another ACTIVE binding still owns in settings without a peep -
    // the duplicate only bites at resume, as a debug-level failure: one of
    // the pair is silently dead, chosen by map iteration order. Refusing up
    // front names the owner instead. An empty target is an unbind and never
    // conflicts; self rebinds and toggle-off duplicates pass (the helper's
    // rules).
    if !binding.trim().is_empty() {
        if let Some(owner) = binding_conflicts_with_active_binding(&settings, &id, &binding) {
            let error_msg = format!(
                "Shortcut '{}' is already in use by the '{}' action: pick a different combination",
                binding.trim(),
                owner
            );
            warn!("change_binding conflict error: {}", error_msg);
            return Err(error_msg);
        }
    }

    // Unregister the existing binding (nothing to do when it was unbound)
    if !binding_to_modify.current_binding.trim().is_empty() {
        if let Err(e) = unregister_shortcut(&app, binding_to_modify.clone()) {
            let error_msg = format!("Failed to unregister shortcut: {}", e);
            error!("change_binding error: {}", error_msg);
        }
    }

    // Validate and register the new binding. An empty target means "unbind":
    // skip both steps and simply persist the cleared value.
    if !binding.trim().is_empty() {
        // Validate the new shortcut for the current keyboard implementation
        if let Err(e) =
            validate_shortcut_for_implementation(&binding, settings.keyboard_implementation)
        {
            warn!("change_binding validation error: {}", e);
            restore_registration(&app, &binding_to_modify);
            return Err(e);
        }

        // Register the new binding. KB-009: only when the POST-update state
        // leaves this binding active - binding_is_active folds in the
        // feature's master toggle, and a toggle-off binding that still holds
        // a registration is a system-swallowed no-op (its handler checks
        // the toggle). The string persists regardless, so flipping the
        // toggle on later registers the chord (the toggle commands do
        // exactly that).
        let mut candidate = binding_to_modify.clone();
        candidate.current_binding = binding.clone();
        let mut post_update = settings.clone();
        post_update.bindings.insert(id.clone(), candidate.clone());
        if binding_is_active(&post_update, &id, &candidate) {
            if let Err(e) = register_shortcut(&app, candidate) {
                let error_msg = format!("Failed to register shortcut: {}", e);
                error!("change_binding error: {}", error_msg);
                restore_registration(&app, &binding_to_modify);
                return Ok(BindingResponse {
                    success: false,
                    binding: None,
                    error: Some(error_msg),
                });
            }
        }
    }

    // Update the binding in the settings
    let mut updated_binding = binding_to_modify.clone();
    updated_binding.current_binding = binding;
    settings.bindings.insert(id, updated_binding.clone());

    // Save the settings and synchronize any active Secure Input shadows.
    settings::write_settings(&app, settings);
    crate::secure_input::reconcile_fallback(&app);

    // Return the updated binding
    Ok(BindingResponse {
        success: true,
        binding: Some(updated_binding),
        error: None,
    })
}

/// Best-effort re-register of the previous binding after a failed change,
/// so a failure leaves the user's shortcut working exactly as before.
fn restore_registration(app: &AppHandle, binding: &ShortcutBinding) {
    if !restore_registration_is_due(&get_settings(app), binding) {
        return;
    }
    if let Err(e) = register_shortcut(app, binding.clone()) {
        error!(
            "Failed to restore previous binding '{}' ({}): {}",
            binding.id, binding.current_binding, e
        );
    }
}

/// KB-212: whether a failed change should re-register the previous chord.
/// The register that failed was KB-009-gated (it only runs for a binding
/// the POST-update state leaves active), so a previous binding whose own
/// feature toggle was off held NO registration to begin with - restoring
/// its chord anyway would resurrect exactly the system-swallowed
/// registration the normal path refuses to create. The settings passed in
/// are still the pre-failure state: both restore_registration call sites
/// run before the change is persisted.
fn restore_registration_is_due(
    settings: &crate::settings::AppSettings,
    binding: &ShortcutBinding,
) -> bool {
    !binding.current_binding.trim().is_empty() && binding_is_active(settings, &binding.id, binding)
}

#[tauri::command]
#[specta::specta]
pub fn reset_binding(app: AppHandle, id: String) -> Result<BindingResponse, String> {
    let binding = settings::get_stored_binding(&settings::get_settings(&app), &id)?;
    change_binding(app, id, binding.default_binding)
}

/// Unregister every binding while the user is recording a new shortcut in
/// the UI, so no existing shortcut can fire (or swallow the keystrokes)
/// mid-capture. The "cancel" binding is untouched: it is managed dynamically
/// by the recording lifecycle.
pub fn suspend_all_shortcuts(app: &AppHandle) {
    for (id, binding) in settings::get_bindings(app) {
        if id == "cancel" {
            continue;
        }
        // Nothing registered for unbound bindings; unregistering an empty
        // chord would only log a parse error.
        if binding.current_binding.trim().is_empty() {
            continue;
        }
        if let Err(e) = unregister_shortcut(app, binding) {
            debug!(
                "suspend_all_shortcuts: could not unregister '{}': {}",
                id, e
            );
        }
    }
}

/// Re-register every binding from settings after shortcut recording ends.
/// Registering an already-registered shortcut fails cleanly in both
/// implementations, so this is idempotent and safe on every exit path.
pub fn resume_all_shortcuts(app: &AppHandle) {
    let settings = get_settings(app);
    for (id, binding) in &settings.bindings {
        if id == "cancel" {
            continue;
        }
        if !binding_is_active(&settings, id, binding) {
            continue;
        }
        if let Err(e) = register_shortcut(app, binding.clone()) {
            debug!("resume_all_shortcuts: could not register '{}': {}", id, e);
        }
    }
}

/// Temporarily unregister all bindings while the user is recording a
/// shortcut in the UI. This avoids firing actions while keys are recorded.
#[tauri::command]
#[specta::specta]
pub fn suspend_all_bindings(app: AppHandle) -> Result<(), String> {
    suspend_all_shortcuts(&app);
    Ok(())
}

/// Re-register all bindings after the user has finished recording.
#[tauri::command]
#[specta::specta]
pub fn resume_all_bindings(app: AppHandle) -> Result<(), String> {
    resume_all_shortcuts(&app);
    Ok(())
}

// ============================================================================
// Keyboard Implementation Switching
// ============================================================================

/// Result of changing keyboard implementation
#[derive(Serialize, Type)]
pub struct ImplementationChangeResult {
    pub success: bool,
    /// List of binding IDs that were reset to defaults due to incompatibility
    pub reset_bindings: Vec<String>,
}

/// Change the keyboard implementation with runtime switching.
/// This will unregister all shortcuts from the old implementation,
/// validate shortcuts for the new implementation (resetting invalid ones to defaults),
/// and register them with the new implementation.
#[tauri::command]
#[specta::specta]
pub fn change_keyboard_implementation_setting(
    app: AppHandle,
    implementation: String,
) -> Result<ImplementationChangeResult, String> {
    let current_settings = settings::get_settings(&app);
    let current_impl = current_settings.keyboard_implementation;
    let new_impl = parse_keyboard_implementation(&implementation);

    // If same implementation, nothing to do
    if current_impl == new_impl {
        return Ok(ImplementationChangeResult {
            success: true,
            reset_bindings: vec![],
        });
    }

    info!(
        "Switching keyboard implementation from {:?} to {:?}",
        current_impl, new_impl
    );

    // Unregister all shortcuts from the current implementation
    unregister_all_shortcuts(&app, current_impl);

    // Update the setting
    let mut settings = settings::get_settings(&app);
    settings.keyboard_implementation = new_impl;
    settings::write_settings(&app, settings);

    // Carbon fallback registrations use the Tauri plugin. Remove them before
    // registering the full Tauri implementation to avoid duplicate conflicts.
    if new_impl == KeyboardImplementation::Tauri {
        crate::secure_input::reconcile_fallback(&app);
    }

    // Initialize new implementation if needed (HandyKeys needs state)
    if new_impl == KeyboardImplementation::HandyKeys && initialize_handy_keys_with_rollback(&app)? {
        // Shortcuts already registered during init.
        crate::secure_input::reconcile_fallback(&app);
        rearm_cancel_after_implementation_switch(&app);
        return Ok(ImplementationChangeResult {
            success: true,
            reset_bindings: vec![],
        });
    }

    // Register all shortcuts with new implementation, resetting invalid ones
    let reset_bindings = register_all_shortcuts_for_implementation(&app, new_impl);
    crate::secure_input::reconcile_fallback(&app);
    rearm_cancel_after_implementation_switch(&app);

    // Emit event to notify frontend of the change
    let _ = app.emit(
        "settings-changed",
        serde_json::json!({
            "setting": "keyboard_implementation",
            "value": implementation,
            "reset_bindings": reset_bindings
        }),
    );

    info!("Keyboard implementation switched to {:?}", new_impl);

    Ok(ImplementationChangeResult {
        success: true,
        reset_bindings,
    })
}

/// Get the current keyboard implementation
#[tauri::command]
#[specta::specta]
pub fn get_keyboard_implementation(app: AppHandle) -> String {
    let settings = settings::get_settings(&app);
    match settings.keyboard_implementation {
        KeyboardImplementation::Tauri => "tauri".to_string(),
        KeyboardImplementation::HandyKeys => "handy_keys".to_string(),
    }
}

// ============================================================================
// Validation Helpers
// ============================================================================

/// Validate a shortcut for a specific implementation
fn validate_shortcut_for_implementation(
    raw: &str,
    implementation: KeyboardImplementation,
) -> Result<(), String> {
    match implementation {
        KeyboardImplementation::Tauri => tauri_impl::validate_shortcut(raw),
        KeyboardImplementation::HandyKeys => handy_keys::validate_shortcut(raw),
    }
}

/// Parse a keyboard implementation string into the enum
fn parse_keyboard_implementation(s: &str) -> KeyboardImplementation {
    match s {
        "tauri" => KeyboardImplementation::Tauri,
        "handy_keys" => KeyboardImplementation::HandyKeys,
        other => {
            warn!(
                "Invalid keyboard implementation '{}', defaulting to tauri",
                other
            );
            KeyboardImplementation::Tauri
        }
    }
}

/// Unregister all shortcuts for the current implementation
fn unregister_all_shortcuts(app: &AppHandle, implementation: KeyboardImplementation) {
    let bindings = settings::get_bindings(app);

    for (id, binding) in &bindings {
        // Skip cancel shortcut as it's dynamically registered
        if id == "cancel" {
            continue;
        }
        // Nothing to unregister for a binding that holds no key.
        if binding.current_binding.trim().is_empty() {
            continue;
        }

        let result = match implementation {
            KeyboardImplementation::Tauri => tauri_impl::unregister_shortcut(app, binding.clone()),
            KeyboardImplementation::HandyKeys => {
                handy_keys::unregister_shortcut(app, binding.clone())
            }
        };

        if let Err(e) = result {
            warn!(
                "Failed to unregister shortcut '{}' during switch: {}",
                id, e
            );
        }
    }

    // The cancel shortcut is dynamically armed, so an implementation switch
    // mid-session would otherwise leave its registration behind in the
    // backend being torn down (unblockable by the new backend, e.g. a
    // handy_keys cancel registration after a rollback to Tauri on Linux).
    // Tear it down through the OLD implementation; the next reconcile pass
    // re-arms it under the new one wherever that is allowed.
    if CANCEL_REQUESTED.load(Ordering::SeqCst) {
        if let Some(cancel_binding) = bindings.get("cancel").cloned() {
            let result = match implementation {
                KeyboardImplementation::Tauri => {
                    tauri_impl::unregister_shortcut(app, cancel_binding)
                }
                KeyboardImplementation::HandyKeys => {
                    handy_keys::unregister_shortcut(app, cancel_binding)
                }
            };
            if let Err(e) = result {
                warn!("Failed to unregister cancel shortcut during switch: {}", e);
            }
        }
        if let Ok(mut registered) = CANCEL_REGISTERED.lock() {
            *registered = false;
        }
    }
}

/// Register all shortcuts for a specific implementation, validating and resetting invalid ones
fn register_all_shortcuts_for_implementation(
    app: &AppHandle,
    implementation: KeyboardImplementation,
) -> Vec<String> {
    let mut reset_bindings = Vec::new();
    let default_bindings = settings::get_default_settings().bindings;
    let mut current_settings = settings::get_settings(app);

    for (id, default_binding) in &default_bindings {
        // Skip cancel shortcut as it's dynamically registered
        if id == "cancel" {
            continue;
        }

        let mut binding = current_settings
            .bindings
            .get(id)
            .cloned()
            .unwrap_or_else(|| default_binding.clone());

        // Unbound or toggle-disabled bindings hold no registration.
        if !binding_is_active(&current_settings, id, &binding) {
            continue;
        }

        // Validate the shortcut for the target implementation
        if let Err(e) =
            validate_shortcut_for_implementation(&binding.current_binding, implementation)
        {
            info!(
                "Shortcut '{}' ({}) is invalid for {:?}: {}. Resetting to default.",
                id, binding.current_binding, implementation, e
            );

            // Reset to default
            binding.current_binding = default_binding.current_binding.clone();
            current_settings
                .bindings
                .insert(id.clone(), binding.clone());
            reset_bindings.push(id.clone());
        }

        // Register with the appropriate implementation
        let result = match implementation {
            KeyboardImplementation::Tauri => tauri_impl::register_shortcut(app, binding),
            KeyboardImplementation::HandyKeys => handy_keys::register_shortcut(app, binding),
        };

        if let Err(e) = result {
            error!(
                "Failed to register shortcut '{}' for {:?}: {}",
                id, implementation, e
            );
        }
    }

    // Save settings if any bindings were reset
    if !reset_bindings.is_empty() {
        settings::write_settings(app, current_settings);
    }

    reset_bindings
}

/// Initialize HandyKeys if not already initialized, with rollback on failure
fn initialize_handy_keys_with_rollback(app: &AppHandle) -> Result<bool, String> {
    if app.try_state::<handy_keys::HandyKeysState>().is_some() {
        return Ok(false); // Already initialized, caller should continue
    }

    if let Err(e) = handy_keys::init_shortcuts(app) {
        error!("Failed to initialize VoxBar Keys: {}", e);
        // Rollback to Tauri
        let mut settings = settings::get_settings(app);
        settings.keyboard_implementation = KeyboardImplementation::Tauri;
        settings::write_settings(app, settings);
        crate::secure_input::reconcile_fallback(app);
        tauri_impl::init_shortcuts(app);
        // KB-199: this Err is the switch command's ONLY error exit, and the
        // `?` on it short-circuits past both rearm_cancel_after_implementation_switch
        // call sites - so a failed handy-keys init mid-recording left the
        // cancel key dead under the rolled-back Tauri backend until
        // something else fired a reconcile. The CANCEL_REGISTERED flag was
        // already reset by unregister_all_shortcuts; a bare reconcile
        // schedule re-arms the key wherever a recording still wants it.
        schedule_cancel_reconcile(app);
        return Err(format!(
            "Failed to initialize VoxBar Keys: {}. Reverted to Tauri.",
            e
        ));
    }

    // init_shortcuts already registered shortcuts
    Ok(true)
}

// ============================================================================
// General Settings Commands
// ============================================================================

#[tauri::command]
#[specta::specta]
pub fn change_shortcut_activation_setting(
    app: AppHandle,
    activation: ShortcutActivation,
) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.shortcut_activation = activation;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_hold_threshold_ms_setting(app: AppHandle, ms: u64) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.hold_threshold_ms = ms;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_audio_feedback_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.audio_feedback = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_memory_pressure_guard_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.memory_pressure_guard = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

/// Persist the memory-gate safety margin (Advanced settings). Mirrors the
/// store-side load guard: margins of 1-4 MB are invalid (neither off nor a
/// usable margin) and normalize to 0, so a value written here can never be
/// silently rewritten on the next load. The UI already rejects 1-4; this is
/// the same rule enforced at the write boundary for any other caller.
#[tauri::command]
#[specta::specta]
pub fn change_memory_gate_headroom_setting(app: AppHandle, headroom_mb: u64) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.memory_gate_headroom_mb = normalize_headroom_mb(headroom_mb);
    settings::write_settings(&app, settings);
    Ok(())
}

/// Companion devices master toggle (OFF by default). Applies the runtime
/// change before persisting it (the update_microphone_mode rule, KB-027):
/// enabling starts the companion server and disabling stops it (finalizing
/// a live phone session first); a failed start throws so the settings store
/// rolls the toggle back instead of persisting an enabled state with no
/// server behind it.
#[tauri::command]
#[specta::specta]
pub fn change_companion_devices_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    crate::companion::apply_enabled(&app, enabled)?;
    let mut settings = settings::get_settings(&app);
    settings.companion_devices_enabled = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

/// Issue a fresh pairing token (rebound to the current LAN /24) and
/// restart the server if it is running, so the QR changes and phones
/// holding the old token are refused.
#[tauri::command]
#[specta::specta]
pub fn reset_companion_pairing(app: AppHandle) -> Result<(), String> {
    let manager = app
        .state::<std::sync::Arc<crate::companion::CompanionManager>>()
        .inner()
        .clone();
    manager.reset_pairing(&app)
}

/// Snapshot for the settings panel: QR (SVG), URL, port, fingerprint,
/// connected devices, and any server error.
#[tauri::command]
#[specta::specta]
pub fn get_companion_status(app: AppHandle) -> crate::companion::CompanionStatus {
    let manager = app
        .state::<std::sync::Arc<crate::companion::CompanionManager>>()
        .inner()
        .clone();
    manager.status(&app)
}

#[tauri::command]
#[specta::specta]
pub fn change_auto_fallback_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.auto_fallback = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_menu_bar_model_title_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.menu_bar_model_title = enabled;
    settings::write_settings(&app, settings);

    // Apply immediately (mirrors change_show_tray_icon_setting): a tray sync
    // recomputes the desired title, and the applier's applied_title diff
    // clears the displayed title when the setting turned off.
    tray::update_tray_menu(&app);

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_show_history_model_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.show_history_model = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_audio_feedback_volume_setting(app: AppHandle, volume: f32) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.audio_feedback_volume = volume;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_sound_theme_setting(app: AppHandle, theme: String) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    let parsed = match theme.as_str() {
        "marimba" => SoundTheme::Marimba,
        "pop" => SoundTheme::Pop,
        "custom" => SoundTheme::Custom,
        other => {
            warn!("Invalid sound theme '{}', defaulting to marimba", other);
            SoundTheme::Marimba
        }
    };
    settings.sound_theme = parsed;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_theme_setting(app: AppHandle, theme: String) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    let parsed = match theme.as_str() {
        "system" => Theme::System,
        "light" => Theme::Light,
        "dark" => Theme::Dark,
        other => {
            warn!("Invalid theme '{}', defaulting to system", other);
            Theme::System
        }
    };
    settings.theme = parsed;
    settings::write_settings(&app, settings);
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    apply_window_theme(&app, parsed);
    // KB-031: the tray icon is theme-dependent - re-sync so the menu bar
    // picks the icon for the new theme instead of the one chosen the last
    // time the tray was built. Runs after apply_window_theme, which the
    // icon lookup reads the window theme from.
    tray::update_tray_menu(&app);
    // Notify other webviews (the recording overlay) so they re-apply the palette
    // live - they set `data-theme` on their own document and can't see this one.
    let _ = app.emit("theme-changed", parsed);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_accent_color_setting(app: AppHandle, accent: String) -> Result<(), String> {
    let accent = accent.trim().to_lowercase();
    let mut settings = settings::get_settings(&app);
    // The valid id set lives in the frontend (src/lib/utils/accent.ts); the
    // store keeps the raw string and the frontend renders its default accent
    // for unknown ids, so no allowlist is duplicated here.
    settings.accent_color = accent.clone();
    settings::write_settings(&app, settings);
    // Notify other webviews (the recording overlay) so they re-apply the
    // accent live - each window sets the palette override on its own document.
    let _ = app.emit("accent-changed", accent);
    Ok(())
}

/// Applies the appearance setting to the native window chrome (title bar), which
/// CSS `data-theme` cannot reach. `System` clears the override so the window
/// follows the OS. Call this on startup and whenever the setting changes to keep
/// the title bar in sync with the in-app palette.
///
/// On Windows this themes the title bar only. On macOS `set_theme` sets
/// `NSApp.appearance` app-wide, which is what we want here: it darkens the title
/// bar and keeps the overlay in step. Linux is left to `data-theme` alone, since
/// its window theming is backend-dependent and unreliable.
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub fn apply_window_theme(app: &AppHandle, theme: Theme) {
    let window_theme = match theme {
        Theme::System => None,
        Theme::Light => Some(tauri::Theme::Light),
        Theme::Dark => Some(tauri::Theme::Dark),
    };
    if let Some(window) = app.get_webview_window("main") {
        if let Err(e) = window.set_theme(window_theme) {
            warn!("Failed to apply window theme: {}", e);
        }
    }
}

#[tauri::command]
#[specta::specta]
pub fn change_translate_to_english_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.translate_to_english = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_selected_language_setting(app: AppHandle, language: String) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.selected_language = language;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_overlay_position_setting(app: AppHandle, position: String) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    let parsed = match position.as_str() {
        // "none" is retired (visibility is overlay_style now); fold legacy callers
        // onto Bottom rather than warn.
        "none" | "bottom" => OverlayPosition::Bottom,
        "top" => OverlayPosition::Top,
        other => {
            warn!("Invalid overlay position '{}', defaulting to bottom", other);
            OverlayPosition::Bottom
        }
    };
    settings.overlay_position = parsed;
    settings::write_settings(&app, settings);

    // Whether the overlay shows at all is owned by overlay_style now; position
    // only ever toggles Top/Bottom, so the enabled cache is untouched here.
    // Update overlay position without recreating window
    crate::utils::update_overlay_position(&app);

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_overlay_style_setting(app: AppHandle, style: String) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
    let previous_style = settings.overlay_style;
    let parsed = match style.as_str() {
        "none" => OverlayStyle::None,
        "minimal" => OverlayStyle::Minimal,
        "live" => OverlayStyle::Live,
        other => {
            warn!("Invalid overlay style '{}', defaulting to minimal", other);
            OverlayStyle::Minimal
        }
    };
    settings.overlay_style = parsed;
    settings::write_settings(&app, settings);

    // Turning the overlay on, on GNOME Wayland without layer-shell support,
    // silently downgrades to a regular window that steals focus and breaks
    // pastes into other apps (the project's own Ubuntu guide recommends
    // Overlay: None there). Warn once per change through the notice channel;
    // KDE/wlroots compositors have the layer-shell path and stay quiet.
    #[cfg(target_os = "linux")]
    if previous_style == OverlayStyle::None
        && parsed != OverlayStyle::None
        && crate::utils::is_gnome_wayland()
        && !crate::overlay::layer_shell_active()
    {
        log::warn!(
            "Overlay enabled on GNOME Wayland without layer-shell support: the overlay \
             falls back to a regular window that steals focus and breaks pastes. \
             Recommend Overlay: None here (see \
             docs/troubleshooting/ubuntu-26-04-gnome-wayland/README.md)"
        );
        crate::managers::transcription::emit_overlay_notice(
            &app,
            crate::managers::transcription::NoticeCode::GnomeOverlayFallback,
            None,
        );
    }

    // Keep the cached overlay-enabled flag in sync so emit_levels stops (or
    // resumes) emitting on the next audio callback.
    crate::overlay::update_overlay_enabled_cache(parsed != OverlayStyle::None);

    // Reposition in case the window needs to re-center for the new style.
    crate::utils::update_overlay_position(&app);

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_debug_mode_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.debug_mode = enabled;
    settings::write_settings(&app, settings);

    // Keep webview log streaming in sync: the live log viewer only exists in
    // debug mode, so logs are forwarded to the frontend only while it is on.
    crate::WEBVIEW_LOG_STREAMING.store(enabled, std::sync::atomic::Ordering::Relaxed);

    // Emit event to notify frontend of debug mode change
    let _ = app.emit(
        "settings-changed",
        serde_json::json!({
            "setting": "debug_mode",
            "value": enabled
        }),
    );

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_start_hidden_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.start_hidden = enabled;
    settings::write_settings(&app, settings);

    // Notify frontend
    let _ = app.emit(
        "settings-changed",
        serde_json::json!({
            "setting": "start_hidden",
            "value": enabled
        }),
    );

    Ok(())
}

/// Launch-at-login toggle. KB-112: the login-item change is applied BEFORE
/// persisting and its failure propagates (the update_microphone_mode /
/// change_companion_devices_setting rule), so a failed enable rolls the
/// toggle back instead of leaving it on with no login item behind it - the
/// preference would only self-heal on the NEXT launch, and until then the
/// toggle lied about the app starting at login.
#[tauri::command]
#[specta::specta]
pub fn change_autostart_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    crate::autostart::apply_autostart(&app, enabled)?;

    let mut settings = settings::get_settings(&app);
    settings.autostart_enabled = enabled;
    settings::write_settings(&app, settings);

    // Notify frontend
    let _ = app.emit(
        "settings-changed",
        serde_json::json!({
            "setting": "autostart_enabled",
            "value": enabled
        }),
    );

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_update_checks_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    if settings::update_checks_forced_disabled() {
        return Err(
            "Update checks are disabled by system configuration (HANDY_DISABLE_UPDATER)".into(),
        );
    }

    let mut settings = settings::get_settings(&app);
    settings.update_checks_enabled = enabled;
    settings::write_settings(&app, settings);

    // KB-034: the tray's "Check for Updates" item is enabled exactly while
    // this setting is on - re-sync so the tray reflects the toggle now
    // instead of after the next restart.
    tray::update_tray_menu(&app);

    let _ = app.emit(
        "settings-changed",
        serde_json::json!({
            "setting": "update_checks_enabled",
            "value": enabled
        }),
    );

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_show_whats_new_on_update_setting(
    app: AppHandle,
    enabled: bool,
) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.show_whats_new_on_update = enabled;
    settings::write_settings(&app, settings);

    let _ = app.emit(
        "settings-changed",
        serde_json::json!({
            "setting": "show_whats_new_on_update",
            "value": enabled
        }),
    );

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_whats_new_last_seen_version_setting(
    app: AppHandle,
    version: String,
) -> Result<(), String> {
    let version = version.trim().to_string();
    let mut settings = settings::get_settings(&app);
    settings.whats_new_last_seen_version = version.clone();
    settings::write_settings(&app, settings);

    let _ = app.emit(
        "settings-changed",
        serde_json::json!({
            "setting": "whats_new_last_seen_version",
            "value": version
        }),
    );

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn update_custom_words(app: AppHandle, words: Vec<String>) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.custom_words = words;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_word_correction_threshold_setting(
    app: AppHandle,
    threshold: f64,
) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.word_correction_threshold = threshold;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_extra_recording_buffer_setting(app: AppHandle, ms: u64) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.extra_recording_buffer_ms = ms;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_streaming_release_tail_setting(app: AppHandle, ms: u64) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.streaming_release_tail_ms = ms;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_paste_delay_ms_setting(app: AppHandle, ms: u64) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.paste_delay_ms = ms;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_paste_delay_after_ms_setting(app: AppHandle, ms: u64) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.paste_delay_after_ms = ms;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_reliable_paste_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.reliable_paste = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_paste_method_setting(app: AppHandle, method: String) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    let parsed = match method.as_str() {
        "ctrl_v" => PasteMethod::CtrlV,
        "direct" => PasteMethod::Direct,
        "none" => PasteMethod::None,
        "shift_insert" => PasteMethod::ShiftInsert,
        "ctrl_shift_v" => PasteMethod::CtrlShiftV,
        "external_script" => PasteMethod::ExternalScript,
        other => {
            warn!("Invalid paste method '{}', defaulting to ctrl_v", other);
            PasteMethod::CtrlV
        }
    };
    settings.paste_method = parsed;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn get_available_typing_tools() -> Vec<String> {
    #[cfg(target_os = "linux")]
    {
        crate::clipboard::get_available_typing_tools()
    }
    #[cfg(not(target_os = "linux"))]
    {
        vec!["auto".to_string()]
    }
}

#[tauri::command]
#[specta::specta]
pub fn change_typing_tool_setting(app: AppHandle, tool: String) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    let parsed = match tool.as_str() {
        "auto" => TypingTool::Auto,
        "wtype" => TypingTool::Wtype,
        "kwtype" => TypingTool::Kwtype,
        "dotool" => TypingTool::Dotool,
        "ydotool" => TypingTool::Ydotool,
        "xdotool" => TypingTool::Xdotool,
        other => {
            warn!("Invalid typing tool '{}', defaulting to auto", other);
            TypingTool::Auto
        }
    };
    settings.typing_tool = parsed;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_external_script_path_setting(
    app: AppHandle,
    path: Option<String>,
) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.external_script_path = path;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_clipboard_handling_setting(app: AppHandle, handling: String) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    let parsed = match handling.as_str() {
        "dont_modify" => ClipboardHandling::DontModify,
        "copy_to_clipboard" => ClipboardHandling::CopyToClipboard,
        other => {
            warn!(
                "Invalid clipboard handling '{}', defaulting to dont_modify",
                other
            );
            ClipboardHandling::DontModify
        }
    };
    settings.clipboard_handling = parsed;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_auto_submit_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.auto_submit = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_auto_submit_key_setting(app: AppHandle, key: String) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    let parsed = match key.as_str() {
        "enter" => AutoSubmitKey::Enter,
        "ctrl_enter" => AutoSubmitKey::CtrlEnter,
        "cmd_enter" => AutoSubmitKey::CmdEnter,
        other => {
            warn!("Invalid auto submit key '{}', defaulting to enter", other);
            AutoSubmitKey::Enter
        }
    };
    settings.auto_submit_key = parsed;
    settings::write_settings(&app, settings);
    Ok(())
}

/// The two bindings that ride the post-process master toggle, in the order
/// the toggle arms them (the dictation key first, the cycle key second).
/// KB-008: the template-cycle key is gated on the same toggle as the
/// post-process dictation key (binding_is_active), so one toggle drives
/// both registrations - otherwise a bound cycle key stays system-swallowed
/// after the toggle goes off while its handler refuses to fire.
const POST_PROCESS_TOGGLE_BINDINGS: [&str; 2] =
    ["transcribe_with_post_process", "cycle_post_process_prompt"];

#[tauri::command]
#[specta::specta]
pub fn change_post_process_enabled_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    // KB-198: apply-then-persist (the KB-027 rule). While ENABLING, the
    // register attempts run BEFORE the toggle is persisted and a failure
    // propagates (Err rolls the settings store's toggle back) instead of
    // being discarded with `let _ =`: a chord duplicated onto a binding
    // whose toggle is off makes the register fail "already in use", and
    // the discarded shape left the toggle reading on with a dead key
    // behind it. The DISABLED direction stays infallible - unregistering
    // is best-effort teardown and must never trap the toggle off.
    let mut settings = settings::get_settings(&app);
    settings.post_process_enabled = enabled;

    if enabled {
        // KB-009: attempt a registration only for a binding the
        // POST-update state leaves active (folds in bound, not a stored
        // bare key, and the now-on toggle) - exactly the set init and
        // resume_all_shortcuts register.
        for id in POST_PROCESS_TOGGLE_BINDINGS {
            if let Some(binding) = settings.bindings.get(id).cloned() {
                if !binding_is_active(&settings, id, &binding) {
                    continue;
                }
                if let Err(e) = register_shortcut(&app, binding) {
                    // A failure after an earlier binding registered rolls
                    // that registration back, restoring the pre-toggle
                    // state (the toggle was off, so nothing was registered)
                    // before the error surfaces.
                    for rollback_id in POST_PROCESS_TOGGLE_BINDINGS {
                        if let Some(b) = settings.bindings.get(rollback_id).cloned() {
                            if !b.current_binding.trim().is_empty() {
                                let _ = unregister_shortcut(&app, b);
                            }
                        }
                    }
                    return Err(e);
                }
            }
        }
    } else {
        for id in POST_PROCESS_TOGGLE_BINDINGS {
            if let Some(binding) = settings.bindings.get(id).cloned() {
                let _ = unregister_shortcut(&app, binding);
            }
        }
    }

    settings::write_settings(&app, settings);

    // KB-188: the tray's Post-process Prompt submenu is enabled exactly
    // while this setting is on - re-sync so the tray reflects the toggle
    // now instead of after the next unrelated rebuild (the same rule the
    // update-checks toggle follows, KB-034).
    tray::update_tray_menu(&app);

    crate::secure_input::reconcile_fallback(&app);
    Ok(())
}

/// The Experimental master switch. KB-183/KB-145: it is also a kill switch
/// for the companion LAN server, whose ONLY UI control is unmounted while
/// experimental is off - so turning it OFF must stop a running server
/// (apply_enabled(false) finalizes a live phone session first), and turning
/// it back ON re-arms the server only when the stored companion toggle is
/// on, mirroring companion::init. The runtime change runs BEFORE persisting
/// (the update_microphone_mode rule); a failed re-arm is logged and badged
/// inside apply_enabled and does not veto the toggle, which gates more than
/// companion.
#[tauri::command]
#[specta::specta]
pub fn change_experimental_enabled_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    if enabled {
        if settings::get_settings(&app).companion_devices_enabled {
            // Best effort, exactly like init: a failed start is logged and
            // badged inside apply_enabled and surfaces on the companion
            // panel this toggle just re-mounted.
            let _ = crate::companion::apply_enabled(&app, true);
        }
    } else {
        crate::companion::apply_enabled(&app, false)?;
    }

    let mut settings = settings::get_settings(&app);
    settings.experimental_enabled = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_post_process_base_url_setting(
    app: AppHandle,
    provider_id: String,
    base_url: String,
) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    let label = settings
        .post_process_provider(&provider_id)
        .map(|provider| provider.label.clone())
        .ok_or_else(|| format!("Provider '{}' not found", provider_id))?;

    let provider = settings
        .post_process_provider_mut(&provider_id)
        .expect("Provider looked up above must exist");

    if provider.id != "custom" {
        return Err(format!(
            "Provider '{}' does not allow editing the base URL",
            label
        ));
    }

    provider.base_url = base_url;
    // A different endpoint serves a different model list: drop the cached
    // list for this provider so the dropdown never shows the old endpoint's
    // models after a base URL change.
    settings.post_process_model_lists.remove(&provider_id);
    settings::write_settings(&app, settings);
    Ok(())
}

/// Generic helper to validate provider exists
fn validate_provider_exists(
    settings: &settings::AppSettings,
    provider_id: &str,
) -> Result<(), String> {
    if !settings
        .post_process_providers
        .iter()
        .any(|provider| provider.id == provider_id)
    {
        return Err(format!("Provider '{}' not found", provider_id));
    }
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_post_process_api_key_setting(
    app: AppHandle,
    provider_id: String,
    api_key: String,
) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    validate_provider_exists(&settings, &provider_id)?;
    // KB-107: a different key can be authorized for a different model set,
    // and the old key's cached list can be stale or refused - drop the
    // cached list (like the base-URL handler does) so the dropdown
    // refetches with the new key instead of re-hydrating the old one.
    settings.post_process_model_lists.remove(&provider_id);
    settings.post_process_api_keys.insert(provider_id, api_key);
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_post_process_model_setting(
    app: AppHandle,
    provider_id: String,
    model: String,
) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    validate_provider_exists(&settings, &provider_id)?;
    settings.post_process_models.insert(provider_id, model);
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn set_post_process_provider(app: AppHandle, provider_id: String) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    validate_provider_exists(&settings, &provider_id)?;
    settings.post_process_provider_id = provider_id;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn add_post_process_prompt(
    app: AppHandle,
    name: String,
    prompt: String,
) -> Result<LLMPrompt, String> {
    let mut settings = settings::get_settings(&app);

    // Generate unique ID using timestamp and random component
    let id = format!("prompt_{}", chrono::Utc::now().timestamp_millis());

    // A user creation: never a builtin, first version of its own line.
    let new_prompt = LLMPrompt {
        id: id.clone(),
        name,
        prompt,
        language: "auto".to_string(),
        register: crate::settings::PromptRegister::General,
        description: String::new(),
        is_builtin: false,
        version: 1,
    };

    settings.post_process_prompts.push(new_prompt.clone());
    settings::write_settings(&app, settings);

    Ok(new_prompt)
}

/// Duplicate one template from the library: a fresh id, " copy" appended to
/// the name, `is_builtin` cleared (a duplicate is the operator's own even
/// when its source is a seed), and version restarted at 1. The source's
/// language/register/description ride along so the copy lands in the same
/// catalog bucket.
#[tauri::command]
#[specta::specta]
pub fn duplicate_post_process_prompt(app: AppHandle, id: String) -> Result<LLMPrompt, String> {
    let mut settings = settings::get_settings(&app);
    let new_id = format!("prompt_{}", chrono::Utc::now().timestamp_millis());
    let duplicate = duplicate_prompt_in_settings(&mut settings, &id, new_id)
        .ok_or_else(|| format!("Prompt with id '{}' not found", id))?;
    settings::write_settings(&app, settings);
    Ok(duplicate)
}

/// Pure core of [`duplicate_post_process_prompt`]: clone the source entry
/// under a fresh id with the copy markers applied, and append it. Unit
/// tested without an app handle.
pub(crate) fn duplicate_prompt_in_settings(
    settings: &mut crate::settings::AppSettings,
    id: &str,
    new_id: String,
) -> Option<LLMPrompt> {
    let source = settings
        .post_process_prompts
        .iter()
        .find(|p| p.id == id)?
        .clone();
    let duplicate = LLMPrompt {
        id: new_id,
        name: format!("{} copy", source.name),
        prompt: source.prompt,
        language: source.language,
        register: source.register,
        description: source.description,
        is_builtin: false,
        version: 1,
    };
    settings.post_process_prompts.push(duplicate.clone());
    Some(duplicate)
}

/// Advance `post_process_selected_prompt_id` to the next template in the
/// library (catalog order, wrapping around). Refuses when fewer than two
/// templates exist; an empty/unresolvable selection lands on the first
/// template. Pure, so the cycling semantics are unit-testable without an
/// app; the command and the hotkey action share it.
pub(crate) fn cycle_prompt_selection(
    settings: &mut crate::settings::AppSettings,
) -> Result<String, String> {
    if settings.post_process_prompts.len() < 2 {
        return Err("Cannot cycle: fewer than two prompt templates".to_string());
    }
    let current_index = settings
        .post_process_selected_prompt_id
        .as_ref()
        .and_then(|id| {
            settings
                .post_process_prompts
                .iter()
                .position(|p| &p.id == id)
        });
    let next_index = match current_index {
        Some(index) => (index + 1) % settings.post_process_prompts.len(),
        // Nothing selected (or a dangling id): land on the first template.
        None => 0,
    };
    let next_id = settings.post_process_prompts[next_index].id.clone();
    settings.post_process_selected_prompt_id = Some(next_id.clone());
    Ok(next_id)
}

/// Advance the selected template and confirm it: shared by the Tauri
/// command (the tray's future use and any UI surface) and the
/// `cycle_post_process_prompt` hotkey action. Errors surface the refusal
/// (fewer than two templates) to the command caller; the hotkey path
/// logs it.
pub(crate) fn cycle_prompt_and_notify(app: &AppHandle) -> Result<(), String> {
    let mut settings = settings::get_settings(app);
    let next_id = cycle_prompt_selection(&mut settings)?;
    let next_name = settings
        .post_process_prompts
        .iter()
        .find(|p| p.id == next_id)
        .map(|p| p.name.clone())
        .unwrap_or_default();
    settings::write_settings(app, settings);
    crate::managers::transcription::emit_overlay_notice(
        app,
        crate::managers::transcription::NoticeCode::PostProcessPromptCycled,
        Some(next_name),
    );
    tray::update_tray_menu(app);
    Ok(())
}

/// The cycle command (the tray and any future surface share it). Advances
/// the selection and confirms the new template through the overlay notice.
#[tauri::command]
#[specta::specta]
pub fn cycle_post_process_prompt(app: AppHandle) -> Result<(), String> {
    cycle_prompt_and_notify(&app)
}

/// "Test on my last transcript": run one template over the most recent
/// history entry's transcription through the exact engine lifecycle a
/// dictation uses (provider, model, shared validator, pp: record with the
/// `prompt_test` binding marker), WITHOUT pasting anything and WITHOUT
/// writing a history row. Typed errors: `no_history` when nothing exists
/// to test against, `prompt_not_found` for a dangling template id.
#[tauri::command]
#[specta::specta]
pub async fn test_post_process_prompt(
    app: AppHandle,
    history_manager: tauri::State<'_, std::sync::Arc<crate::managers::history::HistoryManager>>,
    prompt_id: String,
) -> Result<crate::actions::PromptTestOutcome, crate::actions::TestPromptError> {
    let settings = settings::get_settings(&app);
    let prompt = settings
        .post_process_prompts
        .iter()
        .find(|p| p.id == prompt_id)
        .cloned()
        .ok_or(crate::actions::TestPromptError::PromptNotFound { id: prompt_id })?;

    // Newest-first, first row: the operator's last transcript.
    let page = history_manager
        .get_history_entries(None, Some(1))
        .await
        .map_err(|e| crate::actions::TestPromptError::Other {
            detail: e.to_string(),
        })?;
    let entry = page
        .entries
        .first()
        .ok_or(crate::actions::TestPromptError::NoHistory)?;

    Ok(
        crate::actions::run_prompt_test(Some(&app), &settings, &prompt, &entry.transcription_text)
            .await,
    )
}

#[tauri::command]
#[specta::specta]
pub fn update_post_process_prompt(
    app: AppHandle,
    id: String,
    name: String,
    prompt: String,
) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);

    if let Some(existing_prompt) = settings
        .post_process_prompts
        .iter_mut()
        .find(|p| p.id == id)
    {
        existing_prompt.name = name;
        existing_prompt.prompt = prompt;
        // Every user edit bumps the template's version; run records stamp
        // it, and the seeding migration never touches a versioned prompt.
        existing_prompt.version = existing_prompt.version.saturating_add(1);
        settings::write_settings(&app, settings);
        Ok(())
    } else {
        Err(format!("Prompt with id '{}' not found", id))
    }
}

#[tauri::command]
#[specta::specta]
pub fn delete_post_process_prompt(app: AppHandle, id: String) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);

    // Don't allow deleting the last prompt
    if settings.post_process_prompts.len() <= 1 {
        return Err("Cannot delete the last prompt".to_string());
    }

    // Find and remove the prompt
    let original_len = settings.post_process_prompts.len();
    settings.post_process_prompts.retain(|p| p.id != id);

    if settings.post_process_prompts.len() == original_len {
        return Err(format!("Prompt with id '{}' not found", id));
    }

    // If the deleted prompt was selected, select the first one or None
    if settings.post_process_selected_prompt_id.as_ref() == Some(&id) {
        settings.post_process_selected_prompt_id =
            settings.post_process_prompts.first().map(|p| p.id.clone());
    }

    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn fetch_post_process_models(
    app: AppHandle,
    provider_id: String,
) -> Result<Vec<String>, PostProcessModelError> {
    let settings = settings::get_settings(&app);

    // Find the provider
    let provider = settings
        .post_process_providers
        .iter()
        .find(|p| p.id == provider_id)
        .ok_or_else(|| PostProcessModelError::Other {
            detail: format!("Provider '{}' not found", provider_id),
        })?;

    if provider.id == APPLE_INTELLIGENCE_PROVIDER_ID {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            return Ok(vec![APPLE_INTELLIGENCE_DEFAULT_MODEL_ID.to_string()]);
        }

        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        {
            return Err(PostProcessModelError::Other {
                detail: "Apple Intelligence is only available on Apple silicon Macs running macOS 15 or later.".to_string(),
            });
        }
    }

    // Get API key
    let api_key = settings
        .post_process_api_keys
        .get(&provider_id)
        .cloned()
        .unwrap_or_default();

    // Skip fetching if no API key for providers that typically need one
    if api_key.trim().is_empty() && provider.id != "custom" {
        return Err(PostProcessModelError::Auth {
            detail: format!(
                "API key is required for {}. Please add an API key to list available models.",
                provider.label
            ),
        });
    }

    let models = crate::llm_client::fetch_models(
        provider,
        api_key,
        settings.post_process_timeout_secs_for(&provider.id),
    )
    .await?;

    // Cache the successful list so reopening the panel is instant and works
    // offline: the frontend store hydrates its dropdown options from this
    // field on load. Failures never reach this write, so a good list is
    // never clobbered by a bad fetch.
    let mut settings = settings::get_settings(&app);
    settings.post_process_model_lists.insert(
        provider_id.clone(),
        CachedModelList {
            models: models.clone(),
            fetched_at_unix: chrono::Utc::now().timestamp(),
        },
    );
    settings::write_settings(&app, settings);

    Ok(models)
}

/// Test Connection: probe the selected provider and return a verdict the
/// settings panel renders (auth ok, latency, model reachable, or the
/// failure class). Cloud providers answer a model-list request and, when a
/// model is configured, a tiny completion on a hard 10 s budget; the local
/// provider's verdict is its selected model's downloaded state (no worker
/// spawn); Apple Intelligence maps to its availability check.
#[tauri::command]
#[specta::specta]
pub async fn test_post_process_connection(
    app: AppHandle,
    provider_id: String,
) -> Result<TestConnectionResult, String> {
    use crate::llm_client::{
        apple_intelligence_connection_verdict, assemble_cloud_verdict,
        local_provider_connection_verdict, probe_completion, CONNECTION_PROBE_TIMEOUT_SECS,
    };

    let settings = settings::get_settings(&app);
    let provider = settings
        .post_process_provider(&provider_id)
        .ok_or_else(|| format!("Provider '{}' not found", provider_id))?;

    // The local engine: the verdict is the selected model's on-disk state.
    // Reading manager state never spawns or loads the worker.
    if provider.id == LOCAL_LLM_PROVIDER_ID {
        let selected = crate::local_llm::manager::selected_llm_model_id(&app);
        let model_manager = app.state::<std::sync::Arc<crate::managers::model::ModelManager>>();
        let (downloaded, downloading) = match model_manager.get_model_info(&selected) {
            Some(info) => (info.is_downloaded, info.is_downloading),
            None => (false, false),
        };
        return Ok(local_provider_connection_verdict(downloaded, downloading));
    }

    if provider.id == APPLE_INTELLIGENCE_PROVIDER_ID {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        let available = crate::apple_intelligence::check_apple_intelligence_availability();
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        let available = false;
        return Ok(apple_intelligence_connection_verdict(available));
    }

    // Cloud providers: a missing key is an auth verdict (not a command
    // error) so the panel can show it exactly like a 401.
    let api_key = settings
        .post_process_api_keys
        .get(&provider_id)
        .cloned()
        .unwrap_or_default();
    if api_key.trim().is_empty() && provider.id != "custom" {
        return Ok(assemble_cloud_verdict(
            Err(PostProcessModelError::Auth {
                detail: format!(
                    "API key is required for {}. Please add an API key to test the connection.",
                    provider.label
                ),
            }),
            None,
        ));
    }

    // (a) the model list: auth, reachability, and the latency the verdict
    // reports. A failure here short-circuits the probe.
    let started = std::time::Instant::now();
    let list_outcome = crate::llm_client::fetch_models(
        provider,
        api_key.clone(),
        settings.post_process_timeout_secs_for(&provider.id),
    )
    .await
    .map(|models| (models.len(), started.elapsed().as_millis() as u64));

    if list_outcome.is_err() {
        // No completion probe after a failed list (an unreachable endpoint
        // would only repeat the failure after another wait).
        return Ok(assemble_cloud_verdict(list_outcome, None));
    }

    // (b) the tiny completion, only when a model is configured to probe.
    let configured_model = settings
        .post_process_models
        .get(&provider_id)
        .map(String::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty());

    let probe_outcome = match configured_model {
        Some(model) => {
            Some(probe_completion(provider, api_key, model, CONNECTION_PROBE_TIMEOUT_SECS).await)
        }
        None => None,
    };

    Ok(assemble_cloud_verdict(list_outcome, probe_outcome))
}

/// Set the keep-warm window for the LOCAL post-process model, in seconds.
/// 0 (the default) is the exclusive swap of v1.3.0 (unload after every
/// generation); up to 600 keeps the worker resident that long after the
/// paste. Out-of-range values are rejected, never clamped.
#[tauri::command]
#[specta::specta]
pub fn change_post_process_local_keep_warm_secs_setting(
    app: AppHandle,
    seconds: u64,
) -> Result<(), String> {
    const KEEP_WARM_MAX_SECONDS: u64 = 600;
    if seconds > KEEP_WARM_MAX_SECONDS {
        return Err(format!(
            "Keep-warm window must be between 0 and {KEEP_WARM_MAX_SECONDS} seconds (got {seconds})"
        ));
    }
    let mut settings = settings::get_settings(&app);
    settings.post_process_local_keep_warm_secs = seconds;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn set_post_process_selected_prompt(app: AppHandle, id: String) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);

    // Verify the prompt exists
    if !settings.post_process_prompts.iter().any(|p| p.id == id) {
        return Err(format!("Prompt with id '{}' not found", id));
    }

    settings.post_process_selected_prompt_id = Some(id);
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_mute_while_recording_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.mute_while_recording = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_append_trailing_space_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.append_trailing_space = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_lazy_stream_close_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.lazy_stream_close = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_vad_enabled_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.vad_enabled = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn change_vad_backend_setting(app: AppHandle, backend: VadBackend) -> Result<(), String> {
    if settings::get_settings(&app).vad_backend == backend {
        return Ok(());
    }

    // Construct/swap the detector and, when necessary, reopen cpal away from
    // the webview thread. Persist only after the runtime change succeeds so a
    // rejected in-progress switch or failed microphone reopen rolls back cleanly.
    let manager = app
        .state::<std::sync::Arc<crate::managers::audio::AudioRecordingManager>>()
        .inner()
        .clone();
    tokio::task::spawn_blocking(move || manager.update_vad_backend(backend))
        .await
        .map_err(|e| format!("audio task join failed: {e}"))?
        .map_err(|e| format!("Failed to update VAD backend: {e}"))?;

    let mut current_settings = settings::get_settings(&app);
    current_settings.vad_backend = backend;
    settings::write_settings(&app, current_settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_filler_word_removal_enabled_setting(
    app: AppHandle,
    enabled: bool,
) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.filler_word_removal_enabled = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_spoken_punctuation_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.spoken_punctuation = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_auto_interpret_commands_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.auto_interpret_commands = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

/// Write the edited command matrix. `Some(entries)` normalizes every
/// phrase and validates the table (empty phrases, over-length phrases,
/// duplicates across commands) before anything is persisted; the error
/// string surfaces in the UI toast. `None` resets to the built-in
/// defaults.
#[tauri::command]
#[specta::specta]
pub fn update_command_matrix(
    app: AppHandle,
    entries: Option<Vec<crate::audio_toolkit::command_matrix::CommandMatrixEntry>>,
) -> Result<(), String> {
    let entries = crate::audio_toolkit::command_matrix::normalize_and_validate_matrix(entries)?;
    let mut settings = settings::get_settings(&app);
    settings.command_phrases = entries;
    settings::write_settings(&app, settings);
    Ok(())
}

/// The built-in default command matrix, read by the Settings editor so
/// the default phrases come from the backend instead of being duplicated
/// in TypeScript.
#[tauri::command]
#[specta::specta]
pub fn get_default_command_matrix() -> Vec<crate::audio_toolkit::command_matrix::CommandMatrixEntry>
{
    crate::audio_toolkit::command_matrix::default_command_matrix()
}

#[tauri::command]
#[specta::specta]
pub fn change_terminal_punctuation_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.terminal_punctuation = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_voice_deletion_commands_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.voice_deletion_commands = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_preview_before_paste_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.preview_before_paste = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

/// Flip the delete-last-word master toggle and register or unregister its
/// binding to match, mirroring how the post-processing toggle drives its
/// shortcut. KB-198: apply-then-persist (the KB-027 rule) - the register
/// attempt runs BEFORE the toggle is persisted and its failure propagates
/// while ENABLING (Err rolls the settings store's toggle back) instead of
/// being discarded with `let _ =` (a chord duplicated onto a toggle-off
/// binding fails "already in use" and left the toggle reading on with a
/// dead key). The DISABLED direction stays infallible: unregistering is
/// best-effort teardown and must never trap the toggle off.
#[tauri::command]
#[specta::specta]
pub fn change_delete_last_word_enabled_setting(
    app: AppHandle,
    enabled: bool,
) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.delete_last_word_enabled = enabled;

    if let Some(binding) = settings.bindings.get("delete_last_word").cloned() {
        if enabled {
            // KB-009: register only a binding the POST-update state leaves
            // active (folds in bound and the stored bare-key rule, not just
            // non-emptiness) - the same set init registers.
            if binding_is_active(&settings, "delete_last_word", &binding) {
                register_shortcut(&app, binding)?;
            }
        } else {
            let _ = unregister_shortcut(&app, binding);
        }
    }

    settings::write_settings(&app, settings);

    crate::secure_input::reconcile_fallback(&app);
    Ok(())
}

/// Flip the undo master toggle and register or unregister its binding to
/// match. KB-198: apply-then-persist (the KB-027 rule) - the register
/// attempt runs BEFORE the toggle is persisted and its failure propagates
/// while ENABLING (Err rolls the settings store's toggle back) instead of
/// being discarded with `let _ =` (a chord duplicated onto a toggle-off
/// binding fails "already in use" and left the toggle reading on with a
/// dead key). The DISABLED direction stays infallible: unregistering is
/// best-effort teardown and must never trap the toggle off.
#[tauri::command]
#[specta::specta]
pub fn change_undo_enabled_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.undo_enabled = enabled;

    if let Some(binding) = settings.bindings.get("undo").cloned() {
        if enabled {
            // KB-009: register only a binding the POST-update state leaves
            // active (folds in bound and the stored bare-key rule, not just
            // non-emptiness) - the same set init registers.
            if binding_is_active(&settings, "undo", &binding) {
                register_shortcut(&app, binding)?;
            }
        } else {
            let _ = unregister_shortcut(&app, binding);
        }
    }

    settings::write_settings(&app, settings);

    crate::secure_input::reconcile_fallback(&app);
    Ok(())
}

/// Flip the command-mode master toggle and register or unregister its
/// modifier binding to match. KB-198: apply-then-persist (the KB-027 rule)
/// - the register attempt runs BEFORE the toggle is persisted and its
/// failure propagates while ENABLING (Err rolls the settings store's
/// toggle back) instead of being discarded with `let _ =` (a chord
/// duplicated onto a toggle-off binding fails "already in use" and left
/// the toggle reading on with a dead key). The DISABLED direction stays
/// infallible: unregistering is best-effort teardown and must never trap
/// the toggle off.
#[tauri::command]
#[specta::specta]
pub fn change_command_mode_enabled_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.command_mode_enabled = enabled;

    if let Some(binding) = settings.bindings.get("transcribe_commands").cloned() {
        if enabled {
            // KB-009: register only a binding the POST-update state leaves
            // active (folds in bound and the stored bare-key rule, not just
            // non-emptiness) - the same set init registers.
            if binding_is_active(&settings, "transcribe_commands", &binding) {
                register_shortcut(&app, binding)?;
            }
        } else {
            let _ = unregister_shortcut(&app, binding);
        }
    }

    settings::write_settings(&app, settings);

    crate::secure_input::reconcile_fallback(&app);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_chinese_script_setting(app: AppHandle, script: ChineseScript) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.chinese_script = script;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_number_format_setting(app: AppHandle, format: NumberFormat) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.number_format = format;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_update_policy_setting(app: AppHandle, policy: UpdatePolicy) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.update_policy = policy;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_app_language_setting(app: AppHandle, language: String) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.app_language = language.clone();
    settings::write_settings(&app, settings);

    // Refresh the tray menu with the new language
    tray::update_tray_menu(&app);

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_show_tray_icon_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.show_tray_icon = enabled;
    settings::write_settings(&app, settings);

    // Apply change immediately
    tray::set_tray_visibility(&app, enabled);

    Ok(())
}

/// Save accelerator settings and make the next model use reload with them.
/// The currently running transcription, if any, keeps its existing engine.
fn save_accelerator_and_reload_next_use(app: &AppHandle, s: settings::AppSettings) {
    settings::write_settings(app, s);

    let tm = app.state::<std::sync::Arc<crate::managers::transcription::TranscriptionManager>>();
    tm.reload_model_on_next_use();
}

#[tauri::command]
#[specta::specta]
pub fn change_transcribe_accelerator_setting(
    app: AppHandle,
    accelerator: settings::TranscribeAcceleratorSetting,
) -> Result<(), String> {
    let mut s = settings::get_settings(&app);
    s.transcribe_accelerator = accelerator;
    save_accelerator_and_reload_next_use(&app, s);
    app.state::<std::sync::Arc<crate::managers::transcription::TranscriptionManager>>()
        .retry_transcribe_gpu("the transcribe.cpp accelerator setting changed");
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_ort_accelerator_setting(
    app: AppHandle,
    accelerator: settings::OrtAcceleratorSetting,
) -> Result<(), String> {
    let mut s = settings::get_settings(&app);
    s.ort_accelerator = accelerator;
    save_accelerator_and_reload_next_use(&app, s);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_transcribe_gpu_device(app: AppHandle, device: Option<String>) -> Result<(), String> {
    let mut s = settings::get_settings(&app);
    s.transcribe_gpu_device = device;
    save_accelerator_and_reload_next_use(&app, s);
    app.state::<std::sync::Arc<crate::managers::transcription::TranscriptionManager>>()
        .retry_transcribe_gpu("the transcribe.cpp GPU device setting changed");
    Ok(())
}

/// Return which accelerators and GPU devices are available for this build.
///
/// First-call cost is dominated by enumerating GPU devices through the
/// transcribe.cpp Metal/Vulkan backend, which loads dynamic libraries and
/// probes hardware. Run it on the blocking pool so the webview thread
/// stays responsive - see also the startup pre-warm in `lib.rs`.
#[tauri::command]
#[specta::specta]
pub async fn get_available_accelerators(
    app: AppHandle,
) -> crate::managers::transcription::AvailableAccelerators {
    // KB-158: a panic or rejection inside the probe task must not take the
    // command down with it - panicking here permanently empties the
    // accelerator dropdowns. Log it and answer with empty lists so the UI
    // renders its fallback instead of hanging on a rejected promise.
    match tauri::async_runtime::spawn_blocking(move || {
        let tm =
            app.state::<std::sync::Arc<crate::managers::transcription::TranscriptionManager>>();
        crate::managers::transcription::get_available_accelerators(&tm)
    })
    .await
    {
        Ok(accelerators) => accelerators,
        Err(err) => {
            error!("get_available_accelerators task failed: {err}");
            crate::managers::transcription::AvailableAccelerators {
                transcribe: vec![],
                ort: vec![],
                gpu_devices: vec![],
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use handy_keys::Hotkey;
    use tauri_plugin_global_shortcut::Shortcut;

    use super::{
        bare_key_rejection, binding_conflicts_with_active_binding, binding_is_active,
        cancel_rebind_retirement, is_bare_key_binding, normalize_headroom_mb,
        restore_registration_is_due,
    };

    #[test]
    fn compound_shortcut_keys_parse_on_both_backends() {
        for key in [
            "scrolllock",
            "capslock",
            "numlock",
            "pageup",
            "pagedown",
            "printscreen",
        ] {
            assert!(key.parse::<Shortcut>().is_ok(), "Tauri rejected {key}");
            assert!(key.parse::<Hotkey>().is_ok(), "HandyKeys rejected {key}");
        }
    }

    /// The bare-key rule: a single non-modifier key with no modifier is
    /// bare; modifier-only and combo bindings are not.
    /// Side-specific modifier wire values round-trip through the hotkey
    /// parser and formatter: the capture UI can store, and settings can
    /// hold, right-side variants alongside the left ones. Control's wire
    /// name is the short `ctrl_*` (matching the operator's stored
    /// `ctrl_left+fn`); `control_*` spellings parse as aliases.
    #[test]
    fn side_specific_modifier_wire_values_round_trip() {
        for raw in [
            "command_right",
            "shift_right",
            "option_right",
            "ctrl_right",
            "command_left",
            "shift_left",
            "option_left",
            "ctrl_left",
        ] {
            let hotkey: Hotkey = raw.parse().unwrap_or_else(|e| panic!("{raw}: {e}"));
            assert_eq!(hotkey.to_handy_string(), raw, "{raw} must round-trip");
            assert!(
                hotkey.is_single_modifier(),
                "{raw} is a single modifier and must be hold-gated"
            );
        }
        // The long control spellings parse to the same binding.
        let alias: Hotkey = "control_right".parse().unwrap();
        assert_eq!(alias.to_handy_string(), "ctrl_right");
    }

    /// Side-strict matching for modifier-only bindings: a command_left
    /// binding must not fire on right command, and vice versa. (The
    /// keycode-to-side mapping itself, including the device-dependent
    /// built-in keyboard codes, is pinned by the vendored crate's
    /// keycode tests.)
    #[test]
    fn modifier_only_matching_is_side_strict() {
        use handy_keys::Modifiers;
        let left: Hotkey = "command_left".parse().unwrap();
        let right: Hotkey = "command_right".parse().unwrap();
        assert!(left.modifiers.matches(Modifiers::CMD_LEFT));
        assert!(!left.modifiers.matches(Modifiers::CMD_RIGHT));
        assert!(right.modifiers.matches(Modifiers::CMD_RIGHT));
        assert!(!right.modifiers.matches(Modifiers::CMD_LEFT));
    }

    #[test]
    fn bare_key_detection_covers_the_wire_formats() {
        for raw in ["z", "escape", "a", "space", "f5", "  z  "] {
            assert!(is_bare_key_binding(raw), "{raw} should be rejected as bare");
        }
        for raw in [
            "option+z",
            "ctrl_left+fn",
            "command_right",
            "shift_left",
            "cmd+shift",
            "ctrl+shift+space",
        ] {
            assert!(!is_bare_key_binding(raw), "{raw} is not a bare key");
        }
        // Unparseable values are the impl validators' concern, not ours.
        assert!(!is_bare_key_binding(""));
        assert!(!is_bare_key_binding("not+a+real+key"));
    }

    /// The rejection message names the rule (at least one modifier
    /// required) and echoes the offending binding.
    #[test]
    fn bare_key_rejection_names_the_rule() {
        let message = bare_key_rejection("z");
        assert!(message.contains("at least one modifier"), "{message}");
        assert!(message.contains("'z'"), "{message}");
    }

    /// Stored bare-key action bindings (the shipped `undo = z`) load as
    /// unbound; cancel keeps its intentional bare Escape; proper bindings
    /// (combos and hold-gated single modifiers) stay active.
    #[test]
    fn stored_bare_key_action_bindings_are_treated_as_unbound() {
        let mut settings = crate::settings::get_default_settings();

        settings.undo_enabled = true;
        settings.bindings.get_mut("undo").unwrap().current_binding = "z".to_string();
        let undo = settings.bindings.get("undo").unwrap().clone();
        assert!(
            !binding_is_active(&settings, "undo", &undo),
            "undo = z must not hold a registration"
        );

        let cancel = settings.bindings.get("cancel").unwrap().clone();
        assert!(
            binding_is_active(&settings, "cancel", &cancel),
            "cancel keeps its bare Escape (armed only while recording)"
        );

        let transcribe = settings.bindings.get("transcribe").unwrap().clone();
        assert!(binding_is_active(&settings, "transcribe", &transcribe));

        settings.delete_last_word_enabled = true;
        settings
            .bindings
            .get_mut("delete_last_word")
            .unwrap()
            .current_binding = "shift_left".to_string();
        let delete_last_word = settings.bindings.get("delete_last_word").unwrap().clone();
        assert!(
            binding_is_active(&settings, "delete_last_word", &delete_last_word),
            "a single-modifier binding stays active (it is hold-gated, not bare)"
        );
    }

    /// Template cycling: advances in catalog order, wraps around, lands on
    /// the first template from an empty selection, and refuses (leaving
    /// the selection untouched) when the library holds fewer than two.
    #[test]
    fn cycle_prompt_selection_advances_wraps_and_refuses() {
        use super::cycle_prompt_selection;

        let mut settings = crate::settings::get_default_settings();
        assert_eq!(settings.post_process_prompts.len(), 13);
        settings.post_process_selected_prompt_id =
            Some("default_improve_transcriptions".to_string());
        assert_eq!(
            cycle_prompt_selection(&mut settings).unwrap(),
            "english_casual",
            "advances to the next template in catalog order"
        );

        // From the last seed, cycling wraps to the first.
        settings.post_process_selected_prompt_id = Some("chinese_simplified".to_string());
        assert_eq!(
            cycle_prompt_selection(&mut settings).unwrap(),
            "default_improve_transcriptions",
            "wraps around to the first template"
        );

        // A dangling selection resolves as "nothing selected": first template.
        let mut fresh = crate::settings::get_default_settings();
        fresh.post_process_selected_prompt_id = Some("deleted_long_ago".to_string());
        assert_eq!(
            cycle_prompt_selection(&mut fresh).unwrap(),
            "default_improve_transcriptions"
        );

        // Fewer than two templates: refused, selection unchanged.
        let mut one = crate::settings::get_default_settings();
        one.post_process_prompts.truncate(1);
        one.post_process_selected_prompt_id = Some("default_improve_transcriptions".to_string());
        assert!(cycle_prompt_selection(&mut one).is_err());
        assert_eq!(
            one.post_process_selected_prompt_id,
            Some("default_improve_transcriptions".to_string()),
            "a refusal never moves the selection"
        );
    }

    /// Duplicating a template: fresh id, " copy" name, user flags
    /// (is_builtin=false, version=1), same body/language/register, source
    /// untouched, appended to the library. Unknown ids refuse.
    #[test]
    fn duplicate_prompt_clones_with_fresh_id_and_user_flags() {
        use super::duplicate_prompt_in_settings;
        use crate::settings::PromptRegister;

        let mut settings = crate::settings::get_default_settings();
        let source = settings
            .post_process_prompts
            .iter()
            .find(|p| p.id == "english_casual")
            .cloned()
            .unwrap();
        let before_len = settings.post_process_prompts.len();

        let dup =
            duplicate_prompt_in_settings(&mut settings, "english_casual", "prompt_999".to_string())
                .expect("duplicating a seeded template works");
        assert_eq!(dup.id, "prompt_999", "fresh id");
        assert_eq!(dup.name, "English Casual copy");
        assert!(!dup.is_builtin, "a duplicate is the operator's own");
        assert_eq!(dup.version, 1);
        assert_eq!(dup.language, source.language);
        assert_eq!(dup.register, PromptRegister::Casual);
        assert_eq!(dup.prompt, source.prompt, "the body rides along");
        assert_eq!(settings.post_process_prompts.len(), before_len + 1);
        assert!(
            settings
                .post_process_prompts
                .iter()
                .any(|p| p.id == "prompt_999"),
            "appended to the library"
        );
        assert!(
            settings
                .post_process_prompts
                .iter()
                .any(|p| p.id == "english_casual" && p.is_builtin),
            "the source stays a builtin"
        );

        assert!(
            duplicate_prompt_in_settings(&mut settings, "missing_id", "x".to_string()).is_none(),
            "unknown ids refuse"
        );
    }

    /// The cycle binding ships unbound and gated on the post-process master
    /// toggle: with post-processing on but no key recorded, it holds no
    /// registration (stock installs are inert); a real binding activates
    /// only while post-processing is on.
    #[test]
    fn cycle_prompt_binding_ships_unbound_and_rides_the_post_process_toggle() {
        let mut settings = crate::settings::get_default_settings();
        let unbound = settings
            .bindings
            .get("cycle_post_process_prompt")
            .unwrap()
            .clone();
        assert_eq!(unbound.default_binding, "");
        assert_eq!(unbound.current_binding, "");

        settings.post_process_enabled = true;
        assert!(
            !binding_is_active(&settings, "cycle_post_process_prompt", &unbound),
            "unbound means no registration even with post-processing on"
        );

        settings
            .bindings
            .get_mut("cycle_post_process_prompt")
            .unwrap()
            .current_binding = "option+p".to_string();
        let bound = settings
            .bindings
            .get("cycle_post_process_prompt")
            .unwrap()
            .clone();
        assert!(binding_is_active(
            &settings,
            "cycle_post_process_prompt",
            &bound
        ));

        settings.post_process_enabled = false;
        assert!(
            !binding_is_active(&settings, "cycle_post_process_prompt", &bound),
            "the master toggle unregisters the cycle key"
        );
    }

    /// The rebind retirement seam: a mid-session cancel rebind must retire
    /// the registration held under the PREVIOUS string (the reconcile pass
    /// reads the new string from settings and can never see the old one),
    /// or the old key stays registered - and its key events consumed
    /// system-wide in every app - until restart. Pins both the bound and
    /// the unbound prior state. The live swap itself (unregister + flag
    /// reset + re-arm) is AppHandle-typed and exercised through the
    /// change_binding command path; this pins the decision it executes.
    #[test]
    fn cancel_rebind_retires_the_previous_binding_only() {
        let mut previous = crate::settings::get_default_settings()
            .bindings
            .get("cancel")
            .unwrap()
            .clone();
        previous.current_binding = "f13".to_string();
        let retired =
            cancel_rebind_retirement(&previous).expect("a bound previous binding is retired");
        assert_eq!(
            retired.current_binding, "f13",
            "the retirement must carry the PREVIOUS string"
        );
        assert_eq!(retired.id, "cancel");

        // An unbound previous binding held no registration to drop.
        let mut unbound = previous.clone();
        unbound.current_binding = String::new();
        assert!(
            cancel_rebind_retirement(&unbound).is_none(),
            "an unbound previous binding retires nothing"
        );
        let mut whitespace = previous.clone();
        whitespace.current_binding = "   ".to_string();
        assert!(cancel_rebind_retirement(&whitespace).is_none());
    }

    /// KB-159: the cancel rebind conflict scan. A chord another ACTIVE
    /// binding owns blocks the rebind; a duplicate whose feature toggle is
    /// off owns no registration and must not block; the rebinding id is
    /// never its own conflict; an unowned chord is free.
    #[test]
    fn active_binding_conflict_detected_inactive_duplicate_and_self_ignored() {
        let mut settings = crate::settings::get_default_settings();

        // A distinctive chord on the post-process dictation key with its
        // master toggle on: an active owner.
        settings.post_process_enabled = true;
        settings
            .bindings
            .get_mut("transcribe_with_post_process")
            .unwrap()
            .current_binding = "option+9".to_string();
        assert_eq!(
            binding_conflicts_with_active_binding(&settings, "cancel", "option+9").as_deref(),
            Some("transcribe_with_post_process"),
            "an active binding owning the chord is the conflict"
        );

        // Same chord, master toggle off: holds no registration, so the
        // rebind must go through (it would register cleanly).
        settings.post_process_enabled = false;
        assert!(
            binding_conflicts_with_active_binding(&settings, "cancel", "option+9").is_none(),
            "a toggle-off duplicate owns no registration"
        );

        // Self is excluded: cancel rebinding to its own current chord
        // (bare escape by default) is a no-op rebind, not a conflict.
        let cancel_chord = settings
            .bindings
            .get("cancel")
            .unwrap()
            .current_binding
            .clone();
        assert!(
            binding_conflicts_with_active_binding(&settings, "cancel", &cancel_chord).is_none(),
            "the rebinding id never conflicts with itself"
        );

        // A chord nothing owns is free.
        assert!(
            binding_conflicts_with_active_binding(&settings, "cancel", "option+0").is_none(),
            "an unowned chord is no conflict"
        );

        // KB-194: the generic branch runs this scan with the target still
        // possibly empty (its unbind path - the cancel branch never sees an
        // empty string). An empty chord never reports a conflict: no other
        // binding's empty string is ever active.
        assert!(
            binding_conflicts_with_active_binding(&settings, "undo", "").is_none(),
            "unbinding never reports a conflict"
        );
    }

    /// KB-212: the failed-rebind restore only re-registers a chord that
    /// held a registration before the failure - a binding whose feature
    /// toggle was off (or a stored bare key, or an unbound string) owned
    /// nothing after KB-009's gate, so restoring it would resurrect a
    /// system-swallowed registration the normal path refuses to create.
    #[test]
    fn restore_registration_skips_inactive_previous_bindings() {
        let mut settings = crate::settings::get_default_settings();

        // A bound undo chord with its master toggle ON held a registration.
        settings.undo_enabled = true;
        settings
            .bindings
            .get_mut("undo")
            .unwrap()
            .current_binding = "option+u".to_string();
        let undo = settings.bindings.get("undo").unwrap().clone();
        assert!(
            restore_registration_is_due(&settings, &undo),
            "an active previous binding is restored on failure"
        );

        // Same chord, master toggle OFF: nothing was registered to restore.
        settings.undo_enabled = false;
        assert!(
            !restore_registration_is_due(&settings, &undo),
            "a toggle-off previous binding owns no registration to restore"
        );

        // Unbound previous: nothing to restore.
        settings.undo_enabled = true;
        let mut unbound = undo.clone();
        unbound.current_binding = String::new();
        assert!(
            !restore_registration_is_due(&settings, &unbound),
            "an unbound previous binding restores nothing"
        );

        // Stored bare key (accepted by earlier versions): loads as
        // unbound, so it is never restored either.
        let mut bare = undo.clone();
        bare.current_binding = "z".to_string();
        assert!(
            !restore_registration_is_due(&settings, &bare),
            "a stored bare key never held a registration"
        );
    }

    /// The Memory Safety Margin write boundary (the control used to be
    /// dead: no command, no store updater). Values 1-4 MB normalize to 0
    /// (off) - the same rule the store enforces on load - and everything
    /// else persists verbatim.
    #[test]
    fn headroom_normalization_matches_the_store_guard() {
        assert_eq!(normalize_headroom_mb(0), 0, "off stays off");
        for mb in 1..=4 {
            assert_eq!(
                normalize_headroom_mb(mb),
                0,
                "{mb} MB is neither off nor usable"
            );
        }
        assert_eq!(
            normalize_headroom_mb(5),
            5,
            "the smallest usable margin persists"
        );
        assert_eq!(normalize_headroom_mb(512), 512);
        assert_eq!(normalize_headroom_mb(2048), 2048);
    }
}
