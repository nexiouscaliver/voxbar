//! One-time migration from a legacy Handy install to this VoxBar install.
//!
//! T2 renamed `productName` and `identifier`, so a VoxBar build installed
//! over an existing Handy install resolves a NEW app-data dir
//! (`com.voxbar.app` next to the legacy `com.pais.handy`) and would
//! otherwise start fresh: re-onboarding, model re-download, lost history.
//! On the first run after the rename — before anything in `.setup()` has
//! written into the new dir — this module moves the user's data over
//! per-item and idempotently:
//!
//! - `settings_store.json` ([`SETTINGS_STORE_PATH`]) — the no-store trigger
//!   and the first item moved; migrating it also carries
//!   `autostart_enabled`, so login-item re-registration happens
//!   automatically later in `initialize_core_logic` → `apply_autostart`.
//! - `models/` (the model library; same-volume rename keeps this instant).
//! - `history.db` + `recordings/` (both resolve under the app data dir,
//!   `managers/history.rs`).
//! - logs, best-effort: plugin-init logging may have already created the
//!   new log file before this hook runs, in which case skip-if-present
//!   semantics simply leave the old logs in place.
//!
//! Semantics: skip items already present at the target; prefer same-volume
//! rename with a recursive-copy fallback; never delete anything on the
//! source side, so a partial failure leaves the legacy dir intact and the
//! next launch retries the missing items.
//!
//! Portable installs are unaffected: their `Data/` dir lives next to the
//! executable (`portable.rs`), so an in-place upgrade keeps using it —
//! this module skips them entirely.
//!
//! Legacy autostart entries are also cleaned up where derivable. On macOS
//! the pre-SMAppService launch agent
//! `~/Library/LaunchAgents/{Handy}.plist` is removed when present (the
//! every-launch cleanup in `autostart.rs` is keyed to the NEW product name
//! and can never remove it). DOCUMENTED RESIDUAL, not code-fixable: a
//! Handy install that registered itself through `SMAppService` (macOS 13+)
//! leaves that registration orphaned — SMAppService manages only its own
//! bundle, so the new app cannot unregister it. While the old `Handy.app`
//! remains installed the registration is functional (the old app launches
//! at login); removing it requires uninstalling the old app or toggling it
//! off in System Settings → General → Login Items. This is logged at
//! migration time and called out in the release notes.
//!
//! The old identity is pinned here and nowhere else: post-T2, in code and
//! config the legacy identifier's only home is this module (every other
//! occurrence is documentation).

use std::path::{Path, PathBuf};

use tauri::{AppHandle, Manager};

use crate::portable;
use crate::settings::SETTINGS_STORE_PATH;

/// Identity of the pre-rebrand release this migration understands.
pub const LEGACY_PRODUCT_NAME: &str = "Handy";
pub const LEGACY_IDENTIFIER: &str = "com.pais.handy";

/// Written into the new data dir while a migration pass left failed items,
/// so the next launch retries them even though `settings_store.json` —
/// migrated first — already exists and would otherwise satisfy the
/// no-store trigger. Cleared by the first pass with no failures.
pub const MIGRATION_PENDING_MARKER: &str = ".legacy-migration-pending";

/// Legacy autostart value names / file stems (see `legacy_autostart_paths`).
#[cfg(target_os = "windows")]
pub const LEGACY_WINDOWS_RUN_VALUE: &str = "Handy";

/// Relative names of the migrated items inside the app data dir. These
/// mirror the live resolution in `managers/history.rs` (`history.db`,
/// `recordings/`) and the model manager (`models/`).
const MODELS_DIR: &str = "models";
const HISTORY_DB: &str = "history.db";
const RECORDINGS_DIR: &str = "recordings";

/// The items this migration moves, in order. Kept as a pure fn (C4) so the
/// exact set is unit-testable without touching the filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationItem {
    SettingsStore,
    Models,
    HistoryDb,
    Recordings,
    Logs,
}

impl MigrationItem {
    pub fn label(&self) -> &'static str {
        match self {
            MigrationItem::SettingsStore => SETTINGS_STORE_PATH,
            MigrationItem::Models => MODELS_DIR,
            MigrationItem::HistoryDb => HISTORY_DB,
            MigrationItem::Recordings => RECORDINGS_DIR,
            MigrationItem::Logs => "logs",
        }
    }
}

/// The exact item set migrated from a legacy install, in execution order.
pub fn migration_items() -> [MigrationItem; 5] {
    [
        MigrationItem::SettingsStore,
        MigrationItem::Models,
        MigrationItem::HistoryDb,
        MigrationItem::Recordings,
        MigrationItem::Logs,
    ]
}

/// Legacy app-data dir for `home`, following the same per-OS rules tauri
/// uses for `app_data_dir()` (tauri `path/desktop.rs`: `dirs::data_dir()` +
/// identifier): `~/Library/Application Support` on macOS,
/// `~/.local/share` (XDG data) on Linux, `%APPDATA%` (Roaming) on Windows —
/// with the LEGACY identifier. (`dirs::data_dir()` on Linux is
/// `$XDG_DATA_HOME`/`~/.local/share`, NOT `~/.config` — the pre-rebrand
/// README's `~/.config/com.pais.handy` table row was wrong.)
pub fn legacy_data_dir(home: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    let base = home.join("Library").join("Application Support");
    #[cfg(target_os = "linux")]
    let base = home.join(".local").join("share");
    #[cfg(target_os = "windows")]
    let base = home.join("AppData").join("Roaming");
    base.join(LEGACY_IDENTIFIER)
}

/// Legacy log dir for `home`, following tauri's `app_log_dir()` rules
/// (tauri `path/desktop.rs`): `~/Library/Logs/{identifier}` on macOS,
/// `dirs::data_local_dir()/{identifier}/logs` elsewhere — with the LEGACY
/// identifier.
pub fn legacy_log_dir(home: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        home.join("Library").join("Logs").join(LEGACY_IDENTIFIER)
    }
    #[cfg(not(target_os = "macos"))]
    {
        #[cfg(target_os = "linux")]
        let base = home.join(".local").join("share");
        #[cfg(target_os = "windows")]
        let base = home.join("AppData").join("Local");
        base.join(LEGACY_IDENTIFIER).join("logs")
    }
}

/// Resolve the legacy data dir for a given (`new_dir`, `home`) pair, or
/// `None` when the new dir already IS the legacy dir (identifier unchanged
/// or a shared/portable dir) — nothing to migrate from in that case.
pub fn resolve_legacy_data_dir(new_dir: &Path, home: &Path) -> Option<PathBuf> {
    let legacy = legacy_data_dir(home);
    if &legacy == new_dir {
        None
    } else {
        Some(legacy)
    }
}

/// Source path of `item` under the legacy roots.
pub fn item_source(item: MigrationItem, legacy_data: &Path, legacy_logs: &Path) -> PathBuf {
    match item {
        MigrationItem::SettingsStore => legacy_data.join(SETTINGS_STORE_PATH),
        MigrationItem::Models => legacy_data.join(MODELS_DIR),
        MigrationItem::HistoryDb => legacy_data.join(HISTORY_DB),
        MigrationItem::Recordings => legacy_data.join(RECORDINGS_DIR),
        MigrationItem::Logs => legacy_logs.to_path_buf(),
    }
}

/// Target path of `item` under the new roots.
pub fn item_target(item: MigrationItem, new_data: &Path, new_logs: &Path) -> PathBuf {
    match item {
        MigrationItem::SettingsStore => new_data.join(SETTINGS_STORE_PATH),
        MigrationItem::Models => new_data.join(MODELS_DIR),
        MigrationItem::HistoryDb => new_data.join(HISTORY_DB),
        MigrationItem::Recordings => new_data.join(RECORDINGS_DIR),
        MigrationItem::Logs => new_logs.to_path_buf(),
    }
}

/// Outcome of one migration pass.
#[derive(Debug, Default)]
pub struct MigrationReport {
    pub migrated: Vec<MigrationItem>,
    pub skipped: Vec<MigrationItem>,
    pub failed: Vec<(MigrationItem, String)>,
}

impl MigrationReport {
    pub fn is_noop(&self) -> bool {
        self.migrated.is_empty() && self.failed.is_empty()
    }
}

/// Move every item from the legacy roots to the new roots, per-item:
/// target already present → skip; source missing → skip (nothing to do);
/// otherwise prefer a same-volume `rename` (instant for the multi-GB
/// `models/` dir) and fall back to a recursive copy. The source side is
/// never deleted, so a partial failure leaves the legacy dir intact.
///
/// Retry semantics: when a pass leaves failures, a marker file is written
/// into the new data dir ([`MIGRATION_PENDING_MARKER`]) and cleared once a
/// pass completes without failures. The first-run hook in
/// [`run_first_run_migration`] bypasses its no-store early-return while the
/// marker exists — without it the trigger would never re-fire, because
/// SettingsStore migrates first and its presence is what the trigger keys
/// on.
pub fn migrate_data(
    legacy_data: &Path,
    legacy_logs: &Path,
    new_data: &Path,
    new_logs: &Path,
) -> MigrationReport {
    let mut report = MigrationReport::default();

    for item in migration_items() {
        let src = item_source(item, legacy_data, legacy_logs);
        let dst = item_target(item, new_data, new_logs);

        if dst.exists() || !src.exists() {
            report.skipped.push(item);
            continue;
        }

        if let Some(parent) = dst.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                report
                    .failed
                    .push((item, format!("create {}: {e}", parent.display())));
                continue;
            }
        }

        match move_item(&src, &dst) {
            Ok(()) => report.migrated.push(item),
            Err(e) => report.failed.push((item, e.to_string())),
        }
    }

    // Write/clear the retry marker after the pass. Failures here are
    // best-effort: a marker that cannot be written merely means the failed
    // items are not retried (the pre-fix behavior), never a startup error.
    let marker = new_data.join(MIGRATION_PENDING_MARKER);
    if report.failed.is_empty() {
        let _ = std::fs::remove_file(&marker);
    } else {
        let _ = std::fs::create_dir_all(new_data);
        let _ = std::fs::write(&marker, "pending\n");
    }

    report
}

/// Rename `src` to `dst`; on any error (e.g. cross-volume) fall back to a
/// recursive copy that leaves `src` in place.
fn move_item(src: &Path, dst: &Path) -> std::io::Result<()> {
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(rename_err) => copy_item(src, dst).map_err(|copy_err| {
            // Surface the original rename error with the copy failure as
            // context — the fallback is the unusual path.
            std::io::Error::other(format!(
                "rename failed: {rename_err}; copy fallback failed: {copy_err}"
            ))
        }),
    }
}

/// Recursive copy used as the cross-volume fallback (and by tests).
fn copy_item(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        let mut entries = std::fs::read_dir(src)?;
        entries.try_for_each(|entry| {
            let entry = entry?;
            copy_item(&entry.path(), &dst.join(entry.file_name()))
        })
    } else {
        std::fs::copy(src, dst).map(|_| ())
    }
}

/// Path of the launch-agent plist the auto-launch crate wrote for
/// `product_name` (`~/Library/LaunchAgents/{name}.plist`, matching
/// `autostart.rs`). Pre-SMAppService Handy installs may still carry one.
pub fn legacy_launch_agent_path(home: &Path, product_name: &str) -> PathBuf {
    home.join("Library")
        .join("LaunchAgents")
        .join(format!("{product_name}.plist"))
}

/// Legacy autostart entries derivable as filesystem paths on this OS.
///
/// - macOS: the pre-SMAppService `Handy.plist` launch agent.
/// - Linux: the auto-launch crate's `~/.config/autostart` desktop entries
///   (both name casings, idempotent to attempt).
/// - Windows: the legacy entry is a Run-key registry VALUE, not a path —
///   exposed as [`LEGACY_WINDOWS_RUN_VALUE`]; removal is not attempted
///   (untestable tier, logged and skipped per plan).
pub fn legacy_autostart_paths(home: &Path) -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        vec![legacy_launch_agent_path(home, LEGACY_PRODUCT_NAME)]
    }
    #[cfg(target_os = "linux")]
    {
        let autostart = home.join(".config").join("autostart");
        vec![
            autostart.join(format!("{LEGACY_PRODUCT_NAME}.desktop")),
            autostart.join(format!("{}.desktop", LEGACY_PRODUCT_NAME.to_lowercase())),
        ]
    }
    #[cfg(target_os = "windows")]
    {
        let _ = home;
        Vec::new()
    }
}

/// Remove a legacy launch-agent plist. `true` when a file was removed,
/// `false` when absent (the normal case) — a no-op either way.
pub fn remove_legacy_launch_agent(path: &Path) -> bool {
    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            log::warn!(
                "[legacy-migration] failed to remove legacy launch agent {}: {e}",
                path.display()
            );
            false
        }
    }
}

/// Best-effort removal of the legacy autostart entries for this OS.
/// Returns the removed paths. Non-macOS tiers log and skip.
pub fn remove_legacy_autostart_entries(home: &Path) -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        legacy_autostart_paths(home)
            .into_iter()
            .filter(|p| p.exists())
            .filter(|p| remove_legacy_launch_agent(p))
            .collect::<Vec<_>>()
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Untestable tiers from this macOS-first machine: log what would
        // be cleaned and skip (plan: best-effort on non-macOS).
        #[cfg(target_os = "windows")]
        log::info!(
            "[legacy-migration] legacy autostart Run value '{LEGACY_WINDOWS_RUN_VALUE}' may \
             remain; remove it via the registry if the old install is gone"
        );
        #[cfg(target_os = "linux")]
        for path in legacy_autostart_paths(home) {
            if path.exists() {
                log::info!(
                    "[legacy-migration] legacy autostart entry {} left in place (best-effort \
                     tier); remove it manually if the old install is gone",
                    path.display()
                );
            }
        }
        let _ = home;
        Vec::new()
    }
}

/// Entry point wired as the FIRST statement of the `.setup()` closure in
/// `lib.rs`. Must run before `specta_builder.mount_events`, before the
/// headless branch (whose `ModelManager::new` `create_dir_all`s `models/`)
/// and before the `get_settings(app.handle())` read, which WRITES defaults
/// into a fresh store — either would defeat the no-store trigger or the
/// per-item skip-if-present semantics.
pub fn run_first_run_migration(app: &AppHandle) {
    if portable::is_portable() {
        log::debug!(
            "[legacy-migration] portable install: Data/ lives next to the exe, nothing to migrate"
        );
        return;
    }

    let Ok(home) = app.path().home_dir() else {
        log::warn!("[legacy-migration] home dir unavailable, skipping");
        return;
    };
    let Ok(new_data) = portable::app_data_dir(app) else {
        log::warn!("[legacy-migration] app data dir unavailable, skipping");
        return;
    };
    let Ok(new_logs) = portable::app_log_dir(app) else {
        log::warn!("[legacy-migration] app log dir unavailable, skipping");
        return;
    };

    // Legacy autostart cleanup runs whenever any legacy artifact exists,
    // independent of the data trigger — it is idempotent and cheap, and a
    // data migration that partially failed on an earlier launch should
    // still not leave the old launch agent behind.
    let legacy_exists = resolve_legacy_data_dir(&new_data, &home)
        .map(|dir| dir.exists())
        .unwrap_or(false);
    let legacy_autostart_exists = legacy_autostart_paths(&home).iter().any(|p| p.exists());
    if legacy_exists || legacy_autostart_exists {
        for removed in remove_legacy_autostart_entries(&home) {
            log::info!(
                "[legacy-migration] removed legacy autostart launch agent {}",
                removed.display()
            );
        }
        #[cfg(target_os = "macos")]
        log::info!(
            "[legacy-migration] note: a Handy.app registered via SMAppService keeps its own \
             login-item registration until the old app is uninstalled or the entry is toggled \
             off in System Settings → General → Login Items; this app cannot unregister another \
             bundle's registration"
        );
    }

    // Data migration: only on a fresh new dir (no settings store yet) with
    // a legacy dir present — or while a previous pass left failed items
    // (marker present), since the store migrates first and would otherwise
    // satisfy the no-store trigger, making the promised retry unreachable.
    if new_data.join(SETTINGS_STORE_PATH).exists()
        && !new_data.join(MIGRATION_PENDING_MARKER).exists()
    {
        return;
    }
    let Some(legacy_data) = resolve_legacy_data_dir(&new_data, &home) else {
        return;
    };
    if !legacy_data.exists() {
        return;
    }

    log::info!(
        "[legacy-migration] first run over a legacy install: migrating {} -> {}",
        legacy_data.display(),
        new_data.display()
    );
    let report = migrate_data(&legacy_data, &legacy_log_dir(&home), &new_data, &new_logs);

    for item in &report.migrated {
        log::info!("[legacy-migration] migrated {}", item.label());
    }
    for item in &report.skipped {
        log::debug!(
            "[legacy-migration] skipped {} (already present or absent at source)",
            item.label()
        );
    }
    if !report.failed.is_empty() {
        for (item, err) in &report.failed {
            log::warn!(
                "[legacy-migration] failed to migrate {}: {err}",
                item.label()
            );
        }
        log::warn!(
            "[legacy-migration] partial failure: the legacy dir was left untouched and the \
             failed items will be retried on the next launch"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_layout(root: &Path) -> (PathBuf, PathBuf) {
        let legacy_data = root.join("legacy-data");
        let legacy_logs = root.join("legacy-logs");
        std::fs::create_dir_all(legacy_data.join("models")).unwrap();
        std::fs::create_dir_all(legacy_data.join("recordings")).unwrap();
        std::fs::create_dir_all(&legacy_logs).unwrap();
        std::fs::write(legacy_data.join(SETTINGS_STORE_PATH), b"{\"settings\":{}}").unwrap();
        std::fs::write(legacy_data.join(HISTORY_DB), b"db").unwrap();
        std::fs::write(legacy_data.join("models").join("model.bin"), b"model").unwrap();
        std::fs::write(legacy_data.join("recordings").join("r.wav"), b"wav").unwrap();
        std::fs::write(legacy_logs.join("handy.log"), b"log").unwrap();
        (legacy_data, legacy_logs)
    }

    #[test]
    fn item_selection_returns_exact_set() {
        assert_eq!(
            migration_items(),
            [
                MigrationItem::SettingsStore,
                MigrationItem::Models,
                MigrationItem::HistoryDb,
                MigrationItem::Recordings,
                MigrationItem::Logs,
            ]
        );
        // And each item resolves to the live relative names.
        let data = Path::new("/new-data");
        let logs = Path::new("/new-logs");
        assert_eq!(
            item_target(MigrationItem::SettingsStore, data, logs),
            data.join(SETTINGS_STORE_PATH)
        );
        assert_eq!(
            item_target(MigrationItem::Models, data, logs),
            data.join("models")
        );
        assert_eq!(
            item_target(MigrationItem::HistoryDb, data, logs),
            data.join("history.db")
        );
        assert_eq!(
            item_target(MigrationItem::Recordings, data, logs),
            data.join("recordings")
        );
        assert_eq!(item_target(MigrationItem::Logs, data, logs), logs);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn resolve_legacy_data_dir_returns_legacy_dir_or_none() {
        let home = Path::new("/Users/someone");
        let legacy = home
            .join("Library")
            .join("Application Support")
            .join(LEGACY_IDENTIFIER);
        assert_eq!(
            resolve_legacy_data_dir(Path::new("/tmp/elsewhere"), home),
            Some(legacy.clone())
        );
        // The new dir already being the legacy dir means nothing to migrate.
        assert_eq!(resolve_legacy_data_dir(&legacy, home), None);
    }

    #[test]
    fn migrates_every_item_on_fresh_install() {
        let tmp = tempfile::tempdir().unwrap();
        let (legacy_data, legacy_logs) = legacy_layout(tmp.path());
        let new_data = tmp.path().join("new-data");
        let new_logs = tmp.path().join("new-logs");

        let report = migrate_data(&legacy_data, &legacy_logs, &new_data, &new_logs);

        assert!(report.failed.is_empty(), "failed: {:?}", report.failed);
        assert_eq!(report.migrated, migration_items().to_vec());
        assert_eq!(
            std::fs::read(new_data.join(SETTINGS_STORE_PATH)).unwrap(),
            b"{\"settings\":{}}"
        );
        assert_eq!(
            std::fs::read(new_data.join("models").join("model.bin")).unwrap(),
            b"model"
        );
        assert_eq!(std::fs::read(new_data.join("history.db")).unwrap(), b"db");
        assert_eq!(
            std::fs::read(new_data.join("recordings").join("r.wav")).unwrap(),
            b"wav"
        );
        assert_eq!(std::fs::read(new_logs.join("handy.log")).unwrap(), b"log");
    }

    #[test]
    fn skips_items_already_present_in_new_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let (legacy_data, legacy_logs) = legacy_layout(tmp.path());
        let new_data = tmp.path().join("new-data");
        let new_logs = tmp.path().join("new-logs");
        // Pre-existing settings store and models dir with distinct content.
        std::fs::create_dir_all(new_data.join("models")).unwrap();
        std::fs::write(new_data.join(SETTINGS_STORE_PATH), b"existing").unwrap();
        std::fs::write(new_data.join("models").join("existing.bin"), b"keep").unwrap();

        let report = migrate_data(&legacy_data, &legacy_logs, &new_data, &new_logs);

        assert!(report.skipped.contains(&MigrationItem::SettingsStore));
        assert!(report.skipped.contains(&MigrationItem::Models));
        assert!(!report.migrated.contains(&MigrationItem::SettingsStore));
        assert!(!report.migrated.contains(&MigrationItem::Models));
        // Skip-if-present leaves the pre-existing content untouched.
        assert_eq!(
            std::fs::read(new_data.join(SETTINGS_STORE_PATH)).unwrap(),
            b"existing"
        );
        assert!(new_data.join("models").join("existing.bin").exists());
        assert!(!new_data.join("models").join("model.bin").exists());
        // The remaining items still migrate.
        assert!(report.migrated.contains(&MigrationItem::HistoryDb));
        assert!(report.migrated.contains(&MigrationItem::Recordings));
        assert!(report.migrated.contains(&MigrationItem::Logs));
    }

    #[test]
    fn second_run_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let (legacy_data, legacy_logs) = legacy_layout(tmp.path());
        let new_data = tmp.path().join("new-data");
        let new_logs = tmp.path().join("new-logs");

        let first = migrate_data(&legacy_data, &legacy_logs, &new_data, &new_logs);
        assert!(first.failed.is_empty());
        assert_eq!(first.migrated.len(), migration_items().len());

        let second = migrate_data(&legacy_data, &legacy_logs, &new_data, &new_logs);
        assert!(second.is_noop(), "second run migrated/failed: {second:?}");
        assert_eq!(second.skipped.len(), migration_items().len());
        // A clean pass leaves no retry marker.
        assert!(!new_data.join(MIGRATION_PENDING_MARKER).exists());
        // Migrated content is unchanged by the second pass.
        assert_eq!(
            std::fs::read(new_data.join(SETTINGS_STORE_PATH)).unwrap(),
            b"{\"settings\":{}}"
        );
    }

    #[test]
    fn partial_failure_leaves_source_dir_intact() {
        let tmp = tempfile::tempdir().unwrap();
        let (legacy_data, legacy_logs) = legacy_layout(tmp.path());
        let new_data = tmp.path().join("new-data");
        // A regular file where the new logs dir's parent would be: creating
        // the logs target must fail while the data items succeed.
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, b"not a dir").unwrap();
        let new_logs = blocker.join("logs");

        let report = migrate_data(&legacy_data, &legacy_logs, &new_data, &new_logs);

        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, MigrationItem::Logs);
        assert!(report.migrated.contains(&MigrationItem::SettingsStore));
        // The retry marker is written so the next launch re-runs the pass
        // even though the (first-migrated) settings store now exists.
        assert!(new_data.join(MIGRATION_PENDING_MARKER).is_file());
        // The source side is never deleted: the legacy dir and the failed
        // item's source are still there for the next-launch retry.
        assert!(legacy_data.exists());
        assert!(legacy_logs.exists());
        assert!(legacy_logs.join("handy.log").exists());
        assert!(
            legacy_data.join(SETTINGS_STORE_PATH).is_file()
                || new_data.join(SETTINGS_STORE_PATH).is_file()
        );
    }

    #[test]
    fn failed_items_are_retried_and_marker_cleared_on_the_next_launch() {
        let tmp = tempfile::tempdir().unwrap();
        let (legacy_data, legacy_logs) = legacy_layout(tmp.path());
        let new_data = tmp.path().join("new-data");
        // Block the logs target on the first pass, unblock it for the
        // second — exactly the transient condition (locked dir, full disk)
        // the marker exists to survive.
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, b"not a dir").unwrap();
        let blocked_logs = blocker.join("logs");

        let first = migrate_data(&legacy_data, &legacy_logs, &new_data, &blocked_logs);
        assert_eq!(
            first.failed,
            vec![(MigrationItem::Logs, first.failed[0].1.clone())]
        );
        assert!(new_data.join(SETTINGS_STORE_PATH).is_file());
        assert!(new_data.join(MIGRATION_PENDING_MARKER).is_file());

        // The retry sees the settings store at the target — skip-if-present —
        // but the failed item still moves and the marker clears.
        std::fs::remove_file(&blocker).unwrap();
        let new_logs = tmp.path().join("new-logs");
        let second = migrate_data(&legacy_data, &legacy_logs, &new_data, &new_logs);
        assert!(second.failed.is_empty());
        assert_eq!(second.migrated, vec![MigrationItem::Logs]);
        assert!(!new_data.join(MIGRATION_PENDING_MARKER).exists());
        assert!(new_logs.join("handy.log").is_file());

        // Third pass: clean, no marker, full no-op.
        let third = migrate_data(&legacy_data, &legacy_logs, &new_data, &new_logs);
        assert!(third.is_noop());
        assert!(!new_data.join(MIGRATION_PENDING_MARKER).exists());
    }

    #[test]
    fn copy_fallback_copies_files_and_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("nested")).unwrap();
        std::fs::write(src.join("a.bin"), b"a").unwrap();
        std::fs::write(src.join("nested").join("b.bin"), b"b").unwrap();

        let dst = tmp.path().join("dst");
        copy_item(&src, &dst).unwrap();

        assert_eq!(std::fs::read(dst.join("a.bin")).unwrap(), b"a");
        assert_eq!(
            std::fs::read(dst.join("nested").join("b.bin")).unwrap(),
            b"b"
        );
        // Copy leaves the source in place.
        assert!(src.join("a.bin").exists());
    }

    #[test]
    fn legacy_plist_removed_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let plist = legacy_launch_agent_path(home, LEGACY_PRODUCT_NAME);
        assert_eq!(
            plist,
            home.join("Library")
                .join("LaunchAgents")
                .join("Handy.plist")
        );
        std::fs::create_dir_all(plist.parent().unwrap()).unwrap();
        std::fs::write(&plist, "<plist/>").unwrap();

        assert!(remove_legacy_launch_agent(&plist));
        assert!(!plist.exists());
    }

    #[test]
    fn legacy_plist_absent_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let plist = legacy_launch_agent_path(tmp.path(), LEGACY_PRODUCT_NAME);
        assert!(!remove_legacy_launch_agent(&plist));
    }
}
