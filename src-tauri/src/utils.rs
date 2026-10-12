use crate::managers::audio::AudioRecordingManager;
use crate::managers::transcription::TranscriptionManager;
use crate::shortcut;
use crate::TranscriptionCoordinator;
use log::info;
use std::sync::Arc;
use tauri::{AppHandle, Manager};

// Re-export all utility modules for easy access
// pub use crate::audio_feedback::*;
pub use crate::clipboard::*;
pub use crate::overlay::*;
pub use crate::tray::*;

/// Preserve diagnostic text in development builds, but redact it in releases.
/// Do not use for secrets such as API keys, which must always be redacted.
pub fn redact_text(text: &str) -> &str {
    if cfg!(debug_assertions) {
        text
    } else {
        "[REDACTED]"
    }
}

#[cfg(any(test, all(target_os = "windows", target_arch = "x86_64")))]
const IMAGE_FILE_MACHINE_ARM64: u16 = 0xaa64;

#[cfg(any(test, all(target_os = "windows", target_arch = "x86_64")))]
fn native_machine_is_arm64(native_machine: Option<u16>) -> bool {
    native_machine == Some(IMAGE_FILE_MACHINE_ARM64)
}

/// Whether this is the x64 Windows build running under emulation on Windows ARM64.
///
/// Only that exact process/host pairing disables the transcribe.cpp GPU path.
/// Detection is deliberately fail-open: a native x64 host, an older Windows
/// version without `IsWow64Process2`, or any API error leaves existing behavior
/// unchanged.
pub fn is_windows_x64_emulated_on_arm64() -> bool {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        use std::sync::OnceLock;

        static DETECTED: OnceLock<bool> = OnceLock::new();
        *DETECTED.get_or_init(|| native_machine_is_arm64(native_windows_machine()))
    }

    #[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
    {
        false
    }
}

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn native_windows_machine() -> Option<u16> {
    use windows::core::{s, w, BOOL};
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows::Win32::System::Threading::GetCurrentProcess;

    type IsWow64Process2 = unsafe extern "system" fn(HANDLE, *mut u16, *mut u16) -> BOOL;

    // Resolve IsWow64Process2 dynamically so merely starting Handy never raises
    // the minimum Windows version. Windows-on-ARM versions provide this API,
    // while a missing symbol or failed query safely preserves the x64 behavior.
    unsafe {
        let kernel32 = GetModuleHandleW(w!("kernel32.dll")).ok()?;
        let address = GetProcAddress(kernel32, s!("IsWow64Process2"))?;
        // SAFETY: GetProcAddress returned the documented IsWow64Process2 symbol;
        // function pointers have the same representation on supported Windows.
        let is_wow64_process2: IsWow64Process2 = std::mem::transmute(address);
        let mut process_machine = 0u16;
        let mut native_machine = 0u16;
        is_wow64_process2(
            GetCurrentProcess(),
            &mut process_machine,
            &mut native_machine,
        )
        .as_bool()
        .then_some(native_machine)
    }
}

/// Centralized cancellation function that can be called from anywhere in the app.
/// Handles cancelling both recording and transcription operations and updates UI state.
pub fn cancel_current_operation(app: &AppHandle) {
    info!("Initiating operation cancellation...");

    // Cancel any ongoing recording. v1.6.2 fleet pass 1: the cancel resolves
    // to exactly one target (local, or the most-recently-started lane) and
    // bumps only that target's cancel generation, so it can never discard
    // another session's in-flight stop pipeline.
    let audio_manager = app.state::<Arc<AudioRecordingManager>>();
    let recording_was_active = audio_manager.is_recording();
    let cancel_target = audio_manager.cancel_recording();

    // v1.6.2 fleet pass 3 (the W1-L0 admission rule): a cancelled LANE
    // session must end its companion session record exactly as the
    // pipeline's own stop does (actions.rs). Without this the record stayed
    // live: the phone kept streaming, its button stayed in the recording
    // state, and the server kept admitting audio into a recorder the cancel
    // had already removed - silently blackholed, never counted, forever.
    if let crate::managers::audio::CancelTarget::Lane(lane) = cancel_target {
        crate::companion::on_lane_session_changed(app, lane, false);
    }

    // v1.6.2 fleet pass 2: release exactly the torn-down session's want of
    // the cancel shortcut (the keyboard's None, or the cancelled lane's) -
    // the key stays armed while any other live session still wants it. The
    // torn-down session's own FinishGuard release is idempotent.
    let cancel_session_key = match cancel_target {
        crate::managers::audio::CancelTarget::Lane(lane) => Some(lane),
        _ => None,
    };
    shortcut::unregister_cancel_shortcut(app, cancel_session_key);

    // Abandon any live streaming transcription, and stop a transcription
    // already in progress - scoped to the cancelled target, because both are
    // process singletons: an unconditional cancel here would kill ANOTHER
    // lane's live stream (or its finalize) exactly when this Escape resolved
    // elsewhere. The stream is only cancelled when the target owned it; the
    // engine-wide transcription cancel stays reserved for the keyboard's
    // abort (a local resolution, or the abort-all below), never a lane's
    // own cancel. Fleet pass 2: a local resolution also skips the engine
    // cancel while any LANE pipeline is in flight - that engine work
    // belongs to the phone's pipeline, and killing it would misreport the
    // abort as a transcription failure.
    let tm = app.state::<Arc<TranscriptionManager>>();
    let owns_stream = match cancel_target {
        crate::managers::audio::CancelTarget::Local => {
            audio_manager.stream_owner() == crate::managers::audio::StreamOwner::Local
        }
        crate::managers::audio::CancelTarget::Lane(lane) => {
            audio_manager.stream_owner() == crate::managers::audio::StreamOwner::Lane(lane)
        }
        crate::managers::audio::CancelTarget::None => false,
    };
    if owns_stream {
        tm.cancel_stream();
    }
    // Fleet pass 2, the abort-all resolution: nothing is recording anymore,
    // so the only work left in the process is the in-flight stop pipelines,
    // and the Mac's Escape means "abort the current work" exactly as it did
    // in the single-session era. Bump every in-flight LANE's cancel
    // generation: each pipeline's gates (and its transcription-error arm's
    // cancelled check, which fires when the engine cancel below aborts its
    // batch mid-run) then classify the abort cleanly - no paste, no
    // spurious failure notice, no empty history row. When no lane pipeline
    // is in flight this is a no-op and the Escape stays the keyboard's own
    // abort.
    let in_flight_lanes = crate::actions::in_flight_lane_pipelines();
    if matches!(cancel_target, crate::managers::audio::CancelTarget::None) {
        audio_manager.bump_lane_cancel_generations(&in_flight_lanes);
    }
    let engine_cancel = match cancel_target {
        crate::managers::audio::CancelTarget::Lane(_) => false,
        crate::managers::audio::CancelTarget::Local => in_flight_lanes.is_empty(),
        crate::managers::audio::CancelTarget::None => true,
    };
    if engine_cancel {
        tm.cancel_transcription();
    }

    // Update tray icon and hide overlay. The multi-phone guard: the cancel
    // above resolved to exactly ONE target, and with another session still
    // live (a phone lane still recording) the shared surfaces belong to it
    // until its own teardown - an unconditional flip here showed Idle and
    // hid the card under the surviving session's feet.
    crate::actions::retire_session_surfaces(app, false);

    // Unload model if immediate unload is enabled. Done before notifying the
    // coordinator, so a recording started next always loads afresh; it
    // doesn't wait for the cancelled work, so it never blocks this thread.
    tm.maybe_unload_immediately("cancellation");

    // Notify coordinator so it can keep lifecycle state coherent.
    if let Some(coordinator) = app.try_state::<TranscriptionCoordinator>() {
        coordinator.notify_cancel(recording_was_active);
    }

    info!("Operation cancellation completed - returned to idle state");
}

/// Check if using the Wayland display server protocol
#[cfg(target_os = "linux")]
pub fn is_wayland() -> bool {
    std::env::var("WAYLAND_DISPLAY").is_ok()
        || std::env::var("XDG_SESSION_TYPE")
            .map(|v| v.to_lowercase() == "wayland")
            .unwrap_or(false)
}

/// Check if running on KDE Plasma desktop environment
#[cfg(target_os = "linux")]
pub fn is_kde_plasma() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP")
        .map(|v| v.to_uppercase().contains("KDE"))
        .unwrap_or(false)
        || std::env::var("KDE_SESSION_VERSION").is_ok()
}

/// Check if running on KDE Plasma with Wayland
#[cfg(target_os = "linux")]
pub fn is_kde_wayland() -> bool {
    is_wayland() && is_kde_plasma()
}

/// Check if running on GNOME desktop environment
#[cfg(target_os = "linux")]
pub fn is_gnome() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP")
        .map(|v| v.to_uppercase().contains("GNOME"))
        .unwrap_or(false)
}

/// Check if running on GNOME with Wayland
#[cfg(target_os = "linux")]
pub fn is_gnome_wayland() -> bool {
    is_wayland() && is_gnome()
}

/// Returns true when the environment variable is set to a truthy value
/// (e.g. "1", "true", "yes", "on").
/// "0", "false", "no", "off" and empty string are treated as falsy (case-insensitive).
/// Returns false when the variable is not set.
pub fn env_flag_enabled(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        ),
        Err(_) => false,
    }
}

/// Human-readable text from a panic payload, for fail-open logging sites.
pub fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fleet pass 1 + 2, source pin: cancel_current_operation must scope
    /// the process-singleton transcription teardown to the cancelled
    /// target - cancel_stream only when the target owned the stream, the
    /// engine-wide cancel_transcription never for a lane-resolved cancel
    /// and never for a local resolution while any LANE pipeline is in
    /// flight (that engine work belongs to the phone's pipeline; killing it
    /// misreports the abort as a transcription failure), and the
    /// abort-all resolution (nothing recording, pipelines in flight) bumps
    /// exactly the in-flight lanes' generations so those pipelines cancel
    /// cleanly. Unconditional calls cross-fired into whichever other lane
    /// was live (the same bug the per-lane cancel generation closes on the
    /// capture side). Pinned against this committed source because
    /// mounting an AppHandle in a unit test is not possible (same
    /// convention as the audio manager's cancel pins).
    #[test]
    fn cancel_current_operation_scopes_singletons_to_the_cancel_target() {
        let source = include_str!("utils.rs");
        let fn_at = source
            .find("pub fn cancel_current_operation")
            .expect("cancel_current_operation");
        let body_end = fn_at + source[fn_at..].find("\n}\n").expect("fn end");
        let body = &source[fn_at..body_end];
        assert!(
            body.contains("let cancel_target = audio_manager.cancel_recording();"),
            "the cancel's resolved target must be captured"
        );
        assert!(
            body.contains("if owns_stream {") && body.contains("tm.cancel_stream();"),
            "the stream singleton must be cancelled only when the cancelled target owned it"
        );
        // Fleet pass 2: the engine cancel rule and the abort-all resolution.
        assert!(
            body.contains("let engine_cancel = match cancel_target {"),
            "the engine-wide cancel must be a per-resolution decision, not an unconditional call"
        );
        assert!(
            body.contains("CancelTarget::Local => in_flight_lanes.is_empty(),"),
            "a local-resolved cancel must not kill a lane pipeline's in-flight engine work"
        );
        assert!(
            body.contains("audio_manager.bump_lane_cancel_generations(&in_flight_lanes);"),
            "the abort-all resolution (nothing recording, only pipelines left) must bump the in-flight lanes' generations so they cancel cleanly"
        );
        assert!(
            body.contains("shortcut::unregister_cancel_shortcut(app, cancel_session_key);"),
            "the cancel releases exactly the torn-down session's want; another live session's Escape stays armed"
        );
        // Multi-phone review round 2: the shared surfaces retire through
        // the guarded helper, never an unconditional flip. The cancel
        // resolves to ONE target (pinned above); with phones A and B both
        // live, the Escape tears down B (most recent) and an unconditional
        // Idle-plus-hide retired the tray and card under A's live session.
        assert!(
            body.contains("crate::actions::retire_session_surfaces(app, false);"),
            "the surfaces must retire only when no other session is live (the guarded helper), not unconditionally"
        );
        assert!(
            !body.contains("TrayIconState::Idle"),
            "no direct tray flip may live in the cancel route"
        );
        assert!(
            !body.contains("hide_recording_overlay"),
            "no direct overlay hide may live in the cancel route"
        );
    }

    /// Fleet pass 3, source pin (the W1-L0 admission rule): a cancel that
    /// resolves to a LANE must end that lane's companion session record
    /// through the same on_lane_session_changed(lane, false) call the
    /// pipeline's stop uses - it flips the record's liveness (the server's
    /// admission truth), sends the phone State{recording:false}, and
    /// clears the overlay badge. Before this the record stayed live
    /// forever: the phone kept streaming into the recorder the cancel had
    /// removed, its UI stayed in the recording state, and the chunks were
    /// dropped silently without even counting as out-of-session. Pinned
    /// against this committed source because mounting an AppHandle in a
    /// unit test is not possible (same convention as the pin above).
    #[test]
    fn cancel_current_operation_ends_a_cancelled_lanes_companion_session() {
        let source = include_str!("utils.rs");
        let fn_at = source
            .find("pub fn cancel_current_operation")
            .expect("cancel_current_operation");
        let body_end = fn_at + source[fn_at..].find("\n}\n").expect("fn end");
        let body = &source[fn_at..body_end];
        assert!(
            body.contains("if let crate::managers::audio::CancelTarget::Lane(lane) = cancel_target"),
            "the lane-resolved cancel must be branched on before the teardown below"
        );
        assert!(
            body.contains("crate::companion::on_lane_session_changed(app, lane, false);"),
            "a lane-resolved cancel must end the lane's companion session record (liveness flip + State{{recording:false}} to the phone), exactly as the pipeline's stop does"
        );
    }

    #[test]
    fn arm64_native_machine_is_the_only_match() {
        assert!(native_machine_is_arm64(Some(IMAGE_FILE_MACHINE_ARM64)));
        assert!(!native_machine_is_arm64(Some(0x8664))); // AMD64
        assert!(!native_machine_is_arm64(Some(0x014c))); // I386
        assert!(!native_machine_is_arm64(None)); // API unavailable or failed
    }

    #[test]
    fn env_flag_enabled_true_for_truthy_values() {
        for value in ["1", "true", "TRUE", "yes", "on", " 1 "] {
            std::env::set_var("HANDY_TEST_FLAG_TRUTHY", value);
            assert!(env_flag_enabled("HANDY_TEST_FLAG_TRUTHY"), "{value:?}");
        }
        std::env::remove_var("HANDY_TEST_FLAG_TRUTHY");
    }

    #[test]
    fn env_flag_enabled_false_for_falsy_or_unset() {
        assert!(!env_flag_enabled("HANDY_TEST_FLAG_UNSET"));

        for value in ["0", "false", "FALSE", "no", "off", ""] {
            std::env::set_var("HANDY_TEST_FLAG_FALSY", value);
            assert!(!env_flag_enabled("HANDY_TEST_FLAG_FALSY"), "{value:?}");
        }
        std::env::remove_var("HANDY_TEST_FLAG_FALSY");
    }
}
