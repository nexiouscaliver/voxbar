// v1.6.0 W1-L4: the persistent-notice notification surface, Rust side.
//
// The frontend's persistent surface (src/lib/desktopNotify.ts,
// notifyDesktopPersistent) funnels every persistent post through the
// post_persistent_notice command here: ONE send path per platform.
//
// On macOS the sender is mac-notification-sys directly - the same crate
// the tauri notification plugin's desktop backend already drives (it is
// already in the tree as that transitive dependency, pinned at the same
// version, so nothing new is downloaded). Going direct buys the two
// things the plugin's desktop build cannot provide in 2.4.0: a blocking
// delivery whose response reports that the user clicked, and per-notice
// routing context held by the sending thread (the plugin registers no
// desktop action listener and drops the extra payload before the OS;
// its NS delegate also activates nothing on a click).
//
// On a content click: raise the main window (show_main_window: unminimize,
// show, focus, activation policy Regular - the same path the tray uses)
// and emit notice-notification-click with the notice payload, which the
// frontend click bus (desktopNotify.ts) turns into section navigation.
//
// The transient legacy surface (notifyDesktop's hidden-window banners and
// the updater flow) keeps using the tauri plugin unchanged.

use serde::Serialize;
use tauri::AppHandle;

#[cfg(target_os = "macos")]
use tauri::Emitter;

/// The event name the webview listens for (src/lib/desktopNotify.ts,
/// attachNativeNoticeClickSource). Plain string event, same wiring style
/// as main-window-shown.
pub const NOTICE_CLICK_EVENT: &str = "notice-notification-click";

/// Payload for one notification activation, delivered to the webview.
/// Field names are the contract the frontend click bus reads (it accepts
/// string code/target and nothing else).
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct NoticeNotificationClick {
    pub code: String,
    pub target: String,
}

/// What a finished delivery means for the app (pure, unit-tested seam).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoticeClickAction {
    /// The user clicked the notification content: focus the app and
    /// navigate to the payload's target section.
    FocusAndNavigate(NoticeNotificationClick),
    /// No activating interaction (auto-dismiss, close button, timeout):
    /// the notification did its job quietly.
    Ignore,
}

/// Map a finished mac-notification-sys delivery to the action the app
/// takes. Strict by design: only a direct content click navigates. We
/// register no action buttons, reply fields, or close buttons, so every
/// other variant is unreachable in practice and must stay inert if it
/// ever arrives.
#[cfg(target_os = "macos")]
pub fn click_action_for(
    response: mac_notification_sys::NotificationResponse,
    payload: NoticeNotificationClick,
) -> NoticeClickAction {
    match response {
        mac_notification_sys::NotificationResponse::Click => {
            NoticeClickAction::FocusAndNavigate(payload)
        }
        _ => NoticeClickAction::Ignore,
    }
}

/// Post one persistent notice. The command returns as soon as the notice
/// is handed to the platform; the click wait happens on its own thread.
///
/// Fleet pass 2: the send is gated on the REAL macOS authorization state
/// (notification_permission), because the tauri plugin's desktop build
/// stubs every permission query as granted and mac-notification-sys
/// resolves a disabled-in-System-Settings send as a plain no-response -
/// the old path posted into a void with no log, no error, and no state
/// anywhere. Denied and Unknown are LOUD here (an actionable warn plus an
/// Err the frontend logs); NotDetermined resolves with the system prompt
/// first, which is what makes the app's asks-once claim true.
#[tauri::command]
#[specta::specta]
pub fn post_persistent_notice(
    app: AppHandle,
    body: String,
    code: String,
    target: String,
) -> Result<(), String> {
    match crate::notification_permission::request_if_needed() {
        crate::notification_permission::NotificationPermissionState::Denied => {
            let detail = "persistent notice not delivered: macOS notifications are denied for                 this app. Re-enable them in System Settings > Notifications > VoxBar.";
            log::warn!("{detail}");
            Err(detail.to_string())
        }
        crate::notification_permission::NotificationPermissionState::Unknown => {
            let detail =
                "persistent notice not delivered: the macOS notification authorization state                 could not be queried; skipping rather than posting into an unreadable void.";
            log::warn!("{detail}");
            Err(detail.to_string())
        }
        crate::notification_permission::NotificationPermissionState::Granted
        | crate::notification_permission::NotificationPermissionState::NotDetermined => {
            // Granted posts. NotDetermined cannot survive request_if_needed
            // (it resolves the prompt and returns the outcome), so this arm
            // is Granted in practice; the match keeps every state named.
            let payload = NoticeNotificationClick { code, target };
            post_and_watch(&app, body, payload);
            Ok(())
        }
    }
}

/// The bundle identity notifications attribute to: the configured app id,
/// or Terminal in dev (a bare `cargo tauri dev` binary has no bundle to
/// impersonate). Mirrors the plugin's desktop.rs choice exactly, so
/// whichever leg sends first latches the SAME identity (pure, unit-tested).
#[cfg(target_os = "macos")]
fn sender_identity(is_dev: bool, configured_identifier: &str) -> String {
    if is_dev {
        "com.apple.Terminal".to_string()
    } else {
        configured_identifier.to_string()
    }
}

#[cfg(target_os = "macos")]
fn post_and_watch(app: &AppHandle, body: String, payload: NoticeNotificationClick) {
    // Identity FIRST, before any send: mac-notification-sys latches the
    // sending app process-wide on the first notification (it swizzles
    // NSBundle.mainBundle.bundleIdentifier, objc/notify.h
    // installNSBundleHook, and the Once in its set_application refuses
    // every later call), and when nobody set an identity it defaults to
    // com.apple.Finder (lib.rs get_bundle_identifier_or_default) - which
    // would attribute our notifications to Finder AND report Finder's
    // bundle id to every later native reader for the rest of the run.
    // This direct leg is typically the process's FIRST sender (with the
    // master toggle on, the persistent surface replaces the legacy hidden
    // offer), so it must set the identity itself instead of relying on the
    // plugin's leg having sent first. An AlreadySet error is the expected
    // outcome when the plugin's leg did send first; it is discarded
    // exactly like desktop.rs discards it.
    let _ = mac_notification_sys::set_application(&sender_identity(
        tauri::is_dev(),
        &app.config().identifier,
    ));
    let app = app.clone();
    // The blocking send parks this thread until the user interacts or the
    // banner auto-dismisses (mac-notification-sys resolves that via a main
    // runloop poll timer, so an unclicked Notification Center entry keeps
    // one small parked thread - bounded by how many notices sit uncleared).
    std::thread::spawn(move || {
        let mut notification = mac_notification_sys::Notification::new();
        notification
            .title("VoxBar")
            .message(&body)
            .wait_for_click(true);
        match notification.send() {
            Ok(response) => {
                if let NoticeClickAction::FocusAndNavigate(payload) =
                    click_action_for(response, payload)
                {
                    crate::show_main_window(&app);
                    if let Err(e) = app.emit(NOTICE_CLICK_EVENT, &payload) {
                        log::error!("Failed to emit {}: {}", NOTICE_CLICK_EVENT, e);
                    }
                }
            }
            Err(e) => {
                log::warn!("Persistent notice send failed: {}", e);
            }
        }
    });
}

/// Non-macOS desktop keeps the exact delivery the frontend had before the
/// Rust seam existed: the tauri notification plugin (same backend the JS
/// API drives). No click leg: those platforms have no wired click source
/// yet, matching the pre-seam behavior.
#[cfg(not(target_os = "macos"))]
fn post_and_watch(app: &AppHandle, body: String, _payload: NoticeNotificationClick) {
    use tauri_plugin_notification::NotificationExt;
    if let Err(e) = app
        .notification()
        .builder()
        .title("VoxBar")
        .body(&body)
        .show()
    {
        log::warn!("Persistent notice send failed: {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_payload_serializes_to_the_frontend_bus_shape() {
        let payload = NoticeNotificationClick {
            code: "recording_failed".to_string(),
            target: "notifications".to_string(),
        };
        assert_eq!(
            serde_json::to_value(&payload).unwrap(),
            serde_json::json!({ "code": "recording_failed", "target": "notifications" })
        );
        // The event name is a wire contract with the webview listener.
        assert_eq!(NOTICE_CLICK_EVENT, "notice-notification-click");
    }

    /// Fleet pass 2 wiring pin: the macOS send path must gate on the REAL
    /// authorization state (notification_permission) BEFORE any
    /// mac-notification-sys send, and Denied/Unknown must be loud (warn +
    /// Err). The gate needs a live UNUserNotificationCenter a unit test
    /// cannot mount headless, so the wiring is pinned against this
    /// committed source - the same convention as the coordinator's emit
    /// pins. Before the fix the plugin's desktop stub made every state
    /// read Granted, so a System-Settings denial posted into a void with
    /// no log on any surface.
    #[test]
    fn send_path_gates_on_the_real_authorization_and_fails_loud() {
        let source = include_str!("persistent_notice.rs");
        let at = source
            .find("pub fn post_persistent_notice")
            .expect("post_persistent_notice");
        let end = at
            + source[at..]
                .find("fn sender_identity")
                .expect("the fn after the command");
        let body = &source[at..end];
        let gate = body
            .find("notification_permission::request_if_needed()")
            .expect("the command consults the real authorization state");
        let send = body
            .find("post_and_watch(&app, body, payload)")
            .expect("the send call");
        assert!(
            gate < send,
            "the authorization gate must run BEFORE the platform send"
        );
        for needle in [
            "NotificationPermissionState::Denied =>",
            "NotificationPermissionState::Unknown =>",
            "log::warn!",
            "Err(detail.to_string())",
        ] {
            assert!(
                body.contains(needle),
                "the denied/unknown paths must stay loud: `{needle}` missing"
            );
        }
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn sender_identity_mirrors_the_plugin_desktop_choice() {
        // Dev builds impersonate Terminal (a bare cargo-tauri-dev binary
        // has no bundle); production uses the configured bundle id
        // verbatim (tauri.conf.json: com.voxbar.app). Both legs (this
        // direct sender and the plugin's desktop.rs) must resolve to the
        // SAME identity, or whichever sends first would latch the other's.
        assert_eq!(
            sender_identity(true, "com.voxbar.app"),
            "com.apple.Terminal"
        );
        assert_eq!(sender_identity(false, "com.voxbar.app"), "com.voxbar.app");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn only_a_content_click_focuses_and_navigates() {
        let payload = NoticeNotificationClick {
            code: "companion_session_capped".to_string(),
            target: "history".to_string(),
        };
        assert_eq!(
            click_action_for(
                mac_notification_sys::NotificationResponse::Click,
                payload.clone()
            ),
            NoticeClickAction::FocusAndNavigate(payload.clone())
        );
        for response in [
            mac_notification_sys::NotificationResponse::None,
            mac_notification_sys::NotificationResponse::ActionButton("x".to_string()),
            mac_notification_sys::NotificationResponse::CloseButton("Close".to_string()),
            mac_notification_sys::NotificationResponse::Reply("r".to_string()),
        ] {
            assert_eq!(
                click_action_for(response, payload.clone()),
                NoticeClickAction::Ignore,
                "non-content interactions must never focus or navigate"
            );
        }
    }
}
