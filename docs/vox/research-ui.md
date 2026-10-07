# Research: Tray + Settings UI (upstream cjpais/Handy, vendored at `handy-dictation/`)

Scope: `src-tauri/src/tray.rs`, `src-tauri/src/tray_i18n.rs`, `src-tauri/build.rs` (tray-translation codegen), `src-tauri/src/settings.rs`, the Tauri command layer (`src-tauri/src/shortcut/mod.rs`, `src-tauri/src/commands/`, `src-tauri/src/lib.rs`), and the frontend settings stack (`src/stores/settingsStore.ts`, `src/hooks/useSettings.ts`, `src/components/settings/**`, `src/components/Sidebar.tsx`, `src/components/ui/*`), plus i18n conventions. All paths below are relative to `handy-dictation/`. Every fact cites `path:line` from this session's reads.

---

## 1. The tray (`src-tauri/src/tray.rs`)

### 1.1 Architecture: desired-state diffing, single main-thread writer

The tray does **not** rebuild on every change. Callers record intent into a managed `TrayState` (a `Mutex<TrayInner>`, `src-tauri/src/tray.rs:99`), and a single applier on the main thread diffs the desired snapshot against what is displayed and only touches the native tray for parts that actually changed (`src-tauri/src/tray.rs:1-21` module doc). This was built to fix the macOS tray-disappearance bug (tauri#12060 / Handy #1948): previously every recording cycle rebuilt the full menu 3-6× from several threads (`src-tauri/src/tray.rs:11-14`).

Key types:
- `TrayIconState { Idle, Recording, Transcribing }` — `src-tauri/src/tray.rs:39-44`. Only the idle/busy distinction matters for the menu; `Recording` and `Transcribing` share one menu (`src-tauri/src/tray.rs:46-52`, pinned by test `recording_and_transcribing_share_a_menu` at `src-tauri/src/tray.rs:725-738`).
- `MenuInputs { busy, warning, model_loaded, selected_model, downloaded_models: Vec<(id,name)>, locale, update_checks_enabled }` — everything the menu+tooltip depend on; when two snapshots compare `PartialEq`-equal the menu is not rebuilt (`src-tauri/src/tray.rs:54-66`).
- `TrayDesired { icon_path, menu: MenuInputs }` — complete description of the tray (`src-tauri/src/tray.rs:68-73`).
- `TrayInner` tracks `applied_icon`/`applied_menu` (only recorded when the native call succeeded, so transient failures retry on next sync — `src-tauri/src/tray.rs:79-96`, `379-422`), a `pending` flag for coalescing, a decoded-icon cache `HashMap<&'static str, Image>` so the main thread never touches disk (`src-tauri/src/tray.rs:89-90`, `267-282`), and seq numbers so a slow request can't overwrite a newer snapshot (`src-tauri/src/tray.rs:91-95`, `289-297`).

Public entry points (`src-tauri/src/tray.rs:217-243`):
- `set_tray_state(app, state)` — sets icon state (Recording/Transcribing/Idle) → full sync.
- `refresh_tray_icon(app)` — re-sync after theme or Secure Input warning change (state preserved).
- `update_tray_menu(app)` — re-sync after model list/selection/loaded state, language, or settings change. **Note: despite the name it re-syncs icon+menu both** — both funnel into `sync_tray_with` (`src-tauri/src/tray.rs:241-243`).
- `sync_tray(app)` / `sync_tray_with` — record intent + schedule one main-thread apply (`post_apply` → `run_on_main_thread` → `apply_on_main`, `src-tauri/src/tray.rs:341-435`). Requests arriving while an apply is pending are coalesced (`src-tauri/src/tray.rs:300-309`). The snapshot is computed on the *calling* thread on purpose so the main-thread applier never takes manager locks a worker may hold across slow work (#1716, `src-tauri/src/tray.rs:236-240`). Early calls before the tray exists are kept and picked up later (`src-tauri/src/tray.rs:259-263`).

Exceptions that call the tray directly: `set_tray_visibility` (`src-tauri/src/tray.rs:604-611`) and `recreate_tray_icon` (macOS recovery: hide+show to resurrect a vanished `NSStatusItem`, called on relaunch/`RunEvent::Reopen`/single-instance callback; `src-tauri/src/tray.rs:613-636`, wired at `src-tauri/src/lib.rs:1104-1121`). Re-showing a hidden tray relies on tray-icon recreating from the last *applied* icon/menu/tooltip, which is why icon/menu/tooltip must only be set through the applier (`src-tauri/src/tray.rs:16-21`).

### 1.2 Menu items

Built by `build_menu(app, &MenuInputs)` — pure with respect to app state (`src-tauri/src/tray.rs:454-595`). Two layouts:

**Idle layout** (`src-tauri/src/tray.rs:557-572`), top to bottom:
1. `version` — disabled item showing `Handy v{CARGO_PKG_VERSION}` (+" (Dev)" under `debug_assertions`); `version_label()` at `src-tauri/src/tray.rs:446-452`, item at `:490`.
2. separator
3. `copy_last_transcript` — copies latest completed history entry's post-processed text (fallback raw) to clipboard (`src-tauri/src/tray.rs:597-602`, `638-667`).
4. separator
5. `model_submenu` — Submenu whose **label is the active model's display name** (falls back to localized "Model"); contains one `CheckMenuItem` per downloaded model, id `model_select:{model_id}`, checked on the active one (`src-tauri/src/tray.rs:533-547`).
6. `unload_model` — enabled iff a model is loaded (`inputs.model_loaded`, `src-tauri/src/tray.rs:549-555`).
7. separator
8. `settings` — accelerator Cmd+, / Ctrl+, (`src-tauri/src/tray.rs:483-486`, `491-497`).
9. `check_updates` — enabled per `inputs.update_checks_enabled` (`src-tauri/src/tray.rs:498-504`).
10. separator
11. `quit` — Cmd+Q / Ctrl+Q (`src-tauri/src/tray.rs:512`).

**Busy layout (Recording or Transcribing)** (`src-tauri/src/tray.rs:515-531`): same items except the model submenu + unload are replaced by a single `cancel` item (id `cancel`), placed above `copy_last_transcript`.

**Conditional items:**
- `secure_input_warning` (macOS Secure Input active): inserted at index 2 (right below version, followed by its own separator) so it's the first actionable item (`src-tauri/src/tray.rs:461-480`, `586-592`). Locales missing the key get the English string rather than a blank item (`src-tauri/src/tray.rs:464-470`).
- `check_updates` is *removed entirely* (not greyed) when `HANDY_DISABLE_UPDATER` forces update checks off (Nix builds) — `settings::update_checks_forced_disabled()` at `src-tauri/src/tray.rs:575-583`; a user-disabled toggle keeps the greyed-out behavior via the enabled flag.

**Status text shown in the tray:**
- The **version line** ("Handy v0.x.y", disabled item) is the only always-present text.
- The **tooltip** is `version_label()`, extended to `"{version} — {warning label}"` when the Secure Input warning is active (`src-tauri/src/tray.rs:587-592`; `tray_tooltip()` at `:442-444`). Tooltip set is best-effort/logged, not retried (`src-tauri/src/tray.rs:399-406`).
- The **active model name** as the submenu label (`src-tauri/src/tray.rs:533-541`).
- There is **no recording/transcribing status *text*** — those states are conveyed by the icon only (idle/recording/transcribing PNGs, `src-tauri/src/tray.rs:191-215`); the busy state is otherwise visible only as the menu swap to "Cancel".

**Icon selection** (`get_icon_path`, `src-tauri/src/tray.rs:191-215`): Dark theme → light icons `resources/tray_{idle,recording,transcribing}.png`; Light theme → `*_dark.png`; Linux "Colored" theme → `resources/handy.png`, `recording.png`, `transcribing.png`. A Secure-Input `warning` (macOS-only) overlays a badge on the *idle* icon only (`tray_idle_warning.png` / `tray_idle_warning_dark.png`) so in-flight activity stays recognizable (`src-tauri/src/tray.rs:186-200`). Theme source: Linux always Colored; Windows reads the registry `SystemUsesLightTheme` (taskbar follows system theme, not app theme — `src-tauri/src/tray.rs:142-184`); others read the main window theme (`src-tauri/src/tray.rs:153-162`). System theme changes re-apply the tray via `WindowEvent::ThemeChanged` → `utils::refresh_tray_icon` (`src-tauri/src/lib.rs:1087-1091`).

### 1.3 Tray construction & event handling (lib.rs)

Built once in `initialize_core_logic` (`src-tauri/src/lib.rs:236-359`): `TrayIconBuilder` with initial idle icon, tooltip `tray_tooltip()`, `icon_as_template(true)` (`src-tauri/src/lib.rs:242-253`); on Windows left-click opens the main window and the menu is on right click, elsewhere menu-on-left-click (`src-tauri/src/lib.rs:255-282`). `TrayState` is managed before the tray exists (`src-tauri/src/lib.rs:218`).

`on_menu_event` (`src-tauri/src/lib.rs:285-341`):
- `settings` / `secure_input_warning` → `show_main_window` (the full Secure-Input banner lives in the settings window; comment at `src-tauri/src/lib.rs:289-291`).
- `check_updates` → only if `update_checks_effectively_enabled`, shows window + emits `check-for-updates` event (`src-tauri/src/lib.rs:293-299`).
- `copy_last_transcript` → `tray::copy_last_transcript`.
- `unload_model` → `TranscriptionManager::request_unload` (no-op warn if not loaded).
- `cancel` → `utils::cancel_current_operation` (which also resets the tray to Idle — `src-tauri/src/utils.rs:104`).
- `quit` → `app.exit(0)`.
- `model_select:{id}` → spawns a thread calling `commands::models::switch_active_model`, then `update_tray_menu` (`src-tauri/src/lib.rs:321-339`).

After build: initial `update_tray_menu` (`src-tauri/src/lib.rs:347`); hide the tray if `!settings.show_tray_icon` (`src-tauri/src/lib.rs:349-353`); and a listener on the `model-state-changed` event (emitted by `TranscriptionManager`, e.g. `src-tauri/src/managers/transcription.rs:421,482,539`) refreshes the tray menu (`src-tauri/src/lib.rs:355-359`).

Other tray sync triggers:
- Recording lifecycle: `actions.rs` sets `TrayIconState::Recording` on start (`src-tauri/src/actions.rs:422`), `Transcribing` when transcribing (`:581`), and `Idle` on every completion/error/cancel path (`:539,625,635,689,716-812`).
- Secure Input monitor: `secure_input.rs` calls `tray::refresh_tray_icon` when the warning flips (`src-tauri/src/secure_input.rs:287-292`; `tray_warning_active` at `:115-119` returns true only when a binding is degraded/dead).
- App language change: `change_app_language_setting` calls `tray::update_tray_menu` (`src-tauri/src/shortcut/mod.rs:1386-1395`).
- Show-tray-icon toggle: `tray::set_tray_visibility` immediately (`src-tauri/src/shortcut/mod.rs:1397-1408`).
- `--no-tray` CLI flag hides at startup (`src-tauri/src/lib.rs:1044-1047`); `recreate_tray_icon` honors both flag and setting (`src-tauri/src/tray.rs:621-627`).

### 1.4 Tray i18n (`src-tauri/src/tray_i18n.rs` + codegen)

`tray_i18n.rs` includes a **compile-time generated** `TrayStrings` struct + `TRANSLATIONS: Lazy<HashMap<&'static str, TrayStrings>>` from `$OUT_DIR/tray_translations.rs` (`src-tauri/src/tray_i18n.rs:20-21`). Generation lives in `build.rs::generate_tray_translations` (`src-tauri/build.rs:284-361`):
- Reads every `src/i18n/locales/*/translation.json` (path relative to src-tauri: `../src/i18n/locales`, `src-tauri/build.rs:290`), takes each file's `"tray"` object (`src-tauri/build.rs:311-313`).
- **English defines the schema**: struct fields are derived from the English `tray` keys, camelCase→snake_case (`src-tauri/build.rs:316-321`, `camel_to_snake` at `:363-374`). All languages auto-discovered; missing keys in a locale emit `""` (`src-tauri/build.rs:326-345` — `unwrap_or("")`).
- `cargo:rerun-if-changed` is set on the locales dir and each translation.json (`src-tauri/build.rs:292,306`).

The 8 tray keys (from `src/i18n/locales/en/translation.json` "tray" object, verified this session): `settings` ("Settings..."), `checkUpdates` ("Check for Updates..."), `copyLastTranscript` ("Copy Last Transcript"), `unloadModel` ("Unload Model"), `model` ("Model"), `quit` ("Quit"), `cancel` ("Cancel"), `secureInputWarning` ("⚠ Shortcuts blocked by Secure Input"). All 26 locales currently carry all 8 keys non-blank (verified by script over all `translation.json` files this session).

Lookup: `get_tray_translations(Some(locale))` normalizes (`_`→`-`, lowercase), tries exact match → Chinese-script fallback (`zh-Hant`/TW/HK/MO/`yue` → `zh-TW`, else `zh`) → language code → English (`src-tauri/src/tray_i18n.rs:26-48`; fallback matrix pinned by test `resolves_locale_fallbacks` at `:50-76`). The locale passed in is `settings.app_language` (`MenuInputs.locale` set in `compute_desired`, `src-tauri/src/tray.rs:335`).

**To add a new tray menu item** (documented in `src-tauri/src/tray_i18n.rs:10-13`): 1) add the key to `en/translation.json` under `"tray"`, 2) add translations to other locale files, 3) update `tray.rs` to use the new generated field. The Rust struct regenerates automatically at compile time; **no manual Rust struct edit is needed**, but tray.rs code referencing the field must be written (compile error otherwise).

---

## 2. Settings system end to end

### 2.1 Rust shape (`src-tauri/src/settings.rs`)

`AppSettings` (`src-tauri/src/settings.rs:374-533`) is one flat struct with ~60 fields, `#[derive(Serialize, Deserialize, Debug, Clone, Type)]` (specta `Type` drives the TS bindings) and **container-level `#[serde(default)]`** so every missing field falls back to its `get_default_settings()` value — a partial store can never fail the whole load (#1619) (`src-tauri/src/settings.rs:368-376`). Field-level `#[serde(default = "default_...")]` functions sit next to each field. Notable fields for this area: `selected_model`, `app_language`, `update_checks_enabled`, `show_tray_icon`, `start_hidden`, `autostart_enabled`, `theme`, `debug_mode`, `onboarding_completed`, `bindings: HashMap<String, ShortcutBinding>`, `post_process_*` family, `overlay_style`, `chinese_script`, `keyboard_implementation`.

Enums (all `#[serde(rename_all = "snake_case")]`, specta `Type`): `LogLevel` (custom deserializer accepting legacy 1-5 ints, `src-tauri/src/settings.rs:24-67`), `OverlayPosition` (retired `none` kept as serde alias → Bottom, `:110-120`), `OverlayStyle {None,Minimal,Live}` (`:122-132`), `ModelUnloadTimeout` (`:134-146`), `PasteMethod` (`:148-157`), `ShortcutActivation` (`:159-172`), `ClipboardHandling`, `ChineseScript`, `AutoSubmitKey`, `RecordingRetentionPeriod`, `KeyboardImplementation`, `SoundTheme`, `Theme {System,Light,Dark}`, `TypingTool`, `TranscribeAcceleratorSetting`, `OrtAcceleratorSetting`, `VadBackend`. Platform-conditional defaults exist (e.g. `PasteMethod`: Direct on Linux, CtrlV elsewhere — `:229-237`; `overlay_style`: None on Linux, Live elsewhere — `:587-594`; `KeyboardImplementation` — `:220-227`).

`SecretMap(HashMap<String,String>)` wraps API keys with a redacting Debug impl (`src-tauri/src/settings.rs:340-366`; pinned by tests `debug_output_redacts_api_keys`/`secret_map_debug_redacts_values` at `:1764-1791`).

**Schema versioning & migrations**: `settings_schema_version` (currently `CURRENT_SETTINGS_SCHEMA_VERSION = 2`, `src-tauri/src/settings.rs:539`) plus key-absence-driven one-time migrations in `apply_settings_migrations` (`:1123-1229`): onboarding_completed inference, whats-new version blanking, `push_to_talk`→`ShortcutActivation`, `zh-Hans/Hant`→`chinese_script`, legacy GPU ordinal→Auto, `overlay_position:"none"`→`OverlayStyle::None`. A frozen v0.9 store fixture test (`frozen_v0_9_store_parses_strictly_then_migrates_device_index`, `:1333-1455`) pins backward compatibility — the test's doc comment (`:1323-1332`) says **if a schema change breaks it, add a serde alias or one-time migration, don't just update the fixture**.

**Salvage path**: if the stored object fails strict deserialization, `salvage_settings` merges valid stored fields over defaults field-by-field, dropping only individually-invalid keys (`src-tauri/src/settings.rs:1085-1121`; tests `:1457-1550`). `ensure_post_process_defaults` self-heals the provider list/keys/models on every load (`:822-876`).

**Persistence**: `tauri_plugin_store` JSON store at `SETTINGS_STORE_PATH = "settings_store.json"` (`src-tauri/src/settings.rs:878`) resolved through `portable::store_path` (portable-mode aware, `src-tauri/src/portable.rs:109-115`); the whole `AppSettings` is stored under the single key `"settings"` (`:1044-1080`, `:1248-1254`). `get_settings` is load-or-create + migrate + persist-migrations (`:1037-1083`); `write_settings` overwrites the key whole (`:1248-1254`). Note there is **no in-memory cache and no lock**: every `get_settings` re-reads and re-deserializes the store; `write_settings` rewrites the whole object — read-modify-write cycles are the caller's responsibility (a concurrent pair of `change_*` commands could race; in practice commands run on the main/async runtime serialized per call, but the pattern is last-writer-wins).

`update_checks_forced_disabled()` (env `HANDY_DISABLE_UPDATER`, OnceLock-cached, `:1231-1238`) and `update_checks_effectively_enabled()` (`:1240-1246`) keep the forced-off state from leaking into the persisted setting.

### 2.2 Rust command layer: per-setting `change_*` commands (no bulk update)

There is **no `update_settings` / whole-object save command** exposed to the frontend. Each setting has a dedicated Tauri command, almost all in `src-tauri/src/shortcut/mod.rs` under "General Settings Commands" (comment banner `src-tauri/src/shortcut/mod.rs:573-575`), following the canonical three-line pattern:

```rust
#[tauri::command]
#[specta::specta]
pub fn change_audio_feedback_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.audio_feedback = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}
```
(`src-tauri/src/shortcut/mod.rs:598-605`.)

~45 such commands exist (full list via `grep "pub fn change_"`: `src-tauri/src/shortcut/mod.rs:163,352,579,591,600,609,618,636,682,691,700,725,751,774,793,815,839,860,891,903,912,921,930,939,973,994,1006,1026,1035,1053,1077,1086,1130,1144,1304,1313,1322,1331,1340,1365,1377,1386,1399,1421,1435,1447`). Some take typed enums (`ShortcutActivation`, `ChineseScript`, `VadBackend`, accelerators), some take strings parsed in Rust with a warn-and-default fallback (`change_sound_theme_setting` `:616-632`, `change_theme_setting` `:634-655`). Commands with side effects beyond the store:
- `change_theme_setting` → applies native window theme + emits `theme-changed` (`:649-654`).
- `change_app_language_setting` → `tray::update_tray_menu` (`:1386-1395`).
- `change_show_tray_icon_setting` → `tray::set_tray_visibility` (`:1397-1408`).
- `change_debug_mode_setting` → toggles webview log streaming etc. (`:751-772`, emits `settings-changed`).
- `change_vad_backend_setting` is `async` and returns a `Result` the frontend surfaces as a toast (`:1340-1363`; frontend handling `src/stores/settingsStore.ts:175-183`).
- accelerator changes go through `save_accelerator_and_reload_next_use` (`:1412-1417`).
- `change_keyboard_implementation_setting` emits `settings-changed` with a payload (`:401-409`).

Related non-`change_*` settings commands: `commands::get_app_settings` / `commands::get_default_settings` (`src-tauri/src/commands/mod.rs:40-50`), `commands::set_log_level` (`:61-77`), microphone/output/channel commands in `src-tauri/src/commands/audio.rs` (e.g. `set_selected_microphone` at `:218` region), `commands::history::update_history_limit` / `update_recording_retention_period` (`src-tauri/src/commands/history.rs:116,145`), `commands::models::set_active_model`, and `shortcut::update_custom_words`, post-process prompt CRUD, etc.

### 2.3 Bindings generation ( specta → `src/bindings.ts`)

Every command is registered in one `tauri_specta::Builder::collect_commands![...]` list in `src-tauri/src/lib.rs:652-771`, plus events (`:772-776`). On **debug builds only**, `specta_builder.export(Typescript..., "../src/bindings.ts")` regenerates `src/bindings.ts` (`src-tauri/src/lib.rs:778-784`). The frontend imports typed wrappers from `@/bindings` (path alias `src/bindings.ts` — `tsconfig.json` "paths", `vite.config.ts` resolve.alias). Commands return `Promise<Result<T, string>>` (`{status:"ok",data}` / `{status:"error",error}`) — see `src/bindings.ts` header and generated bodies. **Workflow implication: a new command isn't callable from TS until a debug build regenerates bindings.ts** (release builds never export).

### 2.4 Frontend state: zustand store + per-key updater map

`src/stores/settingsStore.ts` (zustand + `subscribeWithSelector`):
- Holds `settings: AppSettings | null`, `defaultSettings` (for per-setting reset), `isUpdating: Record<string, boolean>`, audio device lists, `postProcessModelOptions`, `updateChecksLocked` (`:16-27`).
- **`settingUpdaters` map** (`:83-200`) maps each `AppSettings` snake_case key to the command call, e.g. `show_tray_icon: (value) => commands.changeShowTrayIconSetting(value as boolean)` (`:188-189`), `app_language: (value) => commands.changeAppLanguageSetting(value as string)` (`:167`). This map is the **single registry a new setting must be added to** on the frontend; `updateSetting` optimistically sets the local value, invokes the updater, and rolls back on error (`:316-345`). Keys with no updater log `No handler for setting: ...` (`:334-335`).
- `initialize()` loads defaults + settings + custom-sound check + update-lock, and subscribes to backend events `model-state-changed` and `settings-changed` → `refreshSettings()` (re-fetch via `commands.getAppSettings`; microphone change also refreshes devices) (`:632-663`). Backend→frontend pushes therefore exist only for these two events.
- `refreshSettings` normalizes nullable mic fields to `"Default"` (`:231-253`).

`src/hooks/useSettings.ts` is a thin React wrapper over the store: returns `settings`, `getSetting(key)`, `updateSetting(key, value)`, `resetSetting(key)`, `isUpdating(key)`, binding actions, post-process helpers (`src/hooks/useSettings.ts:47-80`); calls `store.initialize()` on first mount (`:51-55`).

### 2.5 Frontend page structure

- `src/App.tsx` renders `<Sidebar activeSection onSectionChange/>` + section content + `<Footer/>` (`src/App.tsx:353-381`); default section `"general"` (`:63`).
- `src/components/Sidebar.tsx` exports `SECTIONS_CONFIG` — the seven sections with `labelKey` (i18n), icon, component, and `enabled(settings)` predicate (`src/components/Sidebar.tsx:34-77`): `general` (GeneralSettings, always), `history`, `models`, `advanced`, `postprocessing` (enabled iff `post_process_enabled`), `debug` (enabled iff `debug_mode`), `about`. Sections are filtered by `enabled(settings)` at render (`:91-93`).
- Page components: `src/components/settings/general/GeneralSettings.tsx` (shortcuts group + `ModelSettingsCard` + sound/mic group, `:20-42`), `advanced/AdvancedSettings.tsx` (groups: app, output, transcription, history, plus an `experimental_enabled`-gated group, `:33-79`), `history/HistorySettings.tsx`, `models/ModelsSettings.tsx`, `post-processing/PostProcessingSettings.tsx`, `debug/DebugSettings.tsx`, `about/AboutSettings.tsx`. All exported from the barrel `src/components/settings/index.ts`.
- **Each setting is its own component** in `src/components/settings/` (~48 files, e.g. `ShowTrayIcon.tsx`, `StartHidden.tsx`, `AutostartToggle.tsx`, `ThemeSelector.tsx`, `AppLanguageSelector.tsx`), composed into the pages above. Pattern: `React.memo`, `useTranslation()` for strings, `useSettings()` for value+update, props `{descriptionMode?: "inline"|"tooltip", grouped?: boolean}` (see `src/components/settings/ShowTrayIcon.tsx:6-31`).
- UI primitives in `src/components/ui/`: `SettingsGroup.tsx` (titled card with `divide-y` rows, `src/components/ui/SettingsGroup.tsx:9-31`), `SettingContainer.tsx` (label + info-tooltip/inline description + control slot, horizontal or stacked, `src/components/ui/SettingContainer.tsx:15-194`), `ToggleSwitch`, `Dropdown`, `Slider`, `Select`, `Input`, `Dialog`, `Tooltip`, `ResetButton`.

### 2.6 Worked example: full travel of one setting

**`show_tray_icon`** (toggle in Advanced → group "app"):
1. UI: `AdvancedSettings.tsx:38` renders `<ShowTrayIcon descriptionMode="tooltip" grouped/>`; `ShowTrayIcon.tsx:16-21` reads `getSetting("show_tray_icon") ?? true` and calls `updateSetting("show_tray_icon", enabled)`; label/description from `t("settings.advanced.showTrayIcon.label"/".description")` (`ShowTrayIcon.tsx:23-24`).
2. Store: `settingsStore.updateSetting` optimistic set → `settingUpdaters.show_tray_icon` → `commands.changeShowTrayIconSetting(enabled)` (`src/stores/settingsStore.ts:188-189, 316-345`) → generated binding invoking Tauri command `change_show_tray_icon_setting` (`src/bindings.ts`).
3. Rust: `shortcut::change_show_tray_icon_setting` (`src-tauri/src/shortcut/mod.rs:1397-1408`) does get→mutate→`write_settings` (persists whole `AppSettings` to `settings_store.json` under key `"settings"`), then `tray::set_tray_visibility(&app, enabled)` applies immediately.
4. Persisted: `tauri_plugin_store` file `settings_store.json` (portable-aware path, `src-tauri/src/settings.rs:878,1037-1041`).
5. Next launch: `initialize_core_logic` reads settings and hides the tray if false (`src-tauri/src/lib.rs:349-353`).

**`app_language`** (dropdown): `AppLanguageSelector.tsx:30-33` calls `i18n.changeLanguage` (instant UI) + `updateSetting("app_language", code)`; Rust `change_app_language_setting` persists + `tray::update_tray_menu` re-renders the tray in the new locale (`src-tauri/src/shortcut/mod.rs:1386-1395`) — the menu rebuild picks up `MenuInputs.locale` → `get_tray_translations` (`src-tauri/src/tray.rs:335,459`).

**To add a brand-new setting, the touch points are:**
1. `settings.rs`: field on `AppSettings` + `#[serde(default = "...")]` fn + entry in `get_default_settings()` (`:932-996`) (+ enum if needed).
2. A `change_<field>_setting` command (canonical pattern `src-tauri/src/shortcut/mod.rs:598-605`), registered in `collect_commands!` (`src-tauri/src/lib.rs:653-771`).
3. Debug build to regenerate `src/bindings.ts` (`src-tauri/src/lib.rs:778-784`).
4. `settingUpdaters` entry in `src/stores/settingsStore.ts:83-200`.
5. A per-setting component in `src/components/settings/`, composed into a page (`GeneralSettings.tsx` / `AdvancedSettings.tsx`).
6. i18n keys in `en/translation.json` (then other locales): convention `settings.<page>.<settingName>.{label,description}` (e.g. `settings.advanced.showTrayIcon.label` — verified shape `src/i18n/locales/en/translation.json` `settings.advanced.showTrayIcon = {label, description}`).
7. If it should affect the tray: add a field to `MenuInputs` + `compute_desired` (`src-tauri/src/tray.rs:54-66,312-339`) and call `update_tray_menu`/`refresh_tray_icon` from the command.

---

## 3. i18n conventions

**Frontend (react-i18next)**:
- Setup in `src/i18n/index.ts`: i18next + initReactI18next; locales auto-discovered via `import.meta.glob("./locales/*/translation.json")` (`:12-25`); init with `lng:"en"`, `fallbackLng:"en"`, `useSuspense:false` (`:90-100`). `SUPPORTED_LANGUAGES` is built from discovered dirs + `LANGUAGE_METADATA` (name/nativeName/priority/direction, `src/i18n/languages.ts:17-49`; RTL for ar/he).
- Language selection: `syncLanguageFromSettings` reads `app_language` from Rust settings at startup, falls back to OS locale (`src/i18n/index.ts:103-125`); locale normalization incl. `zh-Hant/TW/HK/MO` → `zh-TW`, `yue` handling (`getSupportedLanguage`, `:55-86`). `languageChanged` updates `dir`/`lang` on the document (`:128-132`).
- **English is the source of truth**: key tree top level is `tray, sidebar, onboarding, modelSelector, settings, footer, whatsNew, common, accessibility, errors, appLanguage, theme, overlay, secureInput` (verified this session). Settings pages use `settings.<page>…` and `sidebar.<section>`; components use `useTranslation()`'s `t()` with `{{variable}}` interpolation (e.g. `settings.general.shortcut.errors.set`: "Failed to set shortcut: {{error}}").
- **Key parity is enforced**: `bun run check:translations` (`scripts/check-translations.ts`) diffs every locale's key paths against `en` (missing/extra); it runs in CI (`scripts` in `package.json`; `.github/workflows/code-quality.yml:50`). **Ran it this session: "✓ All 25 languages have complete translations!"** (i.e. all 25 non-en locales match en's key set).
- Human conventions documented in `CONTRIBUTING_TRANSLATIONS.md`: copy en file, translate values only, keep keys/`{{variables}}`, register in `languages.ts`, don't translate brand names (Handy, transcribe.cpp, OpenAI).

**Tray (Rust side)**: shares the *same* `translation.json` files via build.rs codegen (§1.4). Adding a tray string = add key under `"tray"` in en + translations; the `TrayStrings` field is generated; blank in a locale falls back to English at render time for the warning item (`src-tauri/src/tray.rs:464-470`) and to `""` for others, so **new tray keys should be added to en first and ideally to all locales in the same change** (the check:translations script enforces presence of the key in every locale, since it checks all top-level keys incl. `tray`).

**Native status strings not localized elsewhere**: the version label `Handy v{...}` and model names are not translated (product/model names); menu item labels all come from the tray i18n table.

---

## 4. Seams (extension points our features should build on)

- `MenuInputs` + `compute_desired` (`src-tauri/src/tray.rs:54-66, 312-339`): add any new menu-affecting input here and the diff/coalesce machinery picks it up automatically; trigger with `update_tray_menu(&app)`.
- `build_menu` (`src-tauri/src/tray.rs:458-595`): single place menu items are declared; item ids are matched in one `on_menu_event` match in `src-tauri/src/lib.rs:285-341`.
- `get_tray_translations` + build.rs codegen (`src-tauri/src/tray_i18n.rs:26-48`, `src-tauri/build.rs:284-361`): adding a `"tray"` key to en/translation.json auto-generates the Rust struct field at the next cargo build.
- `settingUpdaters` map (`src/stores/settingsStore.ts:83-200`): the one frontend registry wiring setting key → Rust command.
- Per-setting component + `SettingsGroup`/`SettingContainer` composition (`src/components/settings/*.tsx`, `src/components/ui/SettingsGroup.tsx`, `SettingContainer.tsx`) and `SECTIONS_CONFIG` (`src/components/Sidebar.tsx:34-77`) for new pages/sections.
- `AppSettings` serde-default + migration pattern (`src-tauri/src/settings.rs:374-533, 1123-1229`): new fields need a default fn; renames/semantic changes need a one-time migration (see frozen-fixture test guidance at `:1323-1332`).
- Backend→frontend push: emit `settings-changed` (payload `{setting: string, ...}`) — `settingsStore.initialize` already refreshes on it (`src/stores/settingsStore.ts:657-662`); `model-state-changed` similarly (`:654-656`).
- Tooltip as lightweight status surface: `tray.set_tooltip` is already wired into the applier path (`src-tauri/src/tray.rs:404-406`), and `tray_tooltip()` (`:442-444`) is the initial tooltip — a status-text feature could extend `build_menu`'s tooltip return without new plumbing.

## 5. Risks / unknowns

- **No bulk settings save & no store-level locking**: every command does read-modify-write of the whole `AppSettings` (`get_settings` → mutate → `write_settings`). Two concurrent commands can lose one update (last-writer-wins). Commands currently run effectively serialized per invoke, but a new feature issuing parallel setting updates must sequence them. Also `get_settings` re-reads/re-parses the JSON store on *every* call (hot paths like tray sync call it — `compute_desired` at `src-tauri/src/tray.rs:313`).
- **Settings reads persist migrations**: `get_settings` writes the store when migrations/defaults fire (`src-tauri/src/settings.rs:1042-1080`) — a "read" is not side-effect-free.
- **bindings.ts regeneration is debug-build-only** (`src-tauri/src/lib.rs:778-784`): forgetting to run a debug build before frontend work against a new command yields TS compile errors against a stale `src/bindings.ts`.
- **Tray menu parity risk**: `build.rs` emits `""` for locales missing a tray key; only the Secure-Input warning item has an explicit English fallback in `tray.rs` (`:464-470`). A new tray item that forgets the fallback renders a blank label in untranslated locales until `check:translations` forces the key everywhere (CI gates it, so in practice the key must be added to all 26 locale files in the same change).
- **`update_tray_menu` name is misleading** — it re-syncs icon *and* menu (`src-tauri/src/tray.rs:230-232`); there is no menu-only entry point.
- **Menu item enable/disable semantics**: `check_updates` enabled flag vs forced-removal (`src-tauri/src/tray.rs:575-583`) — new conditional items must decide greyed-out vs removed; a disabled item still shifts positions (comment at `:576-580`).
- **Windows taskbar theme special case** (`src-tauri/src/tray.rs:142-184`) and Linux fixed Colored theme mean icon variants must exist for all three themes when adding states (see `resources/tray_*.png` naming in `get_icon_path`, `:191-215`).
- **MenuInputs equality gates rebuilds**: a new input that changes display must be added to `MenuInputs` (PartialEq) — mutating e.g. a model's display name inside `downloaded_models` tuples is already covered, but anything derived outside the struct won't trigger a rebuild.
- Not verified here: actual runtime behavior of the tray on each OS (no builds/tests were run — research only), and the overlay window UI (`src/overlay/`, `src/components/settings/ShowOverlay.tsx` uses `overlay_style`) beyond its settings surface.
- The `check:translations` run above is the only check executed in this session; no cargo tests, frontend typecheck, or builds were run.
