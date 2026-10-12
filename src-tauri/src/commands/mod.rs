pub mod audio;
pub mod history;
pub mod local_llm;
pub mod models;
pub mod transcription;
pub mod updater;

use crate::settings::{
    get_settings, update_checks_forced_disabled, write_settings, AppSettings, LogLevel,
};
use crate::utils::cancel_current_operation;
use tauri::{AppHandle, Manager};
use tauri_plugin_opener::OpenerExt;

#[tauri::command]
#[specta::specta]
pub fn cancel_operation(app: AppHandle) {
    cancel_current_operation(&app);
}

#[tauri::command]
#[specta::specta]
pub fn is_portable() -> bool {
    crate::portable::is_portable()
}

#[tauri::command]
#[specta::specta]
pub fn is_update_checks_locked() -> bool {
    update_checks_forced_disabled()
}

#[tauri::command]
#[specta::specta]
pub fn get_app_dir_path(app: AppHandle) -> Result<String, String> {
    let app_data_dir = crate::portable::app_data_dir(&app)
        .map_err(|e| format!("Failed to get app data directory: {}", e))?;

    Ok(app_data_dir.to_string_lossy().to_string())
}

#[tauri::command]
#[specta::specta]
pub fn get_app_settings(app: AppHandle) -> Result<AppSettings, String> {
    Ok(get_settings(&app))
}

#[tauri::command]
#[specta::specta]
pub fn get_default_settings() -> Result<AppSettings, String> {
    Ok(crate::settings::get_default_settings())
}

/// Whether the app is running from macOS's read-only translocation mount.
/// The frontend shows the move-to-Applications guidance when true; the
/// updater flow also uses it to make install failures actionable.
#[tauri::command]
#[specta::specta]
pub fn is_app_translocated() -> Result<bool, String> {
    Ok(crate::portable::app_is_translocated())
}

#[tauri::command]
#[specta::specta]
pub fn get_log_dir_path(app: AppHandle) -> Result<String, String> {
    let log_dir = crate::portable::app_log_dir(&app)
        .map_err(|e| format!("Failed to get log directory: {}", e))?;

    Ok(log_dir.to_string_lossy().to_string())
}

#[specta::specta]
#[tauri::command]
pub fn set_log_level(app: AppHandle, level: LogLevel) -> Result<(), String> {
    let tauri_log_level: tauri_plugin_log::LogLevel = level.into();
    let log_level: log::Level = tauri_log_level.into();
    // Update the file log level atomic so the filter picks up the new level
    crate::FILE_LOG_LEVEL.store(
        log_level.to_level_filter() as u8,
        std::sync::atomic::Ordering::Relaxed,
    );

    let mut settings = get_settings(&app);
    settings.log_level = level;
    write_settings(&app, settings);

    Ok(())
}

#[specta::specta]
#[tauri::command]
pub fn open_recordings_folder(app: AppHandle) -> Result<(), String> {
    let app_data_dir = crate::portable::app_data_dir(&app)
        .map_err(|e| format!("Failed to get app data directory: {}", e))?;

    let recordings_dir = app_data_dir.join("recordings");

    // KB-181: open the folder itself (the label says Open, so Finder shows
    // the recordings). The earlier reveal-instead-of-open was an ACL
    // workaround for the FRONTEND plugin command; this Rust-side OpenerExt
    // call bypasses that ACL entirely, and the opener:allow-open-path
    // capability is granted alongside so any future direct use is legal
    // too. Reveal selected the folder inside its parent - technically
    // visible, but every "Open ... Folder" button behaved like Show.
    app.opener()
        .open_path(recordings_dir.to_string_lossy(), None::<&str>)
        .map_err(|e| format!("Failed to open recordings folder: {}", e))?;

    Ok(())
}

#[specta::specta]
#[tauri::command]
pub fn open_log_dir(app: AppHandle) -> Result<(), String> {
    let log_dir = crate::portable::app_log_dir(&app)
        .map_err(|e| format!("Failed to get log directory: {}", e))?;

    // Open, matching the button's label: see open_recordings_folder (KB-181).
    app.opener()
        .open_path(log_dir.to_string_lossy(), None::<&str>)
        .map_err(|e| format!("Failed to open log directory: {}", e))?;

    Ok(())
}

#[specta::specta]
#[tauri::command]
pub fn open_app_data_dir(app: AppHandle) -> Result<(), String> {
    let app_data_dir = crate::portable::app_data_dir(&app)
        .map_err(|e| format!("Failed to get app data directory: {}", e))?;

    // Open, matching the button's label: see open_recordings_folder (KB-181).
    app.opener()
        .open_path(app_data_dir.to_string_lossy(), None::<&str>)
        .map_err(|e| format!("Failed to open app data directory: {}", e))?;

    Ok(())
}

/// Check if Apple Intelligence is available on this device.
/// Called by the frontend when the user selects Apple Intelligence provider.
#[specta::specta]
#[tauri::command]
pub fn check_apple_intelligence_available() -> bool {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        crate::apple_intelligence::check_apple_intelligence_availability()
    }
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    {
        false
    }
}

/// Try to initialize Enigo (keyboard/mouse simulation).
/// On macOS, this will return an error if accessibility permissions are not granted.
#[specta::specta]
#[tauri::command]
pub fn initialize_enigo(app: AppHandle) -> Result<(), String> {
    use crate::input::EnigoState;

    // Check if already initialized
    if app.try_state::<EnigoState>().is_some() {
        log::debug!("Enigo already initialized");
        return Ok(());
    }

    // Try to initialize
    match EnigoState::new() {
        Ok(enigo_state) => {
            app.manage(enigo_state);
            log::info!("Enigo initialized successfully after permission grant");
            Ok(())
        }
        Err(e) => {
            if cfg!(target_os = "macos") {
                log::warn!(
                    "Failed to initialize Enigo: {} (accessibility permissions may not be granted)",
                    e
                );
            } else {
                log::warn!("Failed to initialize Enigo: {}", e);
            }
            Err(format!("Failed to initialize input system: {}", e))
        }
    }
}

/// Marker state to track if shortcuts have been initialized.
pub struct ShortcutsInitialized;

/// Initialize keyboard shortcuts.
/// On macOS, this should be called after accessibility permissions are granted.
/// This is idempotent - calling it multiple times is safe.
#[specta::specta]
#[tauri::command]
pub fn initialize_shortcuts(app: AppHandle) -> Result<(), String> {
    // Check if already initialized
    if app.try_state::<ShortcutsInitialized>().is_some() {
        log::debug!("Shortcuts already initialized");
        return Ok(());
    }

    // Initialize shortcuts
    crate::shortcut::init_shortcuts(&app);

    // Mark as initialized before reconciling the macOS Secure Input fallback.
    app.manage(ShortcutsInitialized);
    crate::secure_input::reconcile_fallback(&app);

    log::info!("Shortcuts initialized successfully");
    Ok(())
}

#[cfg(test)]
mod tests {
    /// KB-181 pin: every directory button that says "Open" actually opens
    /// the directory (Finder shows its CONTENTS), instead of revealing the
    /// folder selected inside its parent. The opener calls need a live app
    /// a unit test cannot mount, so the three commands are pinned against
    /// this committed source - the same convention as the overlay and
    /// persistent-notice wiring pins.
    #[test]
    fn kb181_directory_buttons_open_instead_of_reveal() {
        let source = include_str!("mod.rs");
        for fn_name in [
            "pub fn open_recordings_folder(",
            "pub fn open_log_dir(",
            "pub fn open_app_data_dir(",
        ] {
            let at = source
                .find(fn_name)
                .unwrap_or_else(|| panic!("{fn_name} must exist in commands/mod.rs"));
            let rest = &source[at..];
            let end = rest.find("\n}\n").map(|i| i + 3).unwrap_or(rest.len());
            let body = &rest[..end];
            assert!(
                body.contains(".open_path("),
                "{fn_name} must open the directory itself"
            );
            assert!(
                !body.contains("reveal_item_in_dir"),
                "{fn_name} must not reveal (select) the directory inside its parent"
            );
        }
    }
}
