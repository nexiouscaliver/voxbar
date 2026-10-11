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
#[tauri::command]
#[specta::specta]
pub fn post_persistent_notice(
    app: AppHandle,
    body: String,
    code: String,
    target: String,
) -> Result<(), String> {
    let payload = NoticeNotificationClick { code, target };
    post_and_watch(&app, body, payload);
    Ok(())
}

#[cfg(target_os = "macos")]
fn post_and_watch(app: &AppHandle, body: String, payload: NoticeNotificationClick) {
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
