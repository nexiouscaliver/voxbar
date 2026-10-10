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
use std::time::{Duration, Instant};
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
    /// submenu label prefers this over `selected_model` - the two diverge in
    /// the failed-switch and pending-unload states (spec F4).
    resident_model: Option<(String, String)>,
    /// Pre-formatted resident-footprint segment for the submenu label and
    /// tooltip (`697 MB` measured / `~697 MB` estimate); `None` omits the
    /// segment. A change drives a menu rebuild via this struct's `PartialEq`.
    model_ram: Option<String>,
    /// Persisted idle-unload timeout, driving the "Unload After" submenu's
    /// checkmark (and its Custom… seconds hint).
    unload_timeout: ModelUnloadTimeout,
    /// Whether the local post-process engine is the active provider (and
    /// post-processing is on at all): the Post-process Model submenu only
    /// exists in that case, so a cloud/off user keeps the prior menu shape.
    post_process_local_active: bool,
    /// Whether post-processing is on at all: the Post-process Prompt
    /// submenu stays in the menu while off but renders DISABLED (KB-188) -
    /// picking a template with the feature off would silently move a
    /// checkmark that affects nothing. The update-checks item's greyed-out
    /// pattern, not the forced-off removal.
    post_process_enabled: bool,
    /// The effective post-process model selection (normalized by the same
    /// helper the swap runner uses).
    selected_llm_model: String,
    /// `(id, name)` of DOWNLOADED post-process LLM models, sorted by name -
    /// the LocalLlm mirror of `downloaded_models` (which excludes them).
    downloaded_llm_models: Vec<(String, String)>,
    /// `(id, name)` of every prompt template in the library, in catalog
    /// order - drives the Post-process Prompt submenu.
    post_process_prompts: Vec<(String, String)>,
    /// The selected template id, when one is selected (checkmark state).
    post_process_selected_prompt_id: Option<String>,
    /// macOS menu-bar title (resident model + compact RAM), set via
    /// `TrayIcon::set_title`. `None` (nothing resident) clears it. Only
    /// computed on macOS - Windows does not support tray titles and showing
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

    // The post-process model submenu's inputs: DOWNLOADED LocalLlm entries
    // only (never get_available_models, which filters them out), plus the
    // normalized selection. The submenu exists only when the local engine is
    // the active post-process provider and post-processing is enabled.
    let mut downloaded_llm_models: Vec<(String, String)> = app
        .state::<Arc<ModelManager>>()
        .get_available_llm_models()
        .into_iter()
        .filter(|m| m.is_downloaded)
        .map(|m| (m.id, m.name))
        .collect();
    downloaded_llm_models.sort_by(|a, b| a.1.cmp(&b.1));
    let post_process_local_active = settings.post_process_enabled
        && settings.post_process_provider_id == crate::settings::LOCAL_LLM_PROVIDER_ID;
    let selected_llm_model = crate::local_llm::manager::selected_llm_model_id(app);

    // The Post-process Prompt submenu's inputs: the whole template library
    // in catalog order plus the selection. Present unconditionally (unlike
    // the model submenu) but DISABLED while post-processing is off (KB-188):
    // the submenu is how an unbound-cycle operator switches templates from
    // the tray, and with the feature off a pick would silently move a
    // checkmark that affects nothing.
    let post_process_prompts: Vec<(String, String)> = settings
        .post_process_prompts
        .iter()
        .map(|p| (p.id.clone(), p.name.clone()))
        .collect();
    let post_process_selected_prompt_id = settings.post_process_selected_prompt_id.clone();

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
    // load/unload-driven snapshot as the menu is, PLUS the periodic RAM
    // refresh loop (see [`start_ram_refresh`]) while a model is resident, and
    // gated by the menu_bar_model_title setting (desired_tray_title returns
    // None for every input when it is off - the applier then clears the
    // native title via an explicit empty string).
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
            post_process_local_active,
            post_process_enabled: settings.post_process_enabled,
            selected_llm_model,
            downloaded_llm_models,
            post_process_prompts,
            post_process_selected_prompt_id,
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
    let (desired, icon, icon_changed, menu_changed, title_action) = {
        let mut inner = state.lock();
        inner.pending = false;
        let Some(desired) = inner.desired.clone() else {
            return;
        };
        let icon_changed = inner.applied_icon != Some(desired.icon_path);
        let menu_changed = inner.applied_menu.as_ref() != Some(&desired.menu);
        let title_action = title_reconciliation(
            inner.applied_title.as_deref(),
            desired.menu.title.as_deref(),
        );
        let title_changed = title_action != TitleAction::Keep;
        if !icon_changed && !menu_changed && !title_changed {
            trace!("tray apply: nothing changed");
            return;
        }
        let icon = inner.icons.get(desired.icon_path).cloned();
        (desired, icon, icon_changed, menu_changed, title_action)
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
    // `applied_title` so a failed `set_title` is retried on the next sync -
    // the title is the loaded-state indicator and worth one retry, and it can
    // change without the menu rebuilding only in exotic partial-failure cases.
    // Clearing MUST carry an explicit empty string to the native layer (see
    // [`native_title_arg`]); `title_action` decided Keep/Set/Clear above.
    let mut title_ok = false;
    if title_action != TitleAction::Keep {
        let native_arg = Some(native_title_arg(desired.menu.title.as_deref()));
        if set_tray_title(&tray, native_arg) {
            title_ok = true;
        }
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
        // Build model submenu - the label shows the RESIDENT model and its
        // footprint when one is loaded, falling back to the selection.
        let model_name = resolve_model_label_name(
            inputs.resident_model.as_ref(),
            &inputs.selected_model,
            &inputs.downloaded_models,
            &strings.model,
        );
        let submenu_label = match &inputs.model_ram {
            Some(ram) => format!("{model_name} - {ram}"),
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

        // "Post-process Model" submenu: downloaded LocalLlm models with a
        // checkmark on the effective selection, mirroring the voice model
        // submenu above. Fed exclusively by the LocalLlm list
        // (get_available_llm_models); only shown while the local engine is
        // the active post-process provider, so cloud/off users keep the
        // exact menu shape they had before.
        let post_process_model_submenu = if inputs.post_process_local_active {
            let selected_name = llm_submenu_label_name(inputs)
                .unwrap_or_else(|| strings.post_process_model.clone());
            let submenu = Submenu::with_id(
                app,
                "post_process_model_submenu",
                &format!("{}: {}", strings.post_process_model, selected_name),
                true,
            )?;
            for ((id, name), checked) in llm_submenu_checks(inputs) {
                let item_id = format!("llm_select:{}", id);
                let item =
                    CheckMenuItem::with_id(app, &item_id, &name, true, checked, None::<&str>)?;
                submenu.append(&item)?;
            }
            Some(submenu)
        } else {
            None
        };

        // "Post-process Prompt" submenu: the whole template library with a
        // checkmark on the selection, mirroring the model submenus above.
        // Present unconditionally (it is the tray surface for picking a
        // template, independent of which engine runs it) but DISABLED while
        // post-processing is off - the update-checks item's enabled-flag
        // pattern - so a pick that would silently move a checkmark
        // affecting nothing (KB-188) is unclickable instead.
        let prompt_selected_name = prompt_submenu_label_name(inputs)
            .unwrap_or_else(|| strings.post_process_prompt.clone());
        let post_process_prompt_submenu = Submenu::with_id(
            app,
            "post_process_prompt_submenu",
            &format!("{}: {}", strings.post_process_prompt, prompt_selected_name),
            inputs.post_process_enabled,
        )?;
        for ((id, name), checked) in prompt_submenu_checks(inputs) {
            let item_id = format!("prompt_select:{}", id);
            let item = CheckMenuItem::with_id(app, &item_id, &name, true, checked, None::<&str>)?;
            post_process_prompt_submenu.append(&item)?;
        }

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

        // Assembled dynamically: the Post-process Model submenu is
        // conditional, so the separator after the model block is only
        // present when that submenu is. Bound items (not temporaries) so
        // they outlive the builder call.
        let sep_after_version = separator()?;
        let sep_after_copy = separator()?;
        let sep_after_models = separator()?;
        let sep_after_updates = separator()?;
        let mut idle_items: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> = vec![
            &version_i,
            &sep_after_version,
            &copy_last_transcript_i,
            &sep_after_copy,
            &model_submenu,
            &unload_model_i,
            &unload_after_submenu,
        ];
        if let Some(submenu) = &post_process_model_submenu {
            idle_items.push(submenu);
        }
        idle_items.push(&post_process_prompt_submenu);
        for item in [
            &sep_after_models as &dyn tauri::menu::IsMenuItem<tauri::Wry>,
            &settings_i,
            &check_updates_i,
            &sep_after_updates,
            &quit_i,
        ] {
            idle_items.push(item);
        }
        Menu::with_items(app, &idle_items)?
    };

    // When update checks are forced off (e.g. HANDY_DISABLE_UPDATER, set by
    // the Nix package), the item is dropped from the menu rather than shown
    // disabled - it can never do anything in that case, and a disabled item
    // still shifts every entry below it by one position. A manually-disabled
    // toggle in Debug Settings keeps the old greyed-out behavior via the
    // enabled flag.
    if settings::update_checks_forced_disabled() {
        menu.remove(&check_updates_i)?;
    }

    // Platforms without shipped updater artifacts get no Check for Updates
    // item either: only the macOS release pipeline signs and publishes
    // update payloads (latest.json carries darwin entries alone), so the
    // in-app check on Windows/Linux can only error or find nothing. The
    // frontend keeps the same predicate (updaterPlatform.ts); flipping this
    // requires shipping artifacts first (BUILD.md "Releasing").
    #[cfg(not(target_os = "macos"))]
    menu.remove(&check_updates_i)?;

    // Both layouts start with [version, separator, ...]; slot the warning in
    // right below the version line so it's the first actionable thing seen.
    // The tooltip mirrors the resident model + footprint segment (spec F4).
    let mut tooltip = version_label;
    if let Some((_, resident_name)) = &inputs.resident_model {
        tooltip = match &inputs.model_ram {
            Some(ram) => format!("{tooltip} - {resident_name} - {ram}"),
            None => format!("{tooltip} - {resident_name}"),
        };
    }
    if let Some(warning_item) = secure_input_warning {
        menu.insert(&warning_item, 2)?;
        menu.insert(&separator()?, 3)?;
        tooltip = format!("{} - {}", tooltip, warning_item.text().unwrap_or_default());
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
/// omits the segment entirely. Pure - menu building never fails on
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

/// The Post-process Model submenu's entries with their checkmark state:
/// every DOWNLOADED LocalLlm model (computed upstream, never the ASR
/// list), checked exactly when it is the effective selection. Pure, so
/// the listing/checkmark semantics are unit-testable without an app.
fn llm_submenu_checks(inputs: &MenuInputs) -> Vec<((String, String), bool)> {
    inputs
        .downloaded_llm_models
        .iter()
        .map(|(id, name)| ((id.clone(), name.clone()), *id == inputs.selected_llm_model))
        .collect()
}

/// The submenu label's name segment: the selected model's display name,
/// when the selection resolves inside the downloaded list. Pure.
fn llm_submenu_label_name(inputs: &MenuInputs) -> Option<String> {
    inputs
        .downloaded_llm_models
        .iter()
        .find(|(id, _)| *id == inputs.selected_llm_model)
        .map(|(_, name)| name.clone())
}

/// The Post-process Prompt submenu's entries with their checkmark state:
/// every template in the library (catalog order), checked exactly when it
/// is the selection. Pure, mirroring [`llm_submenu_checks`].
fn prompt_submenu_checks(inputs: &MenuInputs) -> Vec<((String, String), bool)> {
    inputs
        .post_process_prompts
        .iter()
        .map(|(id, name)| {
            (
                (id.clone(), name.clone()),
                Some(id.as_str()) == inputs.post_process_selected_prompt_id.as_deref(),
            )
        })
        .collect()
}

/// The prompt submenu label's name segment: the selected template's name,
/// when the selection resolves inside the library. Pure.
fn prompt_submenu_label_name(inputs: &MenuInputs) -> Option<String> {
    inputs
        .post_process_prompts
        .iter()
        .find(|(id, _)| Some(id.as_str()) == inputs.post_process_selected_prompt_id.as_deref())
        .map(|(_, name)| name.clone())
}

/// Resolve the model submenu label's name part (pure - extracted from
/// `build_menu` so the precedence is unit-testable without an app):
/// prefer the RESIDENT model (what is actually in memory - the
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
/// example (`Parakeet EN · 768M`) exactly - the "~16 characters" guidance
/// budgets this class of length.
const TRAY_TITLE_MAX_CHARS: usize = 18;

/// The title `compute_desired` wants, given the `menu_bar_model_title`
/// setting: off → `None` for EVERY input (even with a model resident - the
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
/// (`Parakeet EN · 768M`). Pure - `None` in, `None` out (nothing resident
/// clears the title entirely). The name is truncated (with `…`) when the
/// combined title would exceed [`TRAY_TITLE_MAX_CHARS`] characters; the RAM
/// segment is never truncated. A missing footprint yields a name-only title.
///
/// NOT the function `compute_desired` calls - use [`desired_tray_title`],
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

/// What the applier should do with the native menu-bar title for a given
/// (applied, desired) pair. Pure, so the reconciliation decisions are
/// unit-testable without an app or a native tray: `Keep` (no native call),
/// `Set` (show/replace the string) and `Clear` (the title must be removed
/// from the menu bar - model unloaded/deleted, or the
/// `menu_bar_model_title` setting turned off).
#[derive(Clone, Debug, PartialEq, Eq)]
enum TitleAction {
    Keep,
    Set(String),
    Clear,
}

/// The applier's native-title decision for a diff. Pure.
fn title_reconciliation(applied: Option<&str>, desired: Option<&str>) -> TitleAction {
    match (applied, desired) {
        // Nothing shown, nothing wanted (the common idle case).
        (None, None) => TitleAction::Keep,
        // Same string: no native work.
        (Some(applied), Some(desired)) if applied == desired => TitleAction::Keep,
        // A title is wanted and differs from what is displayed.
        (_, Some(desired)) => TitleAction::Set(desired.to_string()),
        // Something is displayed but nothing is wanted: CLEAR.
        (Some(_), None) => TitleAction::Clear,
    }
}

/// The string argument the native `set_title` call must receive for a
/// desired title. A `None` desired title MUST become an explicit empty
/// string: tray-icon 0.24's macOS `set_title_inner` only touches the
/// `NSStatusBarButton` inside `if let Some(title) = title`, so passing
/// `None` through to the native layer is a SILENT NO-OP - the stale title
/// stays on the menu bar forever, while tauri still reports `Ok`, which
/// would poison our `applied_title` tracking into never retrying. That is
/// exactly how the stuck-title regression shipped in v1.0.0 (every unload
/// path and the live settings toggle left the old text behind);
/// `setTitle("")` is what actually clears the text.
fn native_title_arg(desired: Option<&str>) -> &str {
    desired.unwrap_or("")
}

/// Apply the menu-bar title to the native tray (macOS). `false` when the
/// native call failed, so the applier does not record it as displayed.
/// Callers pass [`native_title_arg`] output so a `None` desired title is
/// translated to the explicit empty-string clear.
#[cfg(target_os = "macos")]
fn set_tray_title(tray: &TrayIcon, title: Option<&str>) -> bool {
    trace!("Setting tray title to {:?}", title);
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

// --- Resident-model RAM refresh ---------------------------------------------

/// While a model is resident, the footprint-dependent tray parts (model
/// submenu RAM segment, tooltip, macOS menu-bar title) are refreshed on this
/// cadence, so the numbers track the live worker instead of whatever the
/// load-time snapshot happened to be. The applier's diff skips all native
/// work whenever the formatted segment is unchanged, so a steady footprint
/// costs one probe and no native calls.
const TRAY_RAM_REFRESH_INTERVAL: Duration = Duration::from_secs(10);

/// Tauri managed state owning the resident-model refresh task. Absent (or a
/// finished task) means no polling at all - zero background work runs while
/// no model is resident.
pub struct TrayRamRefresh(Mutex<Option<tauri::async_runtime::JoinHandle<()>>>);

impl TrayRamRefresh {
    pub fn new() -> Self {
        Self(Mutex::new(None))
    }

    fn lock(&self) -> MutexGuard<'_, Option<tauri::async_runtime::JoinHandle<()>>> {
        self.0.lock().unwrap_or_else(|poisoned| {
            warn!("Tray RAM refresh mutex was poisoned, recovering");
            poisoned.into_inner()
        })
    }
}

impl Default for TrayRamRefresh {
    fn default() -> Self {
        Self::new()
    }
}

/// Start the periodic RAM refresh unless one is already running. Idempotent,
/// so callers can reconcile on every model-state change without tracking
/// residency themselves.
pub fn start_ram_refresh(app: &AppHandle) {
    let Some(state) = app.try_state::<TrayRamRefresh>() else {
        return;
    };
    let mut running = state.lock();
    if running
        .as_ref()
        .is_some_and(|task| !task.inner().is_finished())
    {
        return; // A refresh loop is already alive.
    }

    let task_app = app.clone();
    let task = tauri::async_runtime::spawn(async move {
        let mut ticker = tokio::time::interval(TRAY_RAM_REFRESH_INTERVAL);
        // interval's first tick completes immediately; the load that started
        // the refresher just applied a fresh snapshot, so consume it.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let resident = task_app
                .try_state::<Arc<TranscriptionManager>>()
                .is_some_and(|transcription| transcription.is_model_loaded());
            if !resident {
                // Residency ended without the "unloaded" hook firing (or the
                // abort raced): stop polling instead of spinning.
                debug!("tray RAM refresh: model no longer resident, stopping");
                break;
            }
            // One footprint read + a desired-state recompute; the applier
            // diffs against what is displayed and touches native state only
            // when the formatted segment actually changed.
            sync_tray(&task_app);
        }
    });
    *running = Some(task);
}

/// Stop the periodic RAM refresh if one is running (model no longer
/// resident, or the app is quitting). Idempotent.
pub fn stop_ram_refresh(app: &AppHandle) {
    let Some(state) = app.try_state::<TrayRamRefresh>() else {
        return;
    };
    let mut running = state.lock();
    if let Some(task) = running.take() {
        task.abort();
    }
}

/// Reconcile the periodic RAM refresh with actual residency: running while
/// (and only while) a model is loaded. Called on every model-state change,
/// which every load/unload path already emits or follows with a tray sync.
pub fn reconcile_ram_refresh(app: &AppHandle) {
    let resident = app
        .try_state::<Arc<TranscriptionManager>>()
        .is_some_and(|transcription| transcription.is_model_loaded());
    if resident {
        start_ram_refresh(app);
    } else {
        stop_ram_refresh(app);
    }
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
/// for a second process) - the natural "where did my icon go?" moment - so a
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
        format_tray_title, get_icon_path, last_transcript_text, llm_submenu_checks,
        llm_submenu_label_name, load_tray_icon, native_title_arg, prompt_submenu_checks,
        prompt_submenu_label_name, resolve_model_label_name, title_reconciliation,
        unload_after_custom_label, unload_after_preset_is_active, AppTheme, MenuInputs,
        TitleAction, TrayDesired, TrayIconState, TRAY_TITLE_MAX_CHARS,
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
            post_process_provider: None,
            post_process_model: None,
            post_process_prompt_id: None,
            post_process_outcome: None,
            post_process_latency_ms: None,
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
            post_process_local_active: false,
            post_process_enabled: false,
            selected_llm_model: crate::local_llm::LOCAL_LLM_MODEL_ID.to_string(),
            downloaded_llm_models: Vec::new(),
            post_process_prompts: Vec::new(),
            post_process_selected_prompt_id: None,
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

    /// KB-188: the Post-process Prompt submenu's enabled state rides the
    /// post_process_enabled flag build_menu passes to Submenu::with_id, so
    /// flipping the master toggle must change MenuInputs - without that the
    /// applier would see no diff and the submenu would stay enabled (or
    /// disabled) until the next unrelated rebuild.
    #[test]
    fn post_process_toggle_flip_drives_menu_rebuild() {
        let mut enabled = inputs(false);
        enabled.post_process_enabled = true;
        assert_ne!(
            inputs(false),
            enabled,
            "toggle flip => MenuInputs differ => the menu rebuilds"
        );
    }

    /// The Post-process Model submenu lists exactly the downloaded LocalLlm
    /// models with ONE checkmark, on the effective selection - and a
    /// selection change drives a menu rebuild through MenuInputs' equality.
    #[test]
    fn llm_submenu_checks_list_downloaded_models_with_one_checkmark() {
        let mut with_llm = inputs(false);
        with_llm.post_process_local_active = true;
        with_llm.downloaded_llm_models = vec![
            (
                "Qwen/Qwen3-0.6B-GGUF/Qwen3-0.6B-Q8_0.gguf".to_string(),
                "Qwen3 0.6B".to_string(),
            ),
            ("org/other/model.gguf".to_string(), "Other".to_string()),
        ];
        with_llm.selected_llm_model = "org/other/model.gguf".to_string();

        let checks = llm_submenu_checks(&with_llm);
        assert_eq!(checks.len(), 2, "downloaded LLM models only");
        let checked: Vec<_> = checks.iter().filter(|(_, c)| *c).collect();
        assert_eq!(checked.len(), 1, "exactly one checkmark");
        assert_eq!(checked[0].0 .0, "org/other/model.gguf");
        assert_eq!(
            llm_submenu_label_name(&with_llm).as_deref(),
            Some("Other"),
            "the label names the selected model"
        );

        // Selection change => MenuInputs differ => the applier rebuilds.
        let mut reselected = with_llm.clone();
        reselected.selected_llm_model = "Qwen/Qwen3-0.6B-GGUF/Qwen3-0.6B-Q8_0.gguf".to_string();
        assert_ne!(with_llm, reselected);
        // And a download joining the list rebuilds too.
        let mut grown = with_llm.clone();
        grown
            .downloaded_llm_models
            .push(("x/y.gguf".to_string(), "X".to_string()));
        assert_ne!(with_llm, grown);
    }

    /// The Post-process Prompt submenu lists the whole library in catalog
    /// order with ONE checkmark on the selection; an empty library yields
    /// an empty submenu and the label falls back (the localized title), and
    /// a selection change drives a rebuild through MenuInputs' equality.
    #[test]
    fn prompt_submenu_lists_the_library_with_one_checkmark() {
        let mut with_prompts = inputs(false);
        with_prompts.post_process_prompts = vec![
            (
                "default_improve_transcriptions".to_string(),
                "English Professional".to_string(),
            ),
            ("english_casual".to_string(), "English Casual".to_string()),
            ("prompt_42".to_string(), "My Notes copy".to_string()),
        ];
        with_prompts.post_process_selected_prompt_id = Some("english_casual".to_string());

        let checks = prompt_submenu_checks(&with_prompts);
        assert_eq!(checks.len(), 3, "the whole library, builtin or not");
        assert_eq!(checks[0].0 .0, "default_improve_transcriptions");
        let checked: Vec<_> = checks.iter().filter(|(_, c)| *c).collect();
        assert_eq!(checked.len(), 1, "exactly one checkmark");
        assert_eq!(checked[0].0 .0, "english_casual");
        assert_eq!(
            prompt_submenu_label_name(&with_prompts).as_deref(),
            Some("English Casual"),
            "the label names the selected template"
        );

        // Selection change => MenuInputs differ => the applier rebuilds.
        let mut reselected = with_prompts.clone();
        reselected.post_process_selected_prompt_id = Some("prompt_42".to_string());
        assert_ne!(with_prompts, reselected);

        // Nothing selected: no checkmark, no name segment.
        let mut unselected = with_prompts.clone();
        unselected.post_process_selected_prompt_id = None;
        assert!(
            !prompt_submenu_checks(&unselected).iter().any(|(_, c)| *c),
            "no checkmark without a selection"
        );
        assert_eq!(prompt_submenu_label_name(&unselected), None);

        // A dangling selection (deleted template) resolves to no checkmark.
        let mut dangling = with_prompts;
        dangling.post_process_selected_prompt_id = Some("gone".to_string());
        assert!(
            !prompt_submenu_checks(&dangling).iter().any(|(_, c)| *c),
            "a dangling id never checks anything"
        );
        assert_eq!(prompt_submenu_label_name(&dangling), None);
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

    /// KB-034's mirror: the update-checks item's greyed-out state rides
    /// update_checks_enabled, and the menu renders in the app's locale, so
    /// flipping either must change MenuInputs - without that the applier
    /// would see no diff and update_tray_menu's re-sync after
    /// change_update_checks_enabled_setting (or a language change) would be
    /// a no-op, leaving the item's state stale until the next unrelated
    /// rebuild.
    #[test]
    fn menu_inputs_differ_on_update_checks_and_locale_change() {
        let mut no_checks = inputs(false);
        no_checks.update_checks_enabled = false;
        assert_ne!(
            inputs(false),
            no_checks,
            "update-checks toggle flip => MenuInputs differ => the menu rebuilds"
        );

        let mut german = inputs(false);
        german.locale = "de".to_string();
        assert_ne!(
            inputs(false),
            german,
            "locale flip => MenuInputs differ => the menu rebuilds (its strings re-localize)"
        );
    }

    /// The theme flip lives on the ICON leg of TrayDesired, not MenuInputs
    /// (the menu carries no theme): a theme switch must pick a different
    /// icon path per state, which is the diff that makes the tray re-sync
    /// after a theme change matter.
    #[test]
    fn theme_flip_changes_the_tray_icon_path() {
        for state in [
            TrayIconState::Idle,
            TrayIconState::Recording,
            TrayIconState::Transcribing,
        ] {
            assert_ne!(
                get_icon_path(AppTheme::Dark, state, false),
                get_icon_path(AppTheme::Light, state, false),
                "{state:?}: a dark/light theme flip must pick a different icon"
            );
        }
        assert_ne!(
            get_icon_path(AppTheme::Dark, TrayIconState::Idle, false),
            get_icon_path(AppTheme::Colored, TrayIconState::Idle, false),
            "the colored (Linux) theme picks its own icon"
        );
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

    // NOTE: the tests below exercise the applier's DECISION layer only
    // (title_reconciliation + native_title_arg). The live NSStatusItem
    // behavior of TrayIcon::set_title - including the tray-icon 0.24.1
    // macOS no-op on None that these decisions work around - cannot be
    // exercised in a unit test: it needs a running macOS event loop.

    #[test]
    fn title_reconciliation_keeps_unchanged_titles() {
        // Idle baseline: nothing shown, nothing wanted.
        assert_eq!(title_reconciliation(None, None), TitleAction::Keep);
        // Same string: no native call, so the 10s RAM refresh stays free
        // whenever the formatted footprint did not change.
        assert_eq!(
            title_reconciliation(Some("Small · 300M"), Some("Small · 300M")),
            TitleAction::Keep
        );
    }

    #[test]
    fn title_reconciliation_sets_new_and_changed_titles() {
        assert_eq!(
            title_reconciliation(None, Some("Small · 300M")),
            TitleAction::Set("Small · 300M".to_string())
        );
        assert_eq!(
            title_reconciliation(Some("Old · 1M"), Some("New · 2M")),
            TitleAction::Set("New · 2M".to_string())
        );
    }

    #[test]
    fn applier_requests_native_clear_when_desired_title_becomes_none() {
        // The stuck-title regression: every Some -> None transition (manual
        // unload, model delete, idle-timeout expiry, menu_bar_model_title
        // turned off) must ask the native layer to CLEAR...
        assert_eq!(
            title_reconciliation(Some("Whisper Base · 243M"), None),
            TitleAction::Clear
        );
        // ...and the native call must carry an explicit empty string:
        // tray-icon 0.24.1's macOS backend no-ops on None, which is how the
        // stale title survived every unload path in v1.0.0.
        assert_eq!(native_title_arg(None), "");
    }

    #[test]
    fn native_title_arg_passes_desired_title_through() {
        assert_eq!(native_title_arg(Some("Small · 300M")), "Small · 300M");
        // Defensive: an empty desired title also clears (same native arg).
        assert_eq!(native_title_arg(Some("")), "");
    }

    #[test]
    fn measured_display_footprint_renders_through_both_formatters() {
        // A display-footprint reading for e.g. an 85 MB Q8 model's worker
        // (weights + real runtime overhead) renders through both the submenu
        // segment and the compact menu-bar title form.
        let bytes = 150 * 1024 * 1024;
        assert_eq!(
            format_ram_segment(Some((bytes, true))),
            Some("150 MB".to_string())
        );
        assert_eq!(compact_ram(bytes), "150M");
    }

    #[test]
    fn disabled_menu_bar_title_never_produces_one_even_with_a_model_resident() {
        let mib = 1024 * 1024;
        // Setting off: None for every resident/footprint combination a
        // loaded model can produce - compute_desired feeds exactly these
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
        // large is what is actually in memory - the label shows the resident
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
