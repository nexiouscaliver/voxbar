//! Launch-at-login (autostart) handling.
//!
//! macOS 13+ registers itself as a login item via `SMAppService` (the
//! plugin's launch agent plist carries no app association, so the System
//! Settings Login Items pane attributes it to the code-signing certificate's
//! developer name instead of the app, #337). Linux writes its own
//! `~/.config/autostart/VoxBar.desktop`: tauri-plugin-autostart (via
//! auto-launch 0.5.0) emits an unquoted `Exec=` line, which breaks
//! launch-at-login whenever the install path contains spaces, and the local
//! writer also honors the APPIMAGE path the plugin provided. Every other
//! platform applies the setting through tauri-plugin-autostart.

use tauri::AppHandle;
use tauri_plugin_autostart::ManagerExt;

/// Apply the user's autostart preference using the best mechanism for the
/// current platform.
///
/// Errors are logged rather than returned: the preference is re-applied on
/// every launch, so a transient failure self-heals and must not block
/// startup. This mirrors the pre-existing behavior of ignoring
/// enable()/disable() results.
pub fn apply_autostart(app: &AppHandle, enabled: bool) {
    #[cfg(target_os = "macos")]
    if macos::login_item_api_available() {
        macos::remove_plugin_launch_agent(app);
        macos::set_login_item(enabled);
        return;
    }

    #[cfg(target_os = "linux")]
    {
        linux::apply(app, enabled);
        return;
    }

    let manager = app.autolaunch();
    let result = if enabled {
        manager.enable()
    } else {
        manager.disable()
    };
    if let Err(e) = result {
        log::warn!(
            "Failed to apply autostart setting (enabled={}): {}",
            enabled,
            e
        );
    }
}

/// Pure .desktop content builders for XDG autostart. Compiled on every
/// platform so the Exec-quoting contract is unit-testable anywhere; only the
/// filesystem work that feeds them is Linux-gated.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod desktop_entry {
    /// Quote one Exec argument per the Desktop Entry Specification when it
    /// contains whitespace: `"..."` keeps it a single argument. Embedded
    /// quotes and backslashes are backslash-escaped as the spec requires.
    /// Plain paths stay unquoted, matching what the ecosystem writes.
    pub(crate) fn quote_exec_arg(arg: &str) -> String {
        if !arg
            .chars()
            .any(|c| c.is_whitespace() || c == '"' || c == '\\')
        {
            return arg.to_string();
        }
        let mut quoted = String::with_capacity(arg.len() + 2);
        quoted.push('"');
        for c in arg.chars() {
            if c == '"' || c == '\\' {
                quoted.push('\\');
            }
            quoted.push(c);
        }
        quoted.push('"');
        quoted
    }

    /// The .desktop file content for launch-at-login. The `Exec` line must
    /// survive install paths with spaces (auto-launch 0.5.0 wrote it
    /// unquoted, which split the path at the first space and broke startup).
    pub(crate) fn desktop_entry_content(app_name: &str, exe_path: &str, args: &[&str]) -> String {
        let mut exec = quote_exec_arg(exe_path);
        for arg in args {
            exec.push(' ');
            exec.push_str(&quote_exec_arg(arg));
        }
        format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Version=1.0\n\
             Name={}\n\
             Comment={} startup script\n\
             Exec={}\n\
             StartupNotify=false\n\
             Terminal=false\n",
            app_name, app_name, exec
        )
    }

    /// The executable launch-at-login should start: an AppImage run must
    /// relaunch the AppImage itself (its mounted binary disappears with the
    /// mount), anything else relaunches the current binary.
    pub(crate) fn launch_target(
        appimage: Option<&str>,
        current_exe: Option<&str>,
    ) -> Option<String> {
        appimage
            .map(|path| path.to_string())
            .or_else(|| current_exe.map(|path| path.to_string()))
    }
}

/// XDG autostart writer (Linux).
#[cfg(target_os = "linux")]
mod linux {
    use std::path::PathBuf;

    use tauri::{AppHandle, Manager};

    use super::desktop_entry::{desktop_entry_content, launch_target};

    /// Path of the autostart entry. Same filename the plugin used
    /// (`{package name}.desktop`), so enabling replaces any file the plugin
    /// wrote earlier instead of doubling the login item.
    fn autostart_file(app: &AppHandle) -> Option<PathBuf> {
        let home = app.path().home_dir().ok()?;
        Some(
            home.join(".config")
                .join("autostart")
                .join(format!("{}.desktop", app.package_info().name)),
        )
    }

    /// Apply the setting: write (or remove) the autostart entry, creating
    /// the directory on demand. Errors are logged by the caller's contract.
    pub(crate) fn apply(app: &AppHandle, enabled: bool) {
        let Some(file) = autostart_file(app) else {
            log::warn!("autostart: could not resolve the autostart file path");
            return;
        };
        if !enabled {
            match std::fs::remove_file(&file) {
                Ok(()) => log::info!("Removed autostart entry {:?}", file),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => log::warn!("Failed to remove autostart entry {:?}: {}", file, e),
            }
            return;
        }
        let appimage = app
            .env()
            .appimage
            .as_ref()
            .and_then(|p| p.to_str())
            .map(|s| s.to_string());
        let current_exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.to_str().map(|s| s.to_string()));
        let Some(exe_path) = launch_target(appimage.as_deref(), current_exe.as_deref()) else {
            log::warn!("autostart: could not resolve the executable path");
            return;
        };
        let content = desktop_entry_content(&app.package_info().name, &exe_path, &[]);
        if let Some(dir) = file.parent() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                log::warn!("Failed to create autostart directory {:?}: {}", dir, e);
                return;
            }
        }
        match std::fs::write(&file, content) {
            Ok(()) => log::info!("Wrote autostart entry {:?} (exec {:?})", file, exe_path),
            Err(e) => log::warn!("Failed to write autostart entry {:?}: {}", file, e),
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::path::{Path, PathBuf};

    use objc2::runtime::AnyClass;
    use objc2_service_management::{SMAppService, SMAppServiceStatus};
    use tauri::{AppHandle, Manager};

    /// `SMAppService` requires macOS 13. The ServiceManagement framework is
    /// linked unconditionally (it has existed since 10.6), so looking up the
    /// class doubles as the OS version check: present exactly when the API is
    /// usable.
    pub fn login_item_api_available() -> bool {
        AnyClass::get(c"SMAppService").is_some()
    }

    /// Register or unregister the app as a login item, skipping the call when
    /// the service is already in the requested state (unregistering a
    /// never-registered service returns an error on every launch otherwise).
    pub fn set_login_item(enabled: bool) {
        let service = unsafe { SMAppService::mainAppService() };
        let status = unsafe { service.status() };

        if enabled {
            if status == SMAppServiceStatus::Enabled {
                return;
            }
            match unsafe { service.registerAndReturnError() } {
                Ok(()) => log::info!("Registered login item via SMAppService"),
                // Fails in dev (no signed app bundle) and when the user has
                // switched the item off in System Settings, which apps are
                // not allowed to override.
                Err(e) => log::warn!("Failed to register login item: {}", e),
            }
        } else {
            if status == SMAppServiceStatus::NotRegistered || status == SMAppServiceStatus::NotFound
            {
                return;
            }
            match unsafe { service.unregisterAndReturnError() } {
                Ok(()) => log::info!("Unregistered login item via SMAppService"),
                Err(e) => log::warn!("Failed to unregister login item: {}", e),
            }
        }
    }

    /// Remove the launch agent plist that tauri-plugin-autostart (via the
    /// auto-launch crate) wrote on older versions, so login doesn't start the
    /// app twice after migrating to `SMAppService`. Runs on every launch;
    /// missing file is the normal case.
    pub fn remove_plugin_launch_agent(app: &AppHandle) {
        let Ok(home) = app.path().home_dir() else {
            return;
        };
        remove_launch_agent_file(&plugin_launch_agent_path(&home, &app.package_info().name));
    }

    /// Path of the plist the auto-launch crate writes:
    /// `~/Library/LaunchAgents/{app name}.plist`.
    fn plugin_launch_agent_path(home: &Path, app_name: &str) -> PathBuf {
        home.join("Library")
            .join("LaunchAgents")
            .join(format!("{}.plist", app_name))
    }

    fn remove_launch_agent_file(path: &Path) {
        match std::fs::remove_file(path) {
            Ok(()) => log::info!("Removed legacy autostart launch agent {:?}", path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!("Failed to remove legacy launch agent {:?}: {}", path, e),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Validates the assumption `login_item_api_available` rests on: the
        /// ServiceManagement framework is linked into the binary, so the
        /// class lookup finds `SMAppService` whenever the host is macOS 13+
        /// (which anything able to build this crate is).
        #[test]
        fn sm_app_service_class_resolves() {
            assert!(login_item_api_available());
        }

        #[test]
        fn launch_agent_path_matches_auto_launch_crate() {
            let path = plugin_launch_agent_path(Path::new("/Users/someone"), "Handy");
            assert_eq!(
                path,
                Path::new("/Users/someone/Library/LaunchAgents/Handy.plist")
            );
        }

        #[test]
        fn removes_existing_launch_agent() {
            let dir = tempfile::tempdir().unwrap();
            let plist = dir.path().join("Handy.plist");
            std::fs::write(&plist, "<plist/>").unwrap();

            remove_launch_agent_file(&plist);
            assert!(!plist.exists());
        }

        #[test]
        fn missing_launch_agent_is_a_no_op() {
            let dir = tempfile::tempdir().unwrap();
            remove_launch_agent_file(&dir.path().join("Handy.plist"));
        }
    }
}

#[cfg(test)]
mod desktop_entry_tests {
    use super::desktop_entry::{desktop_entry_content, launch_target, quote_exec_arg};

    /// An install path containing spaces must produce a spec-valid quoted
    /// Exec line: one double-quoted argument, so the desktop environment
    /// does not split it at the first space (auto-launch 0.5.0 wrote it
    /// unquoted and broke launch-at-login for such installs).
    #[test]
    fn exec_line_quotes_paths_with_spaces() {
        let content = desktop_entry_content("VoxBar", "/opt/My Apps/VoxBar/voxbar", &[]);
        let exec = content
            .lines()
            .find(|l| l.starts_with("Exec="))
            .expect("Exec line present");
        assert_eq!(
            exec, "Exec=\"/opt/My Apps/VoxBar/voxbar\"",
            "the whole path must stay one quoted argument"
        );
        assert!(content.contains("Type=Application"));
        assert!(content.contains("Name=VoxBar"));
    }

    /// Plain paths stay unquoted (what every other autostart writer emits)
    /// and extra startup args ride along as separate arguments.
    #[test]
    fn plain_paths_stay_unquoted_and_args_append() {
        assert_eq!(quote_exec_arg("/usr/bin/voxbar"), "/usr/bin/voxbar");
        let content = desktop_entry_content("VoxBar", "/usr/bin/voxbar", &["--start-hidden"]);
        assert!(content.contains("Exec=/usr/bin/voxbar --start-hidden"));
    }

    /// Embedded quotes and backslashes are escaped per the Desktop Entry
    /// Spec instead of silently corrupting the line.
    #[test]
    fn embedded_quotes_and_backslashes_are_escaped() {
        assert_eq!(
            quote_exec_arg("/opt/we\"ird\\path"),
            "\"/opt/we\\\"ird\\\\path\""
        );
    }

    /// The APPIMAGE path wins over the current executable: an AppImage's
    /// mounted binary vanishes with the mount, so login must relaunch the
    /// AppImage file itself. Without an AppImage, the current binary is
    /// used.
    #[test]
    fn appimage_path_is_preserved_over_the_mounted_binary() {
        assert_eq!(
            launch_target(
                Some("/home/user/Apps/Vox Bar/VoxBar.AppImage"),
                Some("/tmp/.mount_VoxBar/usr/bin/voxbar")
            )
            .as_deref(),
            Some("/home/user/Apps/Vox Bar/VoxBar.AppImage")
        );
        assert_eq!(
            launch_target(None, Some("/usr/bin/voxbar")).as_deref(),
            Some("/usr/bin/voxbar")
        );
        assert_eq!(launch_target(None, None), None);
    }

    /// An AppImage living in a spaced path must come out quoted in the Exec
    /// line (the two contracts compose).
    #[test]
    fn appimage_with_spaces_end_to_end() {
        let target = launch_target(
            Some("/home/user/Apps/Vox Bar/VoxBar.AppImage"),
            Some("/tmp/.mount_VoxBar/usr/bin/voxbar"),
        )
        .unwrap();
        let content = desktop_entry_content("VoxBar", &target, &[]);
        assert!(content.contains("Exec=\"/home/user/Apps/Vox Bar/VoxBar.AppImage\""));
    }
}
