//! Cross-platform global keyboard shortcuts library.
//!
//! `handy-keys` provides a simple way to register and listen for global keyboard
//! shortcuts across macOS, Windows, and Linux.
//!
//! # Features
//!
//! - **Global hotkeys**: Register system-wide keyboard shortcuts that work even
//!   when your application is not focused
//! - **Hotkey blocking**: Registered hotkeys are blocked from reaching other applications
//! - **Modifier-only hotkeys**: Support for shortcuts like `Cmd+Shift` without a key
//! - **String parsing**: Parse hotkeys from strings like `"Ctrl+Alt+Space"`
//! - **Hotkey recording**: Low-level [`KeyboardListener`] for implementing
//!   "record a hotkey" UI flows
//! - **Serde support**: All types implement `Serialize`/`Deserialize`
//!
//! # Quick Start
//!
//! ```no_run
//! use handy_keys::{HotkeyManager, Hotkey, Modifiers, Key};
//!
//! fn main() -> handy_keys::Result<()> {
//!     let manager = HotkeyManager::new()?;
//!
//!     // Register Cmd+Shift+K using the type-safe constructor
//!     let hotkey = Hotkey::new(Modifiers::CMD | Modifiers::SHIFT, Key::K)?;
//!     let id = manager.register(hotkey)?;
//!
//!     // Or parse from a string (useful for UI/config input)
//!     let hotkey2: Hotkey = "Ctrl+Alt+Space".parse()?;
//!     let id2 = manager.register(hotkey2)?;
//!
//!     println!("Registered hotkeys: {:?}, {:?}", id, id2);
//!
//!     // Wait for hotkey events
//!     while let Ok(event) = manager.recv() {
//!         println!("Hotkey triggered: {:?}", event.id);
//!     }
//!
//!     Ok(())
//! }
//! ```
//!
//! # Recording Hotkeys
//!
//! For implementing "press a key to set hotkey" UIs, use [`KeyboardListener`]:
//!
//! ```no_run
//! use handy_keys::KeyboardListener;
//!
//! let listener = KeyboardListener::new()?;
//!
//! // Listen for key events
//! while let Ok(event) = listener.recv() {
//!     if event.is_key_down {
//!         if let Ok(hotkey) = event.as_hotkey() {
//!             println!("User pressed: {}", hotkey);
//!             break;
//!         }
//!     }
//! }
//! # Ok::<(), handy_keys::Error>(())
//! ```
//!
//! # Platform Notes
//!
//! ## macOS
//!
//! Requires accessibility permissions. Use [`check_accessibility`] to check if
//! permissions are granted, and [`open_accessibility_settings`] to prompt the user:
//!
//! ```no_run
//! # #[cfg(target_os = "macos")]
//! # fn main() -> handy_keys::Result<()> {
//! use handy_keys::{check_accessibility, open_accessibility_settings};
//!
//! if !check_accessibility() {
//!     open_accessibility_settings()?;
//!     // User needs to grant permission and restart
//! }
//! # Ok(())
//! # }
//! # #[cfg(not(target_os = "macos"))]
//! # fn main() {}
//! ```
//!
//! ## Windows
//!
//! Uses low-level keyboard hooks. No special permissions required.
//!
//! ## Linux
//!
//! Reads evdev devices (`/dev/input/event*`) directly, which works the same
//! on Wayland, X11, and the console. Requires read access to the device
//! nodes — membership in the `input` group, or the udev rule below. Hotkey
//! *blocking* grabs keyboards exclusively and re-injects non-blocked events
//! through uinput, so it additionally requires write access to
//! `/dev/uinput`; without it, the blocking constructors fail with an
//! actionable error (the non-blocking listener is unaffected).
//!
//! ### Shipping to Linux users
//!
//! When distributing an app, don't ask users to join the `input` group —
//! ship a udev `uaccess` rule instead. systemd-logind then grants access to
//! the *active seat user* (whoever is physically logged in) through device
//! ACLs: effective immediately with no logout, and unlike group membership
//! it does not extend to SSH sessions. One rule file covers both listening
//! and blocking:
//!
//! ```text
//! # /usr/lib/udev/rules.d/70-yourapp-input.rules
//! # (uaccess rules must sort before 73-seat-late.rules: keep the number < 73)
//! KERNEL=="uinput", TAG+="uaccess"
//! SUBSYSTEM=="input", KERNEL=="event*", TAG+="uaccess"
//! ```
//!
//! Packages (deb, rpm, ...) install this file and run
//! `udevadm control --reload && udevadm trigger` in their post-install step,
//! so users never perform any setup. Installer-less formats (AppImage) can
//! write it on first run — e.g. via `pkexec` — when the constructors report
//! missing access, then retry immediately: the ACL applies without a
//! restart. Degrade gracefully where blocking is unavailable:
//!
//! ```no_run
//! use handy_keys::HotkeyManager;
//!
//! let manager = HotkeyManager::new_with_blocking() // blocks matched hotkeys
//!     .or_else(|_| HotkeyManager::new());          // read-only: detect but don't block
//! // If both fail, prompt the user to grant access, then retry.
//! # drop(manager);
//! ```
//!
//! Read access to `/dev/input` is inherently keyboard-read capability — the
//! kernel offers nothing finer-grained. Sandboxes that hide `/dev/input`
//! (e.g. Flatpak) cannot use this backend.

mod error;
mod listener;
mod manager;
mod platform;
mod types;

pub use error::{Error, Result};
pub use listener::{BlockingHotkeys, KeyboardListener};
pub use manager::HotkeyManager;
pub use types::{Hotkey, HotkeyEvent, HotkeyId, HotkeyState, Key, KeyEvent, Modifiers};

#[cfg(target_os = "macos")]
pub use platform::macos::{check_accessibility, open_accessibility_settings};

/// Marker the host app stamps into `CGEventField::EventSourceUserData` on
/// every event it synthesizes (enigo 0.6 exposes the field through
/// `EnigoSettings::event_source_user_data`). The macOS listener ignores
/// events carrying this marker so the app's own paste and editing chords
/// can never re-enter the hotkey machinery as if they were user input.
///
/// The value spells "VOXBAR" in ASCII so it cannot collide with enigo's
/// own default marker (100) or with another app's arbitrary small values.
/// It is defined unconditionally because the host sets it in its Enigo
/// settings on every platform; only the macOS tap consults it.
pub const SYNTHESIZED_EVENT_MARKER: i64 = 0x564F_5842_4152;

/// Default hold-to-activate window for single-modifier hotkeys (see
/// [`HotkeyManager::with_single_modifier_hold`]). A lone modifier key must
/// be held this long, with no other key event in between, before its
/// hotkey activates; chord usage (Cmd+C and friends) releases far faster
/// than this and can never trigger. Individual hotkeys can carry their own
/// window instead of this default via
/// [`HotkeyManager::register_with_hold`].
pub const SINGLE_MODIFIER_HOLD_THRESHOLD: std::time::Duration =
    std::time::Duration::from_millis(400);
