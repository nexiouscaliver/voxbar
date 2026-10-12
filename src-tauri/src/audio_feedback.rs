use crate::settings::{self, SoundTheme};
use cpal::traits::{DeviceTrait, HostTrait};
use log::{debug, error, warn};
use rodio::OutputStreamBuilder;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::thread;
use tauri::{AppHandle, Manager};

/// Debug is for the KB-080 fallback log line (which cue fell back); Copy
/// because the resolver consults it twice (configured + fallback resolve).
#[derive(Clone, Copy, Debug)]
pub enum SoundType {
    Start,
    Stop,
    Error,
}

fn resolve_sound_path(
    app: &AppHandle,
    theme: SoundTheme,
    sound_type: SoundType,
) -> Option<PathBuf> {
    let sound_file = get_sound_path(theme, sound_type);
    let base_dir = get_sound_base_dir(theme);
    match base_dir {
        tauri::path::BaseDirectory::AppData => {
            crate::portable::resolve_app_data(app, &sound_file).ok()
        }
        _ => app.path().resolve(&sound_file, base_dir).ok(),
    }
}

fn get_sound_path(theme: SoundTheme, sound_type: SoundType) -> String {
    match (theme, sound_type) {
        (SoundTheme::Custom, SoundType::Start) => "custom_start.wav".to_string(),
        (SoundTheme::Custom, SoundType::Stop) => "custom_stop.wav".to_string(),
        (SoundTheme::Custom, SoundType::Error) => "custom_error.wav".to_string(),
        (_, SoundType::Start) => theme.to_start_path(),
        (_, SoundType::Stop) => theme.to_stop_path(),
        (_, SoundType::Error) => theme.to_error_path(),
    }
}

fn get_sound_base_dir(theme: SoundTheme) -> tauri::path::BaseDirectory {
    match theme {
        SoundTheme::Custom => tauri::path::BaseDirectory::AppData,
        _ => tauri::path::BaseDirectory::Resource,
    }
}

/// The default theme's cue that stands in when the configured asset is
/// missing. The bundled Marimba WAVs ship inside the app, so the fallback
/// is audible in every real install (a custom theme with unrecorded or
/// deleted WAVs is the gap this covers; a broken resource bundle has
/// nothing to fall back to and the resolver returns None).
const MISSING_ASSET_FALLBACK_THEME: SoundTheme = SoundTheme::Marimba;

/// KB-080: the pure existence decision. Given the configured cue path and
/// the default theme's same-cue fallback, return the path that should
/// actually play: the configured asset when it exists, else the fallback
/// when THAT exists, else nothing. Extracted so the decision is unit-
/// testable against real files without an app handle.
fn cue_to_play<'a>(configured: &'a Path, fallback: Option<&'a Path>) -> Option<&'a Path> {
    if configured.exists() {
        return Some(configured);
    }
    fallback.filter(|path| path.exists())
}

/// KB-080: resolve the cue that should actually play. A custom theme whose
/// `custom_start.wav` / `custom_stop.wav` is missing used to hand the dead
/// path straight to playback, where `File::open` failed and the cue played
/// NOTHING (the error only reached the log). This mirrors the error path's
/// fallback contract (`play_error_feedback` already falls back to the Stop
/// cue when `*_error.wav` is absent): the configured asset when present,
/// otherwise the default theme's same cue.
fn resolve_existing_sound_path(
    app: &AppHandle,
    theme: SoundTheme,
    sound_type: SoundType,
) -> Option<PathBuf> {
    let configured = resolve_sound_path(app, theme, sound_type)?;
    let fallback = resolve_sound_path(app, MISSING_ASSET_FALLBACK_THEME, sound_type);
    let chosen = cue_to_play(&configured, fallback.as_deref())?;
    if chosen != configured {
        warn!(
            "Sound asset '{}' is missing; falling back to the default theme's {:?} cue at {}",
            configured.display(),
            sound_type,
            chosen.display()
        );
    }
    Some(chosen.to_path_buf())
}

pub fn play_feedback_sound(app: &AppHandle, sound_type: SoundType) {
    let settings = settings::get_settings(app);
    if !settings.audio_feedback {
        return;
    }
    if let Some(path) = resolve_existing_sound_path(app, settings.sound_theme, sound_type) {
        play_sound_async(app, path);
    }
}

pub fn play_feedback_sound_blocking(app: &AppHandle, sound_type: SoundType) {
    let settings = settings::get_settings(app);
    if !settings.audio_feedback {
        return;
    }
    if let Some(path) = resolve_existing_sound_path(app, settings.sound_theme, sound_type) {
        play_sound_blocking(app, &path);
    }
}

pub fn play_test_sound(app: &AppHandle, sound_type: SoundType) {
    let settings = settings::get_settings(app);
    if let Some(path) = resolve_existing_sound_path(app, settings.sound_theme, sound_type) {
        play_sound_blocking(app, &path);
    }
}

/// How much quieter the Stop cue plays when it stands in for a missing Error
/// asset: recognizably the same theme's voice, clearly not a normal stop.
const ERROR_FALLBACK_VOLUME_FACTOR: f32 = 0.6;

/// The error cue for the notice channel. Prefers the theme's dedicated
/// `*_error.wav`; when that asset does not exist (a custom theme with only
/// start/stop recorded), falls back to the Stop cue at reduced volume so a
/// failure is always audible and never reads as a normal stop.
pub fn play_error_feedback(app: &AppHandle) {
    let settings = settings::get_settings(app);
    if !settings.audio_feedback {
        return;
    }
    // The error cue keeps its own hand-rolled existence check and reduced-
    // volume Stop fallback (a missing *_error.wav must not read as a normal
    // stop), so it resolves the raw paths rather than the start/stop
    // resolver above.
    if let Some(path) = resolve_sound_path(app, settings.sound_theme, SoundType::Error) {
        if path.exists() {
            play_sound_async_at_volume(app, path, 1.0);
            return;
        }
    }
    if let Some(path) = resolve_sound_path(app, settings.sound_theme, SoundType::Stop) {
        play_sound_async_at_volume(app, path, ERROR_FALLBACK_VOLUME_FACTOR);
    }
}

fn play_sound_async(app: &AppHandle, path: PathBuf) {
    let app_handle = app.clone();
    thread::spawn(move || {
        if let Err(e) = play_sound_at_path(&app_handle, path.as_path(), None) {
            error!("Failed to play sound '{}': {}", path.display(), e);
        }
    });
}

/// Async playback with a volume factor applied on top of the user's
/// `audio_feedback_volume` (the error fallback plays the Stop cue quieter).
fn play_sound_async_at_volume(app: &AppHandle, path: PathBuf, factor: f32) {
    let app_handle = app.clone();
    thread::spawn(move || {
        if let Err(e) = play_sound_at_path(&app_handle, path.as_path(), Some(factor)) {
            error!("Failed to play sound '{}': {}", path.display(), e);
        }
    });
}

fn play_sound_blocking(app: &AppHandle, path: &Path) {
    if let Err(e) = play_sound_at_path(app, path, None) {
        error!("Failed to play sound '{}': {}", path.display(), e);
    }
}

fn play_sound_at_path(
    app: &AppHandle,
    path: &Path,
    volume_factor: Option<f32>,
) -> Result<(), Box<dyn std::error::Error>> {
    let settings = settings::get_settings(app);
    let volume = match volume_factor {
        Some(factor) => settings.audio_feedback_volume * factor,
        None => settings.audio_feedback_volume,
    };
    let selected_device = settings.selected_output_device.clone();
    play_audio_file(path, selected_device, volume)
}

fn play_audio_file(
    path: &std::path::Path,
    selected_device: Option<String>,
    volume: f32,
) -> Result<(), Box<dyn std::error::Error>> {
    let stream_builder = if let Some(device_name) = selected_device {
        if device_name == "Default" {
            debug!("Using default device");
            OutputStreamBuilder::from_default_device()?
        } else {
            let host = crate::audio_toolkit::get_cpal_host();
            let devices = host.output_devices()?;

            let mut found_device = None;
            for device in devices {
                if device.name()? == device_name {
                    found_device = Some(device);
                    break;
                }
            }

            match found_device {
                Some(device) => OutputStreamBuilder::from_device(device)?,
                None => {
                    warn!("Device '{}' not found, using default device", device_name);
                    OutputStreamBuilder::from_default_device()?
                }
            }
        }
    } else {
        debug!("Using default device");
        OutputStreamBuilder::from_default_device()?
    };

    let stream_handle = stream_builder.open_stream()?;
    let mixer = stream_handle.mixer();

    let file = File::open(path)?;
    let buf_reader = BufReader::new(file);

    let sink = rodio::play(mixer, buf_reader)?;
    sink.set_volume(volume);
    sink.sleep_until_end();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::cue_to_play;
    use std::path::PathBuf;

    /// A real directory under the system temp dir, unique per test case, so
    /// the existence decision runs against real files (the behavior under
    /// test IS `Path::exists`) and sibling tests cannot see each other's
    /// fixtures.
    fn temp_file(case: &str, name: &str, create: bool) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("voxbar-kb080-{}-{case}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join(name);
        if create {
            std::fs::write(&path, b"stub").expect("write stub wav");
        }
        path
    }

    /// KB-080, the exact user scenario: the configured (custom) cue does
    /// not exist and the default theme's cue does - the playable answer is
    /// the fallback, never the dead path.
    #[test]
    fn kb080_missing_configured_cue_falls_back_to_the_default_theme_cue() {
        let configured = temp_file("fallback", "custom_stop.wav", false);
        let fallback = temp_file("fallback", "marimba_stop.wav", true);
        assert_eq!(
            cue_to_play(&configured, Some(&fallback)),
            Some(fallback.as_path())
        );
    }

    /// The recorded cue wins when it exists (the custom theme plays its own
    /// voice, not the default's).
    #[test]
    fn kb080_present_configured_cue_plays_itself() {
        let configured = temp_file("present", "custom_start.wav", true);
        let fallback = temp_file("present", "marimba_start.wav", true);
        assert_eq!(
            cue_to_play(&configured, Some(&fallback)),
            Some(configured.as_path())
        );
    }

    /// Both missing (broken install with no bundled resources): nothing
    /// plays, and the resolver says so instead of handing playback a path
    /// whose open is doomed.
    #[test]
    fn kb080_both_missing_plays_nothing() {
        let configured = temp_file("both-missing", "custom_stop.wav", false);
        let fallback = temp_file("both-missing", "marimba_stop.wav", false);
        assert_eq!(cue_to_play(&configured, Some(&fallback)), None);
        assert_eq!(cue_to_play(&configured, None), None);
    }

    /// KB-080 wiring pin: all three player entry points (the async cue, the
    /// blocking cue, and the settings preview) must route through the
    /// existence-checking resolver. The players need a live app handle a
    /// unit test cannot mount, so the calls are pinned against this
    /// committed source - the same convention as the overlay and
    /// persistent-notice wiring pins.
    #[test]
    fn kb080_every_player_routes_through_the_existence_check() {
        let source = include_str!("audio_feedback.rs");
        for fn_name in [
            "pub fn play_feedback_sound(",
            "pub fn play_feedback_sound_blocking(",
            "pub fn play_test_sound(",
        ] {
            let at = source
                .find(fn_name)
                .unwrap_or_else(|| panic!("{fn_name} must exist in audio_feedback.rs"));
            // Scope to the fn body: cut at the next top-level item marker.
            let rest = &source[at..];
            let end = rest.find("\n}\n").map(|i| i + 3).unwrap_or(rest.len());
            let body = &rest[..end];
            assert!(
                body.contains("resolve_existing_sound_path(app, settings.sound_theme, sound_type)"),
                "{fn_name} must resolve its cue through the existence-checking resolver"
            );
        }
    }
}
