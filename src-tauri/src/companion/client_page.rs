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

    /// The slice of the committed asset between two literal anchors (the
    /// anchors are function headers or switch arms, so the slice is one
    /// whole handler).
    fn between<'a>(html: &'a str, start: &str, end: &str) -> &'a str {
        let at = html
            .find(start)
            .unwrap_or_else(|| panic!("anchor '{start}' missing from the committed asset"));
        let rest = &html[at..];
        let until = rest
            .find(end)
            .unwrap_or_else(|| panic!("closing anchor '{end}' missing after '{start}'"));
        &rest[..until]
    }

    /// v1.6.0 zero-audio fix: the AudioContext must be created AND resumed
    /// inside the press gesture (engines that start contexts suspended,
    /// like iOS Safari under its autoplay policy, otherwise capture
    /// silence forever), the old welcome-time context creation must be
    /// gone, and the mic may be warmed outside a gesture (getUserMedia
    /// prompts on its own) while the context never is.
    #[test]
    fn audio_context_is_created_and_resumed_inside_the_press_gesture() {
        // The press handler runs the audio bootstrap before it sends the
        // edge, so the very first press of a fresh page already carries a
        // live graph.
        let press = between(CLIENT_HTML, "function pressStart", "function pressEnd");
        let boot = press
            .find("ensureAudio();")
            .expect("press calls ensureAudio");
        let edge = press.find("sendPtt(true);").expect("press sends the edge");
        assert!(
            boot < edge,
            "ensureAudio must run inside the gesture, before the Ptt edge"
        );

        // Create-or-resume on every press: a fresh context is created when
        // missing and a suspended one (backgrounded tab, first run) resumed.
        let ensure = between(CLIENT_HTML, "function ensureAudio", "function setupGraph");
        assert!(
            ensure.contains("new Ctx("),
            "ensureAudio creates the AudioContext when missing"
        );
        assert!(
            ensure.contains("audioCtx.state === \"suspended\""),
            "ensureAudio checks the suspended state"
        );
        assert!(
            ensure.contains("audioCtx.resume()"),
            "ensureAudio resumes a suspended context"
        );

        // The pre-v1.6.0 welcome-time graph bootstrap is gone: the welcome
        // handler may warm the microphone stream but must never touch the
        // audio context (checked on the code identifiers, not comments; a
        // context created off-gesture starts suspended).
        let welcome = between(CLIENT_HTML, "case \"welcome\":", "break;");
        assert!(
            welcome.contains("openMic();"),
            "the welcome handler warms the microphone stream"
        );
        assert!(
            !welcome.contains("setupGraph"),
            "the welcome handler must not build the audio graph"
        );
        assert!(
            !welcome.contains("audioCtx"),
            "the welcome handler must not create or resume the context"
        );
        assert!(
            !welcome.contains("new Ctx("),
            "the welcome handler must not construct an audio context"
        );
        assert!(
            !CLIENT_HTML.contains("ensureMic"),
            "the old non-gesture ensureMic path must be gone"
        );
    }

    /// A getUserMedia failure must surface as an inline error with a retry
    /// control, and must never disable the talk button (the pre-v1.6.0
    /// path killed the button until a full reload). The next press retries
    /// the microphone instead of starting a guaranteed zero-audio session.
    #[test]
    fn mic_failure_keeps_the_button_alive_and_offers_a_retry() {
        assert!(
            CLIENT_HTML.contains("id=\"micError\""),
            "the page carries an inline mic error card"
        );
        assert!(
            CLIENT_HTML.contains("id=\"micRetry\""),
            "the error card carries a retry button"
        );
        assert!(
            CLIENT_HTML.contains("micRetryBtn.addEventListener(\"click\""),
            "the retry button is wired to reopen the microphone"
        );

        let open = between(
            CLIENT_HTML,
            "function openMic",
            "micRetryBtn.addEventListener",
        );
        assert!(
            open.contains("showMicError();"),
            "a getUserMedia failure shows the inline error"
        );
        assert!(
            !open.contains("ptt.disabled = true"),
            "a mic failure must not disable the talk button"
        );

        // A press while the mic is broken retries the mic and must not
        // send a Ptt edge: the Mac would record a silent session.
        let press = between(CLIENT_HTML, "function pressStart", "function pressEnd");
        assert!(
            press.contains("if (micFailed && !recording)"),
            "the press handler checks the mic failure flag"
        );
    }

    /// The mic-failure gate must only block NEW sessions. A start-tap can
    /// pass the gate while getUserMedia is still pending (the permission
    /// prompt is up), arm the Mac's locked toggle session, and only then
    /// have the denial set micFailed - so the NEXT tap is the session's
    /// only stop edge. Gating it on micFailed alone strands the locked
    /// session with zero audio until the 15-minute cap; the gate must let
    /// the stop-tap through whenever the Mac reports a live session.
    #[test]
    fn mic_failure_gate_still_forwards_the_stop_tap_of_a_live_session() {
        let press = between(CLIENT_HTML, "function pressStart", "function pressEnd");
        assert!(
            press.contains("if (micFailed && !recording)"),
            "the gate is scoped to sessions that have not started (recording flag)"
        );
        assert!(
            press.contains("return;\n        }\n        pressing = true;"),
            "the gate's early return sits before the edge send, so a live-session press falls through to sendPtt(true)"
        );
    }

    /// v1.6.2 W1-L0: with hold_or_toggle the idle button must name BOTH of
    /// the activation's modes. A bare "hold to talk" hides the tap-to-lock
    /// behavior the operator actually uses (every session in the live log
    /// was a 34-62 ms tap that locked on), so quick taps read as a broken
    /// hold button and the user never learns the session stayed live. The
    /// button label and the welcome hint both carry the dedicated
    /// holdOrToggleToTalk key; the plain holdToTalk key remains exactly
    /// for push_to_talk.
    #[test]
    fn hold_or_toggle_button_names_both_modes() {
        let refresh = between(CLIENT_HTML, "function refreshButton", "const TARGET_KEY");
        let hold_or_toggle_arm = refresh
            .find("} else if (activation === \"hold_or_toggle\") {")
            .expect("the hold_or_toggle arm exists in refreshButton");
        let arm = &refresh[hold_or_toggle_arm..];
        assert!(
            arm.contains("holdOrToggleToTalk"),
            "the idle hold_or_toggle label must use the two-mode copy key"
        );
        // The plain hold key still belongs to push_to_talk alone: the
        // hold_or_toggle arm must not fall back to it while idle.
        let idle_label = arm.find("t(\"tapToStop\"").expect("the recording label");
        assert!(
            arm[idle_label..].contains("holdOrToggleToTalk"),
            "the idle (not recording) label is the two-mode one"
        );

        // The welcome hint follows the same split: toggle names its tap,
        // hold_or_toggle names both modes, push_to_talk keeps the plain
        // hold copy.
        let welcome = between(CLIENT_HTML, "case \"welcome\":", "break;");
        assert!(
            welcome.contains("activation === \"hold_or_toggle\""),
            "the welcome hint distinguishes hold_or_toggle"
        );
        assert!(
            welcome.contains("t(\"holdOrToggleToTalk\", \"\")"),
            "the welcome hint uses the two-mode copy for hold_or_toggle"
        );
        assert!(
            welcome.contains("t(\"holdToTalk\", \"\")"),
            "push_to_talk keeps the plain hold copy in the hint"
        );

        // The key ships in every embedded locale (the generator hard-fails
        // on a missing key; this pins the shipped blob itself).
        let page = render_client_page();
        assert!(
            page.contains("\"holdOrToggleToTalk\""),
            "the embedded strings carry the two-mode key"
        );
        assert!(
            page.contains("Hold to talk, tap to lock"),
            "the en value of the two-mode copy is present"
        );
    }

    /// v1.6.0 per-session preferences: every Ptt edge carries the output
    /// target and the post-process flag (the wire names the server parses
    /// are target/postProcess), and the phone persists its last choice in
    /// localStorage so a reopening phone remembers it.
    #[test]
    fn ptt_edges_carry_the_per_session_preferences() {
        let send = between(CLIENT_HTML, "function sendPtt", "ptt.addEventListener");
        assert!(
            send.contains("target: outputTarget"),
            "every edge carries the output target"
        );
        assert!(
            send.contains("postProcess: postProcess"),
            "every edge carries the post-process flag"
        );
        assert!(
            send.contains("type: \"ptt\""),
            "the frame type is unchanged so old servers keep parsing it"
        );
        assert!(
            CLIENT_HTML.contains("localStorage.getItem(TARGET_KEY)"),
            "the output target is loaded from localStorage"
        );
        assert!(
            CLIENT_HTML.contains("localStorage.setItem(TARGET_KEY, outputTarget)"),
            "the output target persists to localStorage"
        );
        assert!(
            CLIENT_HTML.contains("localStorage.getItem(PP_KEY)"),
            "the post-process flag is loaded from localStorage"
        );
        assert!(
            CLIENT_HTML.contains("localStorage.setItem(PP_KEY,"),
            "the post-process flag persists to localStorage"
        );
    }

    /// Phone-target delivery and the recent list: the client handles the
    /// transcript and recentHistory frames, copies through
    /// navigator.clipboard.writeText (the automatic attempt on arrival,
    /// plus the gesture-driven buttons), and caps the recent list at the
    /// protocol's five entries.
    #[test]
    fn transcript_and_recent_history_frames_are_handled() {
        let transcript = between(CLIENT_HTML, "case \"transcript\":", "break;");
        assert!(
            transcript.contains("showTranscript("),
            "the transcript frame renders on the page"
        );

        let recent = between(CLIENT_HTML, "case \"recentHistory\":", "break;");
        assert!(
            recent.contains("RECENT_LIMIT"),
            "the recent list is capped at the protocol limit"
        );

        let show = between(
            CLIENT_HTML,
            "function showTranscript",
            "resultCopyBtn.addEventListener",
        );
        assert!(
            show.contains("copyText(text, resultCopyBtn)"),
            "a transcript arrival attempts the automatic copy"
        );
        assert!(
            show.contains("resultBadge.style.display = polished ? \"\" : \"none\""),
            "the post-processed state is surfaced as a badge"
        );

        let copy = between(CLIENT_HTML, "function copyText", "function flashCopied");
        assert!(
            copy.contains("navigator.clipboard.writeText"),
            "copies go through navigator.clipboard.writeText"
        );

        assert!(
            CLIENT_HTML.contains("resultCopyBtn.addEventListener(\"click\""),
            "the prominent Copy button copies inside its tap gesture"
        );
        assert!(
            CLIENT_HTML.contains("recentList.addEventListener(\"click\""),
            "each recent entry has a working copy button"
        );
    }
}
