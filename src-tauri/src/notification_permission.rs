//! The REAL macOS notification authorization state (fleet pass 2).
//!
//! The tauri notification plugin's desktop build (2.4.0) stubs
//! `request_permission`/`permission_state` as unconditional `Granted` and
//! shims `window.Notification.permission = "granted"` at boot, so every
//! frontend gate on `isPermissionGranted()` is vacuous on macOS: without
//! the System Settings grant the plugin (and mac-notification-sys) post
//! nothing, the skip branch never fires, and no surface can tell. This
//! module talks to UNUserNotificationCenter directly (objc2-user-notifications,
//! already in the dependency tree through tauri's macOS stack) to QUERY
//! the real grant, REQUEST it exactly once (the system prompt), and report
//! a state the settings UI and the send paths can act on. A query that
//! fails surfaces as `Unknown`, never as a silent grant.
//!
//! macOS only this cycle (the companion cycle's platform note). The
//! non-macOS arm reports Granted to keep the plugin's behavior there;
//! if a mobile/desktop-OTHER target ever ships, that arm must grow the
//! platform's real query (the plugin's mobile APIs are real).

use serde::Serialize;

/// The authorization states callers act on. `Granted` folds Apple's
/// Authorized/Provisional/Ephemeral together: all three deliver.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum NotificationPermissionState {
    /// The system prompt has not been shown (or answered) yet.
    NotDetermined,
    /// The operator denied the grant; nothing will be delivered.
    Denied,
    /// Authorized (or provisional/ephemeral): delivery proceeds.
    Granted,
    /// The real query could not be completed (FFI/timeout failure). Never
    /// treated as a grant: callers skip loudly rather than post into a
    /// void they cannot see.
    Unknown,
}

/// Pure mapping from UNAuthorizationStatus's raw NSInteger to the state
/// callers act on (unit-pinned; the FFI layer feeds it).
fn state_from_status(status: i64) -> NotificationPermissionState {
    match status {
        // UNAuthorizationStatusNotDetermined
        0 => NotificationPermissionState::NotDetermined,
        // UNAuthorizationStatusDenied
        1 => NotificationPermissionState::Denied,
        // Authorized, Provisional, and Ephemeral all deliver.
        2 | 3 | 4 => NotificationPermissionState::Granted,
        // An unrecognized future value: honest refusal, not a grant.
        _ => NotificationPermissionState::Unknown,
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{state_from_status, NotificationPermissionState};
    use objc2_foundation::NSError;
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNNotificationSettings, UNUserNotificationCenter,
    };
    use std::time::Duration;

    /// UNUserNotificationCenter's completion handlers arrive on a private
    /// dispatch queue, so each query runs on its own thread and blocks on
    /// a channel with a hard timeout: a hung XPC roundtrip must read as
    /// Unknown, never wedge the caller.
    const REPLY_TIMEOUT: Duration = Duration::from_secs(2);
    /// The authorization REQUEST shows the system prompt, which needs the
    /// user's attention: bound it longer than a settings roundtrip, but
    /// still finite (an unanswered prompt reads as Unknown, and the next
    /// notice re-asks; the OS's deny-once bookkeeping keeps repeats quiet
    /// once answered).
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

    fn raw_status() -> Option<i64> {
        // The settings roundtrip must happen on a thread that is allowed
        // to block; the tauri command thread awaits it via join.
        std::thread::spawn(|| {
            let (tx, rx) = std::sync::mpsc::channel::<i64>();
            let center = UNUserNotificationCenter::currentNotificationCenter();
            let block =
                block2::RcBlock::new(move |settings: std::ptr::NonNull<UNNotificationSettings>| {
                    // The settings object is owned by the framework's
                    // callback; reading the enum field through the shared
                    // reference is the documented accessor pattern. The
                    // pointer is only dereferenced synchronously inside the
                    // callback the framework itself invoked.
                    let status = unsafe { settings.as_ref().authorizationStatus() };
                    let _ = tx.send(status.0 as i64);
                });
            center.getNotificationSettingsWithCompletionHandler(&block);
            rx.recv_timeout(REPLY_TIMEOUT).ok()
        })
        .join()
        .ok()
        .flatten()
    }

    /// The current authorization state, queried for real.
    pub fn current_state() -> NotificationPermissionState {
        raw_status()
            .map(state_from_status)
            .unwrap_or(NotificationPermissionState::Unknown)
    }

    /// Query, and if the state is NotDetermined, show the system prompt
    /// (once per the OS's own bookkeeping) and report what came back.
    /// A denied-by-the-user request is DENIAL, not failure: the caller
    /// distinguishes the two (denial is expected output; Unknown is the
    /// loud failure).
    pub fn request_if_needed() -> NotificationPermissionState {
        let status = match raw_status() {
            Some(status) => status,
            None => return NotificationPermissionState::Unknown,
        };
        if state_from_status(status) != NotificationPermissionState::NotDetermined {
            return state_from_status(status);
        }
        let granted = std::thread::spawn(|| {
            let (tx, rx) = std::sync::mpsc::channel::<bool>();
            let center = UNUserNotificationCenter::currentNotificationCenter();
            let block =
                block2::RcBlock::new(move |granted: objc2::runtime::Bool, _error: *mut NSError| {
                    // The error is framework-owned and only meaningful
                    // inside the callback; the boolean carries the answer.
                    let _ = tx.send(granted.as_bool());
                });
            center.requestAuthorizationWithOptions_completionHandler(
                UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
                &block,
            );
            rx.recv_timeout(REQUEST_TIMEOUT).ok()
        })
        .join()
        .ok()
        .flatten();
        match granted {
            Some(true) => NotificationPermissionState::Granted,
            Some(false) => NotificationPermissionState::Denied,
            None => NotificationPermissionState::Unknown,
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::NotificationPermissionState;

    // Off macOS this cycle: the plugin's permission story is the
    // platform's own; report the plugin-equivalent grant so the frontend
    // keeps its current behavior. A real query must land here before any
    // other desktop target ships.
    pub fn current_state() -> NotificationPermissionState {
        NotificationPermissionState::Granted
    }

    pub fn request_if_needed() -> NotificationPermissionState {
        NotificationPermissionState::Granted
    }
}

pub use imp::{current_state, request_if_needed};

/// The state query command: the settings UI and the frontend priming flow
/// read the REAL macOS grant through this (the plugin's desktop stub
/// always says granted).
#[tauri::command]
#[specta::specta]
pub fn notification_permission_state() -> NotificationPermissionState {
    current_state()
}

/// The one-shot request command for the priming flow: resolves
/// NotDetermined with the system prompt and reports the outcome. The OS's
/// own deny-once semantics make repeats no-ops.
#[tauri::command]
#[specta::specta]
pub fn request_notification_permission() -> NotificationPermissionState {
    request_if_needed()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_mapping_folds_only_delivering_states_into_granted() {
        assert_eq!(
            state_from_status(0),
            NotificationPermissionState::NotDetermined
        );
        assert_eq!(state_from_status(1), NotificationPermissionState::Denied);
        for status in 2..=4 {
            assert_eq!(
                state_from_status(status),
                NotificationPermissionState::Granted,
                "Authorized/Provisional/Ephemeral all deliver"
            );
        }
        assert_eq!(
            state_from_status(5),
            NotificationPermissionState::Unknown,
            "an unrecognized future value is an honest refusal, never a grant"
        );
        assert_eq!(
            state_from_status(-1),
            NotificationPermissionState::Unknown,
            "a garbage value is an honest refusal, never a grant"
        );
    }
}
