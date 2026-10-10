//! The embedded single-file web client served at GET /. The HTML/JS/CSS
//! asset is committed under `client/index.html` and embedded with
//! include_str! (no app-store install, no bundler); its UI strings are the
//! SAME `companion.*` keys the app locales ship, injected as a JSON blob by
//! the embed generator (scripts/embed-companion-strings.ts) so the phone
//! UI is localized too.

/// The injection marker. The raw file carries `MARKER null` (a JS comment
/// plus a null literal, so the page stays viewable unprocessed); rendering
/// swaps the `null` for the generated locale blob.
const MARKER: &str = "/*__COMPANION_STRINGS__*/";

pub const CLIENT_HTML: &str = include_str!("client/index.html");

/// Render the page with the localized strings inlined. Whitespace between
/// the marker and the `null` it replaces is tolerated, so a reformatted
/// asset still injects.
pub fn render_client_page() -> String {
    let Some(marker_at) = CLIENT_HTML.find(MARKER) else {
        return CLIENT_HTML.to_string();
    };
    let after_marker = marker_at + MARKER.len();
    let tail = &CLIENT_HTML[after_marker..];
    let Some(null_end) = tail.find("null") else {
        return CLIENT_HTML.to_string();
    };
    // Only a comment-to-literal gap (whitespace) may sit between them;
    // anything else means the asset drifted and must not be mangled.
    if !tail[..null_end].trim().is_empty() {
        return CLIENT_HTML.to_string();
    }
    let mut page = String::with_capacity(CLIENT_HTML.len() + super::strings_gen::ALL.len());
    page.push_str(&CLIENT_HTML[..after_marker]);
    page.push_str(super::strings_gen::ALL);
    page.push_str(&CLIENT_HTML[after_marker + null_end + "null".len()..]);
    page
}

#[cfg(test)]
mod tests {
    use super::{render_client_page, CLIENT_HTML, MARKER};

    #[test]
    fn rendered_page_carries_the_locale_strings() {
        let page = render_client_page();
        assert!(
            page.contains(&format!("{MARKER}{{")),
            "the null placeholder must be replaced by the locale blob"
        );
        assert!(page.contains("\"en\""));
        // Every locale the generator covers ships its strings.
        for lang in ["de", "es", "fr", "ja", "zh", "pt", "ru", "ar", "hi", "he"] {
            assert!(
                page.contains(&format!("\"{lang}\":")),
                "missing {lang} in the embedded strings"
            );
        }
        // The page stays valid JS: no stray null literal after injection.
        let marker_at = page.find(MARKER).expect("marker");
        let after = &page[marker_at + MARKER.len()..];
        assert!(after.starts_with('{'));
    }

    #[test]
    fn injection_tolerates_whitespace_after_the_marker() {
        // A prettier-formatted asset renders `MARKER null` (space before the
        // literal); the injection must still swap the null.
        let html = "const STRINGS = /*__COMPANION_STRINGS__*/ null;\n";
        let marker_at = html.find(MARKER).unwrap();
        assert_eq!(&html[marker_at + MARKER.len()..], " null;\n");
        // The production renderer works on the committed asset; this test
        // pins the tolerance rule by construction over the same logic.
        let rendered = render_client_page();
        assert!(rendered.contains(MARKER));
        assert!(!rendered.contains(&format!("{MARKER}null")));
        assert!(!rendered.contains(&format!("{MARKER} null")));
    }

    /// Regression test for the phone stop-tap no-op: the button's tap
    /// handler must send a PRESS edge, never a release, as its stop action.
    /// In toggle mode the Mac ignores release edges by design (accidental
    /// key-ups must not stop a toggle dictation), so a "Tap to stop" that
    /// sends pressed=false can never stop a phone-started toggle session -
    /// only a press toggles it off. The handler may still send releases
    /// elsewhere (pressEnd for the hold modes, backgrounding); this pins
    /// the tap path.
    #[test]
    fn tap_handler_stops_with_a_press_edge_not_a_release() {
        let press_start = CLIENT_HTML
            .find("function pressStart")
            .expect("pressStart handler in the committed asset");
        let press_end = CLIENT_HTML
            .find("function pressEnd")
            .expect("pressEnd handler in the committed asset");
        let body = &CLIENT_HTML[press_start..press_end];
        assert!(
            body.contains("sendPtt(true);"),
            "the tap path must send a press edge (sendPtt(true))"
        );
        assert!(
            !body.contains("sendPtt(false)"),
            "the tap path must not send a release edge: toggle sessions ignore releases, so a release stop-tap is a silent no-op"
        );
    }
}
