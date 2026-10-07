//! System tray icon and menu.
//!
//! The tray is driven by a single *desired state* snapshot ([`TrayDesired`])
//! that callers update through [`set_tray_state`], [`refresh_tray_icon`] and
//! [`update_tray_menu`]. Every such call just records intent and schedules a
//! single applier on the main thread, which diffs the desired snapshot against
//! what is currently displayed and touches the native tray only for the parts
//! that actually changed. Requests that arrive while an apply is pending are
//! coalesced into it, so bursts of state changes never queue up native work.
//!
//! Why: native tray updates are the lever we control for the macOS tray
//! disappearance bug (tauri-apps/tauri#12060, Handy #1948). Before this, every
//! recording cycle rebuilt the full menu 3-6 times from several threads, and
//! concurrent rebuilds could interleave and leave a stale menu behind.
//!
//! Exception: [`set_tray_visibility`] and [`recreate_tray_icon`] call the tray
//! directly. Visibility is a separate attribute that never participates in the
//! icon/menu diff, both are rare and user-initiated, and Tauri marshals them
//! onto the main thread so they serialize with the applier anyway. Re-showing
//! a hidden tray relies on tray-icon recreating it from the last applied
//! icon/menu/tooltip, so those must only ever be set through the applier.

use crate::managers::history::{HistoryEntry, HistoryManager};
use crate::managers::model::ModelManager;
use crate::managers::transcription::TranscriptionManager;
use crate::settings::{self, ModelUnloadTimeout};
use crate::tray_i18n::get_tray_translations;
use log::{debug, error, info, trace, warn};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::TrayIcon;
use tauri::{AppHandle, Manager, Theme};
use tauri_plugin_clipboard_manager::ClipboardExt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayIconState {
    Idle,
    Recording,
    Transcribing,
}

impl TrayIconState {
    /// Recording and Transcribing share the same menu ("Cancel" instead of the
    /// model submenu), so only the idle/busy distinction matters for the menu.
    fn is_busy(self) -> bool {
        self != TrayIconState::Idle
    }
}

/// Everything the tray *menu* (and tooltip) depends on. When two snapshots
/// compare equal the menu is not rebuilt.
#[derive(Clone, Debug, PartialEq, Eq)]
struct MenuInputs {
    busy: bool,
    warning: bool,
    model_loaded: bool,
    selected_model: String,
    /// `(id, name)` of downloaded models, sorted by name.
    downloaded_models: Vec<(String, String)>,
    locale: String,
    update_checks_enabled: bool,
    /// `(id, display name)` of the RESIDENT model, when one is loaded. The
    /// submenu label prefers this over `selected_model` — the two diverge in
    /// the failed-switch and pending-unload states (spec F4).
    resident_model: Option<(String, String)>,
    /// Pre-formatted resident-footprint segment for the submenu label and
    /// tooltip (`697 MB` measured / `~697 MB` estimate); `None` omits the
    /// segment. A change drives a menu rebuild via this struct's `PartialEq`.
    model_ram: Option<String>,
    /// Persisted idle-unload timeout, driving the "Unload After" submenu's
    /// checkmark (and its Custom… seconds hint).
    unload_timeout: ModelUnloadTimeout,
    /// macOS menu-bar title (resident model + compact RAM), set via
    /// `TrayIcon::set_title`. `None` (nothing resident) clears it. Only
    /// computed on macOS — Windows does not support tray titles and showing
    /// one on the Linux panel is a behavior change nobody asked for.
    title: Option<String>,
}

/// Complete description of what the tray should look like.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TrayDesired {
    icon_path: &'static str,
    menu: MenuInputs,
}

struct TrayInner {
    /// Intent set by [`set_tray_state`].
    icon_state: TrayIconState,
    /// Latest computed snapshot, waiting to be (or just) applied.
    desired: Option<TrayDesired>,
    /// Icon the native tray currently shows. Only updated when `set_icon`
    /// succeeds, so a failed update is retried on the next sync.
    applied_icon: Option<&'static str>,
    /// Inputs the native menu was last successfully built from. The tooltip
    /// is derived from the same inputs and set best-effort alongside the menu;
    /// it is not tracked separately.
    applied_menu: Option<MenuInputs>,
    /// Menu-bar title the native tray currently shows (macOS). Recorded only
    /// when `set_title` succeeded, so a failed update is retried on the next
    /// sync.
    applied_title: Option<String>,
    /// An apply is scheduled on the main thread.
    pending: bool,
    /// Decoded icons by resource path so the main thread never touches disk.
    icons: HashMap<&'static str, Image<'static>>,
    /// Handed out to each sync request in trigger order, so a slow request
    /// can't overwrite the snapshot of one that was triggered after it.
    next_seq: u64,
    /// Sequence number of the request that produced `desired`.
    desired_seq: u64,
}

/// Tauri managed state owning the tray's desired/applied snapshots.
pub struct TrayState(Mutex<TrayInner>);

impl TrayState {
    pub fn new() -> Self {
        Self(Mutex::new(TrayInner {
            icon_state: TrayIconState::Idle,
            desired: None,
            applied_icon: None,
            applied_menu: None,
            applied_title: None,
            pending: false,
            icons: HashMap::new(),
            next_seq: 0,
            desired_seq: 0,
        }))
    }

    fn lock(&self) -> MutexGuard<'_, TrayInner> {
        self.0.lock().unwrap_or_else(|poisoned| {
            warn!("Tray state mutex was poisoned, recovering");
            poisoned.into_inner()
        })
    }
}

impl Default for TrayState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum AppTheme {
    Dark,
    Light,
    Colored, // Pink/colored theme for Linux
}

/// Gets the current app theme, with Linux defaulting to Colored theme
pub fn get_current_theme(app: &AppHandle) -> AppTheme {
    if cfg!(target_os = "linux") {
        // On Linux, always use the colored theme
        AppTheme::Colored
    } else {
        // On Windows the tray icon sits on the taskbar, which follows the
        // *system* theme (SystemUsesLightTheme), not the app theme. With the
        // "Custom" personalization mode the two can differ (e.g. dark taskbar
        // + light apps), and the window theme would pick an icon that is
        // invisible against the taskbar.
        #[cfg(target_os = "windows")]
        if let Some(theme) = windows_taskbar_theme() {
            return theme;
        }

        // On other platforms, map system theme to our app theme
        if let Some(main_window) = app.get_webview_window("main") {
            match main_window.theme().unwrap_or(Theme::Dark) {
                Theme::Light => AppTheme::Light,
                Theme::Dark => AppTheme::Dark,
                _ => AppTheme::Dark, // Default fallback
            }
        } else {
            AppTheme::Dark
        }
    }
}

/// Reads the Windows taskbar theme from the registry.
///
/// Returns None if the value is missing (older Windows 10 builds default to a
/// dark taskbar there, but falling back to the window theme is safer than
/// guessing).
#[cfg(target_os = "windows")]
fn windows_taskbar_theme() -> Option<AppTheme> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let personalize = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize")
        .ok()?;
    let system_uses_light: u32 = personalize.get_value("SystemUsesLightTheme").ok()?;
    Some(if system_uses_light == 1 {
        AppTheme::Light
    } else {
        AppTheme::Dark
    })
}

/// Gets the appropriate icon path for the given theme and state.
///
/// `warning` overlays a badge on the idle icon while keyboard shortcuts are
/// blocked (macOS Secure Input); recording/transcribing states keep their
/// normal icons so in-flight activity stays recognizable.
pub fn get_icon_path(theme: AppTheme, state: TrayIconState, warning: bool) -> &'static str {
    if warning && state == TrayIconState::Idle {
        return match theme {
            AppTheme::Dark => "resources/tray_idle_warning.png",
            AppTheme::Light => "resources/tray_idle_warning_dark.png",
            // Linux never sets the warning flag (Secure Input is macOS-only),
            // but fall back to the normal icon just in case.
            AppTheme::Colored => "resources/handy.png",
        };
    }
    match (theme, state) {
        // Dark theme uses light icons
        (AppTheme::Dark, TrayIconState::Idle) => "resources/tray_idle.png",
        (AppTheme::Dark, TrayIconState::Recording) => "resources/tray_recording.png",
        (AppTheme::Dark, TrayIconState::Transcribing) => "resources/tray_transcribing.png",
        // Light theme uses dark icons
        (AppTheme::Light, TrayIconState::Idle) => "resources/tray_idle_dark.png",
        (AppTheme::Light, TrayIconState::Recording) => "resources/tray_recording_dark.png",
        (AppTheme::Light, TrayIconState::Transcribing) => "resources/tray_transcribing_dark.png",
        // Colored theme uses pink icons (for Linux)
        (AppTheme::Colored, TrayIconState::Idle) => "resources/handy.png",
        (AppTheme::Colored, TrayIconState::Recording) => "resources/recording.png",
        (AppTheme::Colored, TrayIconState::Transcribing) => "resources/transcribing.png",
    }
}

/// Sets the recording state shown by the tray (icon + Cancel/model menu).
pub fn set_tray_state(app: &AppHandle, state: TrayIconState) {
    sync_tray_with(app, |inner| inner.icon_state = state);
}

/// Re-syncs the tray after something other than the recording state changed
/// (theme, Secure Input warning). The recording state itself is preserved.
pub fn refresh_tray_icon(app: &AppHandle) {
    sync_tray(app);
}

/// Re-syncs the tray after something the menu depends on changed (model
/// list/selection/loaded state, language, settings).
pub fn update_tray_menu(app: &AppHandle) {
    sync_tray(app);
}

/// Records the current desired tray state and schedules one apply on the main
/// thread (or lets an already-pending apply pick it up). Never blocks on the
/// main thread.
///
/// The snapshot (settings, model list, loaded state) is computed on the
/// *calling* thread on purpose: the main-thread applier must not take manager
/// locks that a worker may hold across slow work (see #1716).
pub fn sync_tray(app: &AppHandle) {
    sync_tray_with(app, |_| {});
}

fn sync_tray_with(app: &AppHandle, update: impl FnOnce(&mut TrayInner)) {
    let Some(state) = app.try_state::<TrayState>() else {
        return;
    };

    // Record intent and claim a sequence number in one critical section, so
    // sequence order == the order in which state changes were requested.
    let (seq, icon_state) = {
        let mut inner = state.lock();
        update(&mut inner);
        inner.next_seq += 1;
        (inner.next_seq, inner.icon_state)
    };

    // Tray not built yet (early secure-input monitor callbacks). The intent
    // is kept and picked up by the first sync after the tray exists.
    if app.try_state::<TrayIcon>().is_none() {
        return;
    }

    let desired = compute_desired(app, icon_state);

    // Decode the icon off the main thread, once per path, outside the lock.
    let needs_icon = !state.lock().icons.contains_key(desired.icon_path);
    let loaded_icon = if needs_icon {
        match load_tray_icon(
            app.path()
                .resolve(desired.icon_path, tauri::path::BaseDirectory::Resource),
        ) {
            Ok(image) => Some(image),
            Err(err) => {
                error!("Failed to load tray icon '{}': {err}", desired.icon_path);
                None
            }
        }
    } else {
        None
    };

    let schedule = {
        let mut inner = state.lock();
        if let Some(image) = loaded_icon {
            inner.icons.insert(desired.icon_path, image);
        }
        if seq < inner.desired_seq {
            // A request triggered after this one already stored its snapshot
            // (and scheduled an apply). Ours is stale; drop it.
            trace!(
                "tray sync: request {seq} superseded by {}",
                inner.desired_seq
            );
            return;
        }
        inner.desired = Some(desired);
        inner.desired_seq = seq;
        // If an apply is already pending it will read the snapshot we just
        // stored; otherwise schedule one.
        !std::mem::replace(&mut inner.pending, true)
    };

    if schedule {
        post_apply(app);
    } else {
        trace!("tray sync: apply already pending");
    }
}

fn compute_desired(app: &AppHandle, icon_state: TrayIconState) -> TrayDesired {
    let settings = settings::get_settings(app);
    let theme = get_current_theme(app);
    let warning = crate::secure_input::tray_warning_active(app);
    let transcription = app.state::<Arc<TranscriptionManager>>();
    let model_loaded = transcription.is_model_loaded();

    let mut downloaded_models: Vec<(String, String)> = app
        .state::<Arc<ModelManager>>()
        .get_available_models()
        .into_iter()
        .filter(|m| m.is_downloaded)
        .map(|m| (m.id, m.name))
        .collect();
    downloaded_models.sort_by(|a, b| a.1.cmp(&b.1));

    // Resident model (id + display name) via the existing public accessor,
    // mapped through the same downloaded list the submenu builds from; the
    // footprint segment comes from the resident-footprint plumbing (measured
    // worker RSS for transcribe-cpp, size estimate for in-process ONNX).
    let resident_model = transcription.get_current_model().and_then(|id| {
        downloaded_models
            .iter()
            .find(|(mid, _)| *mid == id)
            .map(|(_, name)| (id, name.clone()))
    });
    let footprint = transcription.resident_model_footprint();
    let model_ram = format_ram_segment(footprint);
    // Menu-bar title (macOS only; see MenuInputs::title). Computed from the
    // same load/unload-driven snapshot as the menu — no polling (spec C2) —
    // and gated by the menu_bar_model_title setting (desired_tray_title
    // returns None for every input when it is off).
    #[cfg(target_os = "macos")]
    let title = desired_tray_title(
        settings.menu_bar_model_title,
        resident_model.as_ref().map(|(_, name)| name.as_str()),
        footprint.map(|(bytes, _)| bytes),
    );
    #[cfg(not(target_os = "macos"))]
    let title: Option<String> = None;

    TrayDesired {
        icon_path: get_icon_path(theme, icon_state, warning),
        menu: MenuInputs {
            busy: icon_state.is_busy(),
            warning,
            model_loaded,
            selected_model: settings.selected_model,
            downloaded_models,
            locale: settings.app_language,
            update_checks_enabled: settings.update_checks_enabled,
            resident_model,
            model_ram,
            unload_timeout: settings.model_unload_timeout,
            title,
        },
    }
}

fn post_apply(app: &AppHandle) {
    let handle = app.clone();
    if let Err(err) = app.run_on_main_thread(move || apply_on_main(&handle)) {
        // Event loop is gone (shutdown). Clear `pending` so a later call, if
        // any, doesn't wait forever for an apply that will never run.
        error!("Failed to dispatch tray update to the main thread: {err}");
        if let Some(state) = app.try_state::<TrayState>() {
            state.lock().pending = false;
        }
    }
}

/// The single writer to the native tray. Runs on the main thread.
fn apply_on_main(app: &AppHandle) {
    let Some(state) = app.try_state::<TrayState>() else {
        return;
    };
    let Some(tray) = app.try_state::<TrayIcon>() else {
        return;
    };

    let started = Instant::now();
    let (desired, icon, icon_changed, menu_changed, title_changed) = {
        let mut inner = state.lock();
        inner.pending = false;
        let Some(desired) = inner.desired.clone() else {
            return;
        };
        let icon_changed = inner.applied_icon != Some(desired.icon_path);
        let menu_changed = inner.applied_menu.as_ref() != Some(&desired.menu);
        let title_changed = inner.applied_title != desired.menu.title;
        if !icon_changed && !menu_changed && !title_changed {
            trace!("tray apply: nothing changed");
            return;
        }
        let icon = inner.icons.get(desired.icon_path).cloned();
        (desired, icon, icon_changed, menu_changed, title_changed)
    };

    // Each part is recorded as applied only if its native call succeeded, so a
    // transient failure is retried on the next sync instead of being
    // remembered as displayed.
    let mut icon_ok = false;
    if icon_changed {
        match icon {
            Some(image) => match tray.set_icon_with_as_template(Some(image), true) {
                Ok(()) => icon_ok = true,
                Err(err) => error!("Failed to update tray icon '{}': {err}", desired.icon_path),
            },
            None => error!("Tray icon '{}' is not loaded", desired.icon_path),
        }
    }

    let mut menu_ok = false;
    if menu_changed {
        match build_menu(app, &desired.menu) {
            Ok((menu, tooltip)) => match tray.set_menu(Some(menu)) {
                Ok(()) => {
                    menu_ok = true;
                    // Best-effort: logged, not retried. The tooltip is cosmetic
                    // and can only fail on Windows, where a failing
                    // Shell_NotifyIcon call means the icon is failing too.
                    // Gating `menu_ok` on it would re-run the full menu
                    // rebuild on every sync for the cheapest mutation.
                    if let Err(err) = tray.set_tooltip(Some(tooltip)) {
                        error!("Failed to set tray tooltip: {err}");
                    }
                }
                Err(err) => error!("Failed to set tray menu: {err}"),
            },
            Err(err) => error!("Failed to build tray menu: {err}"),
        }
    }

    // Menu-bar title (macOS). Best-effort like the tooltip, but tracked in
    // `applied_title` so a failed `set_title` is retried on the next sync —
    // the title is the loaded-state indicator and worth one retry, and it can
    // change without the menu rebuilding only in exotic partial-failure cases.
    let mut title_ok = false;
    if title_changed && set_tray_title(&tray, desired.menu.title.as_deref()) {
        title_ok = true;
    }

    {
        let mut inner = state.lock();
        if icon_ok {
            inner.applied_icon = Some(desired.icon_path);
        }
        if menu_ok {
            inner.applied_menu = Some(desired.menu.clone());
        }
        if title_ok {
            inner.applied_title = desired.menu.title.clone();
        }
    }

    debug!(
        "tray apply: icon={} menu={} busy={} took={:?}",
        if icon_changed {
            desired.icon_path
        } else {
            "unchanged"
        },
        if menu_changed { "rebuilt" } else { "unchanged" },
        desired.menu.busy,
        started.elapsed()
    );
}

fn load_tray_icon(resolved_icon_path: tauri::Result<PathBuf>) -> tauri::Result<Image<'static>> {
    let resolved_icon_path = resolved_icon_path?;
    Image::from_path(&resolved_icon_path).map(Image::to_owned)
}

pub fn tray_tooltip() -> String {
    version_label()
}

fn version_label() -> String {
    if cfg!(debug_assertions) {
        format!("VoxBar v{} (Dev)", env!("CARGO_PKG_VERSION"))
    } else {
        format!("VoxBar v{}", env!("CARGO_PKG_VERSION"))
    }
}

/// Builds the tray menu and tooltip for the given inputs. Pure with respect
/// to app state: everything it depends on is in `inputs`, plus the
/// process-constant `HANDY_DISABLE_UPDATER` env flag behind
/// `update_checks_forced_disabled()`, which cannot change during a run.
fn build_menu(app: &AppHandle, inputs: &MenuInputs) -> tauri::Result<(Menu<tauri::Wry>, String)> {
    let strings = get_tray_translations(Some(inputs.locale.clone()));

    // Secure Input warning entry (macOS): clicking opens the settings window
    // where the full warning banner explains the situation. Locales that
    // haven't translated the key yet get the English string rather than a
    // blank menu item (build.rs emits "" for missing keys).
    let secure_input_warning = if inputs.warning {
        let label = if strings.secure_input_warning.is_empty() {
            get_tray_translations(Some("en".to_string())).secure_input_warning
        } else {
            strings.secure_input_warning.clone()
        };
        Some(MenuItem::with_id(
            app,
            "secure_input_warning",
            &label,
            true,
            None::<&str>,
        )?)
    } else {
        None
    };

    // Platform-specific accelerators
    #[cfg(target_os = "macos")]
    let (settings_accelerator, quit_accelerator) = (Some("Cmd+,"), Some("Cmd+Q"));
    #[cfg(not(target_os = "macos"))]
    let (settings_accelerator, quit_accelerator) = (Some("Ctrl+,"), Some("Ctrl+Q"));

    // Create common menu items
    let version_label = version_label();
    let version_i = MenuItem::with_id(app, "version", &version_label, false, None::<&str>)?;
    let settings_i = MenuItem::with_id(
        app,
        "settings",
        &strings.settings,
        true,
        settings_accelerator,
    )?;
    let check_updates_i = MenuItem::with_id(
        app,
        "check_updates",
        &strings.check_updates,
        inputs.update_checks_enabled,
        None::<&str>,
    )?;
    let copy_last_transcript_i = MenuItem::with_id(
        app,
        "copy_last_transcript",
        &strings.copy_last_transcript,
        true,
        None::<&str>,
    )?;
    let quit_i = MenuItem::with_id(app, "quit", &strings.quit, true, quit_accelerator)?;
    let separator = || PredefinedMenuItem::separator(app);

    let menu = if inputs.busy {
        let cancel_i = MenuItem::with_id(app, "cancel", &strings.cancel, true, None::<&str>)?;
        Menu::with_items(
            app,
            &[
                &version_i,
                &separator()?,
                &cancel_i,
                &separator()?,
                &copy_last_transcript_i,
                &separator()?,
                &settings_i,
                &check_updates_i,
                &separator()?,
                &quit_i,
            ],
        )?
    } else {
        // Build model submenu — the label shows the RESIDENT model and its
        // footprint when one is loaded, falling back to the selection.
        let model_name = resolve_model_label_name(
            inputs.resident_model.as_ref(),
            &inputs.selected_model,
            &inputs.downloaded_models,
            &strings.model,
        );
        let submenu_label = match &inputs.model_ram {
            Some(ram) => format!("{model_name} — {ram}"),
            None => model_name.clone(),
        };

        let model_submenu = Submenu::with_id(app, "model_submenu", &submenu_label, true)?;
        for (id, name) in &inputs.downloaded_models {
            let is_active = *id == inputs.selected_model;
            let item_id = format!("model_select:{}", id);
            let item = CheckMenuItem::with_id(app, &item_id, name, true, is_active, None::<&str>)?;
            model_submenu.append(&item)?;
        }

        let unload_model_i = MenuItem::with_id(
            app,
            "unload_model",
            &strings.unload_model,
            inputs.model_loaded,
            None::<&str>,
        )?;

        // "Unload After" submenu: preset idle timeouts with a checkmark on
        // the active one, plus a Custom… item that opens Settings focused on
        // the custom-seconds field. Selecting a preset persists immediately
        // through the same setting the app uses.
        let unload_after_submenu =
            Submenu::with_id(app, "unload_after_submenu", &strings.unload_after, true)?;
        for (preset_id, preset_secs) in UNLOAD_AFTER_PRESETS {
            let label = match preset_secs {
                None => strings.unload_after_never.clone(),
                Some(0) => strings.unload_after_immediately.clone(),
                Some(secs) => format_duration_compact(*secs),
            };
            let item_id = format!("unload_after:{preset_id}");
            let is_active = unload_after_preset_is_active(&inputs.unload_timeout, *preset_secs);
            let item =
                CheckMenuItem::with_id(app, &item_id, &label, true, is_active, None::<&str>)?;
            unload_after_submenu.append(&item)?;
        }
        let custom_label =
            unload_after_custom_label(&strings.unload_after_custom, &inputs.unload_timeout);
        let unload_after_custom_i = MenuItem::with_id(
            app,
            "unload_after:custom",
            &custom_label,
            true,
            None::<&str>,
        )?;
        unload_after_submenu.append(&unload_after_custom_i)?;

        Menu::with_items(
            app,
            &[
                &version_i,
                &separator()?,
                &copy_last_transcript_i,
                &separator()?,
                &model_submenu,
                &unload_model_i,
                &unload_after_submenu,
                &separator()?,
                &settings_i,
                &check_updates_i,
                &separator()?,
                &quit_i,
            ],
        )?
    };

    // When update checks are forced off (e.g. HANDY_DISABLE_UPDATER, set by
    // the Nix package), the item is dropped from the menu rather than shown
    // disabled — it can never do anything in that case, and a disabled item
    // still shifts every entry below it by one position. A manually-disabled
    // toggle in Debug Settings keeps the old greyed-out behavior via the
    // enabled flag.
    if settings::update_checks_forced_disabled() {
        menu.remove(&check_updates_i)?;
    }

    // Both layouts start with [version, separator, ...]; slot the warning in
    // right below the version line so it's the first actionable thing seen.
    // The tooltip mirrors the resident model + footprint segment (spec F4).
    let mut tooltip = version_label;
    if let Some((_, resident_name)) = &inputs.resident_model {
        tooltip = match &inputs.model_ram {
            Some(ram) => format!("{tooltip} — {resident_name} — {ram}"),
            None => format!("{tooltip} — {resident_name}"),
        };
    }
    if let Some(warning_item) = secure_input_warning {
        menu.insert(&warning_item, 2)?;
        menu.insert(&separator()?, 3)?;
        tooltip = format!("{} — {}", tooltip, warning_item.text().unwrap_or_default());
    }

    Ok((menu, tooltip))
}

fn last_transcript_text(entry: &HistoryEntry) -> &str {
    entry
        .post_processed_text
        .as_deref()
        .unwrap_or(&entry.transcription_text)
}

/// Format the resident-footprint segment shown after the model name:
/// measured bytes render plain (`697 MB`), estimates get a `~` prefix
/// (`~697 MB`), and `None` (nothing resident, or no estimate resolved)
/// omits the segment entirely. Pure — menu building never fails on
/// measurement errors.
fn format_ram_segment(footprint: Option<(u64, bool)>) -> Option<String> {
    let (bytes, measured) = footprint?;
    let mb = bytes.saturating_add(512 * 1024) / (1024 * 1024);
    if measured {
        Some(format!("{mb} MB"))
    } else {
        Some(format!("~{mb} MB"))
    }
}

/// Resolve the model submenu label's name part (pure — extracted from
/// `build_menu` so the precedence is unit-testable without an app):
/// prefer the RESIDENT model (what is actually in memory — the
/// selected-vs-resident divergence of the failed-switch and pending-unload
/// states), fall back to the `selected_model` lookup, then the localized
/// "Model" fallback string.
fn resolve_model_label_name(
    resident_model: Option<&(String, String)>,
    selected_model: &str,
    downloaded_models: &[(String, String)],
    fallback: &str,
) -> String {
    if let Some((_, name)) = resident_model {
        return name.clone();
    }
    downloaded_models
        .iter()
        .find(|(id, _)| id == selected_model)
        .map(|(_, name)| name.clone())
        .unwrap_or_else(|| fallback.to_string())
}

// --- Menu-bar title (macOS loaded-state indicator) -------------------------

/// Keep the menu-bar title from eating the menu bar: everything past this
/// many characters is truncation territory. 18 fits the spec's worked
/// example (`Parakeet EN · 768M`) exactly — the "~16 characters" guidance
/// budgets this class of length.
const TRAY_TITLE_MAX_CHARS: usize = 18;

/// The title `compute_desired` wants, given the `menu_bar_model_title`
/// setting: off → `None` for EVERY input (even with a model resident — the
/// applier's diff then clears any currently-displayed title); on → exactly
/// what [`format_tray_title`] produces today. Pure, so the setting's effect
/// is unit-testable without an app.
fn desired_tray_title(
    setting_enabled: bool,
    resident_name: Option<&str>,
    footprint_bytes: Option<u64>,
) -> Option<String> {
    if !setting_enabled {
        return None;
    }
    format_tray_title(resident_name, footprint_bytes)
}

/// Format the macOS menu-bar title: short model name + compact resident RAM
/// (`Parakeet EN · 768M`). Pure — `None` in, `None` out (nothing resident
/// clears the title entirely). The name is truncated (with `…`) when the
/// combined title would exceed [`TRAY_TITLE_MAX_CHARS`] characters; the RAM
/// segment is never truncated. A missing footprint yields a name-only title.
///
/// NOT the function `compute_desired` calls — use [`desired_tray_title`],
/// which gates on the `menu_bar_model_title` setting.
fn format_tray_title(resident_name: Option<&str>, footprint_bytes: Option<u64>) -> Option<String> {
    let name = resident_name?;
    let ram = footprint_bytes.map(compact_ram);
    let title = match &ram {
        Some(ram) => format!("{name} · {ram}"),
        None => name.to_string(),
    };
    let total = title.chars().count();
    if total <= TRAY_TITLE_MAX_CHARS {
        return Some(title);
    }
    // Truncate the NAME so the whole fits; reserve one char for the ellipsis.
    // The separator + RAM segment are the informative tail, keep them intact.
    let tail_len = match &ram {
        Some(ram) => " · ".chars().count() + ram.chars().count(),
        None => 0,
    };
    let name_budget = TRAY_TITLE_MAX_CHARS.saturating_sub(tail_len + 1);
    let mut truncated: String = name.chars().take(name_budget).collect();
    truncated.push('…');
    match &ram {
        Some(ram) => Some(format!("{truncated} · {ram}")),
        None => Some(truncated),
    }
}

/// Compact RAM for the menu-bar title: `768M` below 1 GiB, `1.2G` above.
/// Rounds MiB like [`format_ram_segment`] (nearest, not truncating).
fn compact_ram(bytes: u64) -> String {
    let mb = bytes.saturating_add(512 * 1024) / (1024 * 1024);
    if mb >= 1024 {
        format!("{:.1}G", mb as f64 / 1024.0)
    } else {
        format!("{mb}M")
    }
}

/// Apply the menu-bar title to the native tray (macOS). `false` when the
/// native call failed, so the applier does not record it as displayed.
#[cfg(target_os = "macos")]
fn set_tray_title(tray: &TrayIcon, title: Option<&str>) -> bool {
    match tray.set_title(title) {
        Ok(()) => true,
        Err(err) => {
            error!("Failed to set tray title: {err}");
            false
        }
    }
}

/// Windows has no tray title and the Linux panel is not a target for this
/// feature; the applier's diff never sees a title change there anyway
/// (`compute_desired` leaves `MenuInputs::title` as `None`).
#[cfg(not(target_os = "macos"))]
fn set_tray_title(_tray: &TrayIcon, _title: Option<&str>) -> bool {
    true
}

// --- "Unload After" tray submenu --------------------------------------------

/// Tray "Unload After" presets: `(menu-id suffix, idle seconds; None = Never)`.
/// The numeric items' ids ARE their seconds, so the menu-event handler parses
/// them straight into `ModelUnloadTimeout::from_preset_seconds`; "never" and
/// the trailing Custom… item are handled specially. Order is display order.
const UNLOAD_AFTER_PRESETS: &[(&str, Option<u64>)] = &[
    ("0", Some(0)),
    ("15", Some(15)),
    ("30", Some(30)),
    ("60", Some(60)),
    ("120", Some(120)),
    ("300", Some(300)),
    ("600", Some(600)),
    ("900", Some(900)),
    ("3600", Some(3600)),
    ("never", None),
];

/// Locale-neutral compact duration for preset labels: `15s`, `1m`, `1h`.
/// Pure.
fn format_duration_compact(seconds: u64) -> String {
    if seconds >= 3600 && seconds % 3600 == 0 {
        format!("{}h", seconds / 3600)
    } else if seconds >= 60 && seconds % 60 == 0 {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

/// Whether a preset's checkmark should be on for the current setting. Presets
/// match semantically (by idle seconds), so `Custom { seconds: 300 }` lights
/// up the 5m preset just like `Min5` does. Pure.
fn unload_after_preset_is_active(current: &ModelUnloadTimeout, preset_secs: Option<u64>) -> bool {
    match preset_secs {
        None => matches!(current, ModelUnloadTimeout::Never),
        Some(secs) => current.to_seconds() == Some(secs),
    }
}

/// Label for the trailing Custom… item: shows the current custom seconds when
/// the setting is a custom value (`Custom… (90s)`), plain otherwise. Pure.
fn unload_after_custom_label(base: &str, current: &ModelUnloadTimeout) -> String {
    match current {
        ModelUnloadTimeout::Custom { seconds } => format!("{base} ({seconds}s)"),
        _ => base.to_string(),
    }
}

pub fn set_tray_visibility(app: &AppHandle, visible: bool) {
    let tray = app.state::<TrayIcon>();
    if let Err(e) = tray.set_visible(visible) {
        error!("Failed to set tray visibility: {}", e);
    } else {
        info!("Tray visibility set to: {}", visible);
    }
}

/// Recovery for the macOS tray-disappearance bug (#1948, tauri-apps/tauri#12060):
/// the `NSStatusItem` can silently vanish with no error surfaced to the app.
/// Hiding and re-showing the tray recreates it with its current icon, menu and
/// tooltip. Called when the user "relaunches" Handy while it is already running
/// (`RunEvent::Reopen` for Spotlight/Finder/Dock, the single-instance callback
/// for a second process) — the natural "where did my icon go?" moment — so a
/// relaunch brings the icon back without a full quit.
#[cfg(target_os = "macos")]
pub fn recreate_tray_icon(app: &AppHandle) {
    let no_tray = app
        .try_state::<crate::cli::CliArgs>()
        .map(|args| args.no_tray)
        .unwrap_or(false);
    if no_tray || !settings::get_settings(app).show_tray_icon {
        return;
    }
    let Some(tray) = app.try_state::<TrayIcon>() else {
        return;
    };
    info!("Recreating tray icon on relaunch");
    if let Err(e) = tray.set_visible(false).and_then(|_| tray.set_visible(true)) {
        error!("Failed to recreate tray icon: {}", e);
    }
}

pub fn copy_last_transcript(app: &AppHandle) {
    let history_manager = app.state::<Arc<HistoryManager>>();
    let entry = match history_manager.get_latest_completed_entry() {
        Ok(Some(entry)) => entry,
        Ok(None) => {
            warn!("No completed transcription history entries available for tray copy.");
            return;
        }
        Err(err) => {
            error!(
                "Failed to fetch last completed transcription entry: {}",
                err
            );
            return;
        }
    };

    let text = last_transcript_text(&entry);
    if text.trim().is_empty() {
        warn!("Last completed transcription is empty; skipping tray copy.");
        return;
    }

    if let Err(err) = app.clipboard().write_text(text) {
        error!("Failed to copy last transcript to clipboard: {}", err);
        return;
    }

    info!("Copied last transcript to clipboard via tray.");
}

#[cfg(test)]
mod tests {
    use super::{
        compact_ram, desired_tray_title, format_duration_compact, format_ram_segment,
        format_tray_title, last_transcript_text, load_tray_icon, resolve_model_label_name,
        unload_after_custom_label, unload_after_preset_is_active, MenuInputs, TrayDesired,
        TrayIconState, TRAY_TITLE_MAX_CHARS,
    };
    use crate::managers::history::HistoryEntry;
    use crate::settings::ModelUnloadTimeout;

    fn build_entry(transcription: &str, post_processed: Option<&str>) -> HistoryEntry {
        HistoryEntry {
            id: 1,
            file_name: "handy-1.wav".to_string(),
            timestamp: 0,
            saved: false,
            title: "Recording".to_string(),
            transcription_text: transcription.to_string(),
            post_processed_text: post_processed.map(|text| text.to_string()),
            post_process_prompt: None,
            post_process_requested: false,
            model_id: None,
        }
    }

    fn inputs(busy: bool) -> MenuInputs {
        MenuInputs {
            busy,
            warning: false,
            model_loaded: true,
            selected_model: "small".to_string(),
            downloaded_models: vec![("small".to_string(), "Small".to_string())],
            locale: "en".to_string(),
            update_checks_enabled: true,
            resident_model: None,
            model_ram: None,
            unload_timeout: ModelUnloadTimeout::Min2,
            title: None,
        }
    }

    #[test]
    fn ram_segment_formats_measured_estimate_and_absent() {
        let mb = 697 * 1024 * 1024;
        assert_eq!(
            format_ram_segment(Some((mb, true))),
            Some("697 MB".to_string())
        );
        assert_eq!(
            format_ram_segment(Some((mb, false))),
            Some("~697 MB".to_string())
        );
        // Nothing resident / no estimate -> the segment is omitted.
        assert_eq!(format_ram_segment(None), None);
        // Rounds to the nearest MB rather than truncating: 400 KiB stays at
        // 697, 600 KiB rounds up to 698.
        assert_eq!(
            format_ram_segment(Some((mb + 400 * 1024, true))),
            Some("697 MB".to_string())
        );
        assert_eq!(
            format_ram_segment(Some((mb + 600 * 1024, true))),
            Some("698 MB".to_string())
        );
    }

    #[test]
    fn menu_inputs_differ_on_model_ram_change() {
        let mut with_ram = inputs(false);
        with_ram.model_ram = Some("697 MB".to_string());
        assert_ne!(inputs(false), with_ram);
    }

    #[test]
    fn menu_inputs_differ_on_resident_model_change() {
        let mut with_resident = inputs(false);
        with_resident.resident_model = Some(("small".to_string(), "Small".to_string()));
        assert_ne!(inputs(false), with_resident);
    }

    #[test]
    fn menu_inputs_differ_on_unload_timeout_change() {
        // Selecting an "Unload After" preset must rebuild the menu so the
        // checkmark moves.
        let mut other_timeout = inputs(false);
        other_timeout.unload_timeout = ModelUnloadTimeout::Custom { seconds: 90 };
        assert_ne!(inputs(false), other_timeout);
    }

    #[test]
    fn menu_inputs_differ_on_title_change() {
        let mut with_title = inputs(false);
        with_title.title = Some("Small · 300M".to_string());
        assert_ne!(inputs(false), with_title);
    }

    #[test]
    fn tray_title_formats_name_and_compact_ram() {
        let mib = 1024 * 1024;
        // Spec example shape: short name + compact resident RAM.
        assert_eq!(
            format_tray_title(Some("Parakeet EN"), Some(768 * mib)),
            Some("Parakeet EN · 768M".to_string())
        );
        // GiB-scale footprints compact to one decimal.
        assert_eq!(
            format_tray_title(Some("Whisper Lg"), Some((1.3 * 1024.0) as u64 * mib)),
            Some("Whisper Lg · 1.3G".to_string())
        );
        // No footprint estimate -> name-only title.
        assert_eq!(
            format_tray_title(Some("Small"), None),
            Some("Small".to_string())
        );
    }

    #[test]
    fn tray_title_clears_when_no_model_resident() {
        assert_eq!(format_tray_title(None, Some(768 * 1024 * 1024)), None);
        assert_eq!(format_tray_title(None, None), None);
    }

    #[test]
    fn disabled_menu_bar_title_never_produces_one_even_with_a_model_resident() {
        let mib = 1024 * 1024;
        // Setting off: None for every resident/footprint combination a
        // loaded model can produce — compute_desired feeds exactly these
        // inputs, so the applier's diff clears any displayed title.
        assert_eq!(
            desired_tray_title(false, Some("Parakeet EN"), Some(768 * mib)),
            None
        );
        assert_eq!(desired_tray_title(false, Some("Parakeet EN"), None), None);
        assert_eq!(desired_tray_title(false, None, Some(768 * mib)), None);
        assert_eq!(desired_tray_title(false, None, None), None);
    }

    #[test]
    fn enabled_menu_bar_title_matches_format_tray_title_exactly() {
        let mib = 1024 * 1024;
        // Setting on: today's behavior, byte for byte, across the input
        // shapes (resident+RAM, resident-only, nothing resident).
        let cases: Vec<(Option<&str>, Option<u64>)> = vec![
            (Some("Parakeet EN"), Some(768 * mib)),
            (Some("Parakeet Unified EN 0.6B"), Some(768 * mib)),
            (Some("Small"), None),
            (None, Some(768 * mib)),
            (None, None),
        ];
        for (name, footprint) in cases {
            assert_eq!(
                desired_tray_title(true, name, footprint),
                format_tray_title(name, footprint),
                "name={name:?} footprint={footprint:?}"
            );
        }
    }

    #[test]
    fn tray_title_truncates_name_to_fit_but_never_the_ram_segment() {
        let mib = 1024 * 1024;
        // Exactly at the limit: untouched.
        let exact = format_tray_title(Some("Parakeet EN"), Some(768 * mib)).unwrap();
        assert_eq!(exact, "Parakeet EN · 768M");
        assert_eq!(exact.chars().count(), TRAY_TITLE_MAX_CHARS);
        // A long name gets truncated with an ellipsis, RAM segment intact,
        // total within budget.
        let long = format_tray_title(Some("Parakeet Unified EN 0.6B"), Some(768 * mib)).unwrap();
        assert!(long.chars().count() <= TRAY_TITLE_MAX_CHARS, "{long}");
        assert!(long.ends_with("· 768M"), "{long}");
        assert!(long.contains('…'), "{long}");
        // Name-only titles truncate too.
        let name_only = format_tray_title(Some("A Very Long Model Name Indeed"), None).unwrap();
        assert!(
            name_only.chars().count() <= TRAY_TITLE_MAX_CHARS,
            "{name_only}"
        );
        assert!(name_only.ends_with('…'), "{name_only}");
        // Truncation is char-based, not byte-based: a CJK-heavy name never
        // panics or overruns the budget.
        let cjk = format_tray_title(Some("语音识别模型很长很长"), Some(300 * mib)).unwrap();
        assert!(cjk.chars().count() <= TRAY_TITLE_MAX_CHARS, "{cjk}");
    }

    #[test]
    fn compact_ram_boundaries() {
        let kib = 1024;
        let mib = 1024 * 1024;
        // Sub-MiB footprints round to the nearest MB (600 KiB -> 1M).
        assert_eq!(compact_ram(0), "0M");
        assert_eq!(compact_ram(600 * kib), "1M");
        assert_eq!(compact_ram(768 * mib), "768M");
        // 1 GiB boundary flips to the G scale.
        assert_eq!(compact_ram(1024 * mib), "1.0G");
        assert_eq!(compact_ram(1536 * mib), "1.5G");
        // Rounding to one decimal, not truncation: 1.26 GiB -> 1.3G.
        assert_eq!(compact_ram(1290 * mib), "1.3G");
    }

    #[test]
    fn duration_compact_uses_s_m_h() {
        assert_eq!(format_duration_compact(15), "15s");
        assert_eq!(format_duration_compact(45), "45s");
        assert_eq!(format_duration_compact(60), "1m");
        assert_eq!(format_duration_compact(120), "2m");
        assert_eq!(format_duration_compact(900), "15m");
        assert_eq!(format_duration_compact(3600), "1h");
    }

    #[test]
    fn unload_after_checkmark_matches_by_idle_seconds() {
        // Canonical variants light their preset.
        assert!(unload_after_preset_is_active(
            &ModelUnloadTimeout::Min5,
            Some(300)
        ));
        assert!(unload_after_preset_is_active(
            &ModelUnloadTimeout::Immediately,
            Some(0)
        ));
        assert!(unload_after_preset_is_active(
            &ModelUnloadTimeout::Never,
            None
        ));
        // A custom value equal to a preset lights that preset; a custom value
        // matching no preset lights none of them.
        assert!(unload_after_preset_is_active(
            &ModelUnloadTimeout::Custom { seconds: 300 },
            Some(300)
        ));
        assert!(!unload_after_preset_is_active(
            &ModelUnloadTimeout::Custom { seconds: 90 },
            Some(300)
        ));
        // The debug-only Sec15 and Custom{15} are the same timeout: both mark
        // the 15s preset, and neither marks Never.
        assert!(unload_after_preset_is_active(
            &ModelUnloadTimeout::Sec15,
            Some(15)
        ));
        assert!(unload_after_preset_is_active(
            &ModelUnloadTimeout::Custom { seconds: 15 },
            Some(15)
        ));
        assert!(!unload_after_preset_is_active(
            &ModelUnloadTimeout::Custom { seconds: 15 },
            None
        ));
        // Never is not "0 seconds".
        assert!(!unload_after_preset_is_active(
            &ModelUnloadTimeout::Never,
            Some(0)
        ));
    }

    #[test]
    fn unload_after_custom_label_shows_seconds_only_for_custom() {
        assert_eq!(
            unload_after_custom_label("Custom…", &ModelUnloadTimeout::Custom { seconds: 90 }),
            "Custom… (90s)"
        );
        assert_eq!(
            unload_after_custom_label("Custom…", &ModelUnloadTimeout::Min2),
            "Custom…"
        );
        assert_eq!(
            unload_after_custom_label("Custom…", &ModelUnloadTimeout::Never),
            "Custom…"
        );
    }

    #[test]
    fn label_prefers_resident_model_over_selection() {
        // The failed-switch / pending-unload divergence: selected=small but
        // large is what is actually in memory — the label shows the resident
        // model.
        let resident = Some(("large".to_string(), "Large".to_string()));
        let models = vec![
            ("large".to_string(), "Large".to_string()),
            ("small".to_string(), "Small".to_string()),
        ];
        assert_eq!(
            resolve_model_label_name(resident.as_ref(), "small", &models, "Model"),
            "Large"
        );
    }

    #[test]
    fn label_falls_back_to_selected_model_then_localized_fallback() {
        let models = vec![("small".to_string(), "Small".to_string())];
        assert_eq!(
            resolve_model_label_name(None, "small", &models, "Model"),
            "Small"
        );
        // Selection not in the downloaded list -> localized fallback.
        assert_eq!(
            resolve_model_label_name(None, "missing", &models, "Modell"),
            "Modell"
        );
        assert_eq!(
            resolve_model_label_name(None, "x", &[], "Modello"),
            "Modello"
        );
    }

    #[test]
    fn uses_post_processed_text_when_available() {
        let entry = build_entry("raw", Some("processed"));
        assert_eq!(last_transcript_text(&entry), "processed");
    }

    #[test]
    fn falls_back_to_raw_transcription() {
        let entry = build_entry("raw", None);
        assert_eq!(last_transcript_text(&entry), "raw");
    }

    #[test]
    fn tray_icon_resolution_failure_is_returned_instead_of_panicking() {
        assert!(load_tray_icon(Err(tauri::Error::UnknownPath)).is_err());
    }

    #[test]
    fn tray_icon_returns_err_when_file_does_not_exist() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let missing = dir.path().join("does_not_exist.png");
        assert!(load_tray_icon(Ok(missing)).is_err());
    }

    #[test]
    fn recording_and_transcribing_share_a_menu() {
        // The icon differs but the menu inputs are identical, so a
        // Recording -> Transcribing transition must not rebuild the menu.
        let recording = TrayDesired {
            icon_path: "resources/tray_recording.png",
            menu: inputs(TrayIconState::Recording.is_busy()),
        };
        let transcribing = TrayDesired {
            icon_path: "resources/tray_transcribing.png",
            menu: inputs(TrayIconState::Transcribing.is_busy()),
        };
        assert_ne!(recording.icon_path, transcribing.icon_path);
        assert_eq!(recording.menu, transcribing.menu);
    }

    #[test]
    fn idle_and_busy_menus_differ() {
        assert_ne!(inputs(false), inputs(true));
    }
}
