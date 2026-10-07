//! Synthesized editing chords (delete word, undo, delete line, paste).
//!
//! The paste chord in [`crate::input::send_paste_ctrl_v`] shows the pattern
//! this module follows: describe the keys abstractly, then translate them to
//! enigo `Key`s per platform. Editing actions assignable in the shortcut
//! system and command mode's key actions both inject through here, so the
//! CGEvent/SendInput synthesis lives in exactly one place instead of being
//! duplicated per feature.
//!
//! The mapping table is deliberately a pure function of an explicit
//! [`Platform`] value (not of `cfg`), so unit tests pin every platform's
//! chords from any host. Only the executor touches real input APIs, and live
//! injection cannot be exercised in tests.

use enigo::{Direction, Enigo, Key, Keyboard};
use std::time::Duration;
use tauri::{AppHandle, Manager};

use crate::input::EnigoState;

/// Which platform a key sequence is resolved for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Macos,
    Windows,
    Linux,
}

/// The platform this build runs on.
pub fn current_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::Macos
    } else if cfg!(target_os = "windows") {
        Platform::Windows
    } else {
        Platform::Linux
    }
}

/// A logical key that has a stable native equivalent on every platform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogicalKey {
    Backspace,
    Delete,
    Home,
    V,
    Z,
}

/// One synthesized chord: modifiers held while [`LogicalKey`] is clicked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeySpec {
    /// Command on macOS, Control on Windows and Linux.
    pub primary: bool,
    /// Option on macOS, Alt on Windows and Linux.
    pub alt: bool,
    pub shift: bool,
    pub key: LogicalKey,
}

impl KeySpec {
    const fn primary(key: LogicalKey) -> Self {
        Self {
            primary: true,
            alt: false,
            shift: false,
            key,
        }
    }

    const fn alt(key: LogicalKey) -> Self {
        Self {
            primary: false,
            alt: true,
            shift: false,
            key,
        }
    }

    const fn shift(key: LogicalKey) -> Self {
        Self {
            primary: false,
            alt: false,
            shift: true,
            key,
        }
    }

    const fn plain(key: LogicalKey) -> Self {
        Self {
            primary: false,
            alt: false,
            shift: false,
            key,
        }
    }
}

/// Editing actions delivered as synthesized key chords to the focused app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditAction {
    /// Delete the word before the caret: Option+Backspace on macOS,
    /// Ctrl+Backspace on Windows and Linux.
    DeleteLastWord,
    /// Undo the last edit: Cmd+Z on macOS, Ctrl+Z on Windows and Linux.
    Undo,
    /// Delete to the start of the line: Cmd+Backspace on macOS,
    /// Shift+Home followed by Delete on Windows and Linux.
    DeleteLine,
    /// A plain paste chord: Cmd+V on macOS, Ctrl+V on Windows and Linux.
    Paste,
}

/// Pure mapping table: action to chord sequence, per platform. Sequences with
/// more than one chord run in order (only DeleteLine needs that today).
pub fn key_sequence_for(action: EditAction, platform: Platform) -> Vec<KeySpec> {
    match (action, platform) {
        (EditAction::DeleteLastWord, Platform::Macos) => vec![KeySpec::alt(LogicalKey::Backspace)],
        (EditAction::DeleteLastWord, Platform::Windows | Platform::Linux) => {
            vec![KeySpec::primary(LogicalKey::Backspace)]
        }
        (EditAction::Undo, _) => vec![KeySpec::primary(LogicalKey::Z)],
        (EditAction::DeleteLine, Platform::Macos) => vec![KeySpec::primary(LogicalKey::Backspace)],
        (EditAction::DeleteLine, Platform::Windows | Platform::Linux) => vec![
            KeySpec::shift(LogicalKey::Home),
            KeySpec::plain(LogicalKey::Delete),
        ],
        (EditAction::Paste, _) => vec![KeySpec::primary(LogicalKey::V)],
    }
}

/// The chord sequence for the running platform.
pub fn key_sequence(action: EditAction) -> Vec<KeySpec> {
    key_sequence_for(action, current_platform())
}

/// How long the modifiers stay held after the key click. Matches the legacy
/// paste chord hold: most apps read the modifiers from the key event's flags,
/// but apps that poll global keyboard state need the modifier still down.
const CHORD_HOLD_MS: u64 = 100;

/// Pause between consecutive chords of a multi-chord sequence so the target
/// app's event loop observes each step as its own edit.
const STEP_GAP_MS: u64 = 30;

/// Windows virtual key code for Z, following the same convention as the
/// paste path's VK_V.
#[cfg(target_os = "windows")]
const VK_Z: u32 = 0x5A;

/// Translate a logical key to the enigo key for the running platform. Letters
/// reuse the layout-aware resolution the paste chord uses: a resolved macOS
/// keycode, a Windows virtual key code, and a Unicode key on Linux.
fn enigo_key(key: LogicalKey) -> Key {
    match key {
        LogicalKey::Backspace => Key::Backspace,
        LogicalKey::Delete => Key::Delete,
        LogicalKey::Home => Key::Home,
        LogicalKey::V => {
            #[cfg(target_os = "macos")]
            {
                crate::input::command_v_key()
            }
            #[cfg(target_os = "windows")]
            {
                Key::Other(0x56) // VK_V, matching the paste path
            }
            #[cfg(target_os = "linux")]
            {
                Key::Unicode('v')
            }
        }
        LogicalKey::Z => {
            #[cfg(target_os = "macos")]
            {
                crate::input::command_z_key()
            }
            #[cfg(target_os = "windows")]
            {
                Key::Other(VK_Z)
            }
            #[cfg(target_os = "linux")]
            {
                Key::Unicode('z')
            }
        }
    }
}

/// The primary modifier (Command on macOS, Control elsewhere).
fn primary_modifier() -> Key {
    #[cfg(target_os = "macos")]
    {
        Key::Meta
    }
    #[cfg(not(target_os = "macos"))]
    {
        Key::Control
    }
}

/// Send one chord: press the modifiers, click the key, hold, then release.
fn send_spec(enigo: &mut Enigo, spec: KeySpec) -> Result<(), String> {
    let mut pressed: Vec<Key> = Vec::with_capacity(3);
    if spec.primary {
        let key = primary_modifier();
        enigo
            .key(key, Direction::Press)
            .map_err(|e| format!("Failed to press primary modifier: {}", e))?;
        pressed.push(key);
    }
    if spec.alt {
        enigo
            .key(Key::Alt, Direction::Press)
            .map_err(|e| format!("Failed to press alt modifier: {}", e))?;
        pressed.push(Key::Alt);
    }
    if spec.shift {
        enigo
            .key(Key::Shift, Direction::Press)
            .map_err(|e| format!("Failed to press shift modifier: {}", e))?;
        pressed.push(Key::Shift);
    }

    let click_result = enigo
        .key(enigo_key(spec.key), Direction::Click)
        .map_err(|e| format!("Failed to click {:?} key: {}", spec.key, e));

    // Always release what was pressed, even when the click failed, so a
    // failed chord can never leave a stuck modifier on the user's keyboard.
    std::thread::sleep(Duration::from_millis(CHORD_HOLD_MS));
    for key in pressed.into_iter().rev() {
        if let Err(e) = enigo.key(key, Direction::Release) {
            log::warn!("Failed to release modifier after chord: {}", e);
        }
    }

    click_result
}

/// Synthesize the chords for `action` through the shared Enigo state, in
/// sequence order. On macOS the caller must be on the main thread because the
/// letter-key resolution queries the keyboard layout (the paste path already
/// enters through `AppHandle::run_on_main_thread` for the same reason).
pub fn send_edit_action(app: &AppHandle, action: EditAction) -> Result<(), String> {
    let specs = key_sequence(action);
    let enigo_state = app
        .try_state::<EnigoState>()
        .ok_or("Enigo state not initialized")?;
    let mut enigo = enigo_state
        .0
        .lock()
        .map_err(|e| format!("Failed to lock Enigo: {}", e))?;

    for (index, spec) in specs.iter().enumerate() {
        if index > 0 {
            std::thread::sleep(Duration::from_millis(STEP_GAP_MS));
        }
        send_spec(&mut enigo, *spec)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_word_uses_option_on_macos_and_ctrl_elsewhere() {
        let expected_alt = vec![KeySpec::alt(LogicalKey::Backspace)];
        let expected_ctrl = vec![KeySpec::primary(LogicalKey::Backspace)];
        assert_eq!(
            key_sequence_for(EditAction::DeleteLastWord, Platform::Macos),
            expected_alt
        );
        for platform in [Platform::Windows, Platform::Linux] {
            assert_eq!(
                key_sequence_for(EditAction::DeleteLastWord, platform),
                expected_ctrl.clone(),
                "wrong delete-word chord for {platform:?}"
            );
        }
    }

    #[test]
    fn undo_is_primary_plus_z_on_every_platform() {
        let expected = vec![KeySpec::primary(LogicalKey::Z)];
        for platform in [Platform::Macos, Platform::Windows, Platform::Linux] {
            assert_eq!(
                key_sequence_for(EditAction::Undo, platform),
                expected.clone(),
                "wrong undo chord for {platform:?}"
            );
        }
    }

    #[test]
    fn delete_line_is_cmd_backspace_on_macos_and_home_selection_elsewhere() {
        assert_eq!(
            key_sequence_for(EditAction::DeleteLine, Platform::Macos),
            vec![KeySpec::primary(LogicalKey::Backspace)]
        );
        let selection = vec![
            KeySpec::shift(LogicalKey::Home),
            KeySpec::plain(LogicalKey::Delete),
        ];
        for platform in [Platform::Windows, Platform::Linux] {
            assert_eq!(
                key_sequence_for(EditAction::DeleteLine, platform),
                selection.clone(),
                "wrong delete-line sequence for {platform:?}"
            );
        }
    }

    #[test]
    fn paste_is_primary_plus_v_on_every_platform() {
        let expected = vec![KeySpec::primary(LogicalKey::V)];
        for platform in [Platform::Macos, Platform::Windows, Platform::Linux] {
            assert_eq!(
                key_sequence_for(EditAction::Paste, platform),
                expected.clone(),
                "wrong paste chord for {platform:?}"
            );
        }
    }

    #[test]
    fn running_platform_lookup_matches_the_sequence_table() {
        // The cfg-based lookup and the explicit-enum table must agree for
        // every action on the host platform.
        for action in [
            EditAction::DeleteLastWord,
            EditAction::Undo,
            EditAction::DeleteLine,
            EditAction::Paste,
        ] {
            assert_eq!(
                key_sequence(action),
                key_sequence_for(action, current_platform()),
                "cfg platform lookup diverged from the table for {action:?}"
            );
        }
    }
}
