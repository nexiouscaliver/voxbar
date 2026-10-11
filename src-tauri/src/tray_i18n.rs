//! Tray menu internationalization
//!
//! Everything is auto-generated at compile time by build.rs from the
//! frontend locale files (src/i18n/locales/*/translation.json).
//!
//! The English translation.json is the single source of truth:
//! - TrayStrings struct fields are derived from the English "tray" keys
//! - All languages are auto-discovered from the locales directory
//!
//! To add a new tray menu item:
//! 1. Add the key to en/translation.json under "tray"
//! 2. Add translations to other locale files
//! 3. Update tray.rs to use the new field (e.g., strings.new_field)

use crate::chinese_script::chinese_script_for_locale;
use crate::settings::ChineseScript;
use once_cell::sync::Lazy;
use std::collections::HashMap;

// Include the auto-generated TrayStrings struct and TRANSLATIONS static
include!(concat!(env!("OUT_DIR"), "/tray_translations.rs"));

/// Get localized tray menu strings based on the system locale.
///
/// Lookup order: exact locale → Chinese script/region fallback → language code → English.
pub fn get_tray_translations(locale: Option<String>) -> TrayStrings {
    let normalized = locale
        .as_deref()
        .unwrap_or("en")
        .to_lowercase()
        .replace('_', "-");
    let language = normalized.split('-').next().unwrap_or("en");

    let exact_match = TRANSLATIONS
        .iter()
        .find_map(|(code, strings)| code.eq_ignore_ascii_case(&normalized).then_some(strings));
    let fallback = match chinese_script_for_locale(&normalized) {
        Some(ChineseScript::Traditional) => "zh-TW",
        Some(_) => "zh",
        None => language,
    };

    exact_match
        .or_else(|| TRANSLATIONS.get(fallback))
        .or_else(|| TRANSLATIONS.get("en"))
        .cloned()
        .expect("English translations must exist")
}

#[cfg(test)]
mod tests {
    use super::{get_tray_translations, TrayStrings, TRANSLATIONS};

    #[test]
    fn resolves_locale_fallbacks() {
        for (locale, expected) in [
            ("zh-Hant-TW", "zh-TW"),
            ("zh-Hant-HK", "zh-TW"),
            ("zh-HK", "zh-TW"),
            ("zh-MO", "zh-TW"),
            ("ZH-TW", "zh-TW"),
            ("zh_Hant_TW", "zh-TW"),
            ("zh-Hans-CN", "zh"),
            ("yue-Hant-HK", "zh-TW"),
            ("yue-Hans-CN", "zh"),
            ("fr-FR", "fr"),
            ("xx-YY", "en"),
        ] {
            assert_eq!(
                format!("{:?}", get_tray_translations(Some(locale.into()))),
                format!("{:?}", TRANSLATIONS[expected]),
                "{locale} should resolve to {expected}"
            );
        }
    }

    /// The generated-translations contract for the W1-L8 tray sections
    /// (status / models / recent dictations / companion blocks): build.rs
    /// emits an empty String for a key a locale file omits, and a blank
    /// menu line would otherwise ship silently. Every generated locale
    /// must carry a non-empty value for each new key.
    #[test]
    fn every_locale_translates_the_new_tray_sections() {
        let keys_of = |s: &TrayStrings| {
            vec![
                ("statusIdle", s.status_idle.clone()),
                ("statusRecording", s.status_recording.clone()),
                ("statusTranscribing", s.status_transcribing.clone()),
                ("asrModel", s.asr_model.clone()),
                ("modelsNone", s.models_none.clone()),
                ("recentDictations", s.recent_dictations.clone()),
                ("recentDictationsEmpty", s.recent_dictations_empty.clone()),
                ("companionLabel", s.companion_label.clone()),
                ("companionOn", s.companion_on.clone()),
                ("companionOff", s.companion_off.clone()),
                ("companionPairedDevices", s.companion_paired_devices.clone()),
                ("companionTarget", s.companion_target.clone()),
                ("companionTargetMac", s.companion_target_mac.clone()),
                ("companionTargetPhone", s.companion_target_phone.clone()),
            ]
        };
        for (code, strings) in TRANSLATIONS.iter() {
            for (key, value) in keys_of(strings) {
                assert!(
                    !value.is_empty(),
                    "{code} tray.{key} is empty in the generated translations"
                );
            }
        }
    }
}
