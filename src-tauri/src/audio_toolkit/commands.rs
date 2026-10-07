//! Command mode: the during-dictation grammar for editing the live buffer.
//!
//! Command mode is NOT a recording trigger. The transcribe_commands
//! binding is a modifier: while a NORMAL dictation session is live,
//! holding it switches that session into command interpretation, and the
//! interim transcript deltas arriving while it is held are parsed here and
//! their actions edit the SESSION BUFFER live (see
//! `StreamSessionBuffer::render` in `managers/transcription.rs`). Releasing
//! the modifier returns the session to normal dictation, and the final
//! paste delivers exactly the edited buffer.
//!
//! GRAMMAR
//! =======
//!
//! Spoken phrase (case-insensitive, matched on word boundaries)  | Action
//! ------------------------------------------------------------- | ------
//! "question mark"                                               | Insert("?")
//! "full stop" or "period"                                      | Insert(".")
//! "comma"                                                       | Insert(",")
//! "new line"                                                    | Insert("\n")
//! "new paragraph"                                               | Insert("\n\n")
//! "delete word"                                                 | DeleteWord
//! "delete line"                                                 | DeleteLine
//! "undo"                                                        | Undo
//! "paste"                                                       | Paste
//!
//! Every other word is DISCARDED. That is the command-mode contract:
//! nothing unrecognized ever reaches the dictation buffer, so the user
//! can speak filler ("um", "eh", half-sentences) while issuing commands
//! and nothing stray lands in the pasted text. A delta with no recognized
//! commands produces no actions and edits nothing.
//!
//! TARGET-APP ACTIONS DURING A SESSION: "undo" and "paste" act on the
//! focused app, not the dictation buffer, and injecting keystrokes
//! mid-dictation is forbidden (the synthesized events would race the
//! recording, and undoing in the target app while its final text has not
//! landed yet is meaningless). Both are recognized by the grammar but
//! deliberately IGNORED by [`apply_command_delta_to_buffer`]; only the
//! buffer-edit actions (Insert, DeleteWord, DeleteLine) have an effect
//! while the modifier is held.
//!
//! Matching follows the phrase style of the spoken-punctuation normalizer
//! in [`super::text`]: phrases are matched whole (two-word entries before
//! any shorter rival, so "new paragraph" can never fire on "new"), tokens
//! are compared case-insensitively, and punctuation the model attached to a
//! token (a trailing comma or period) is stripped before comparison so
//! "undo," still reads as "undo". Unlike the normalizer, unmatched words are
//! dropped rather than kept, because here the transcript is a program, not
//! prose.

use super::text::{remove_trailing_line_from_buffer, remove_trailing_word_from_buffer};

/// One parsed command-mode action, in transcript order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandAction {
    /// Insert literal text (punctuation, line breaks).
    Insert(&'static str),
    /// Delete the word before the caret.
    DeleteWord,
    /// Delete the current line.
    DeleteLine,
    /// Undo the last edit. Target-app action: ignored during a session.
    Undo,
    /// Send a plain paste chord. Target-app action: ignored during a
    /// session.
    Paste,
}

/// The command vocabulary. Two-word phrases are listed first so the
/// longest phrase always wins at a given position.
const COMMAND_VOCABULARY: &[(&[&str], CommandAction)] = &[
    (&["question", "mark"], CommandAction::Insert("?")),
    (&["full", "stop"], CommandAction::Insert(".")),
    (&["new", "paragraph"], CommandAction::Insert("\n\n")),
    (&["new", "line"], CommandAction::Insert("\n")),
    (&["period"], CommandAction::Insert(".")),
    (&["comma"], CommandAction::Insert(",")),
    (&["delete", "word"], CommandAction::DeleteWord),
    (&["delete", "line"], CommandAction::DeleteLine),
    (&["undo"], CommandAction::Undo),
    (&["paste"], CommandAction::Paste),
];

/// Longest phrase in the vocabulary, in words. Every entry is at most this
/// long, so a lookahead window of this size is enough for longest-match.
const MAX_PHRASE_WORDS: usize = 2;

/// Normalize one whitespace-delimited token for comparison: lowercase it and
/// strip leading/trailing punctuation the model may have attached. Inner
/// characters (apostrophes in "don't") are kept; such words simply never
/// match a command and are discarded per the contract.
fn normalize_token(token: &str) -> String {
    token
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase()
}

/// Parse a raw transcript into the command sequence it spells out, in
/// transcript order. Unmatched words are discarded; a transcript with no
/// recognized commands yields an empty vector.
pub fn parse_command_transcript(transcript: &str) -> Vec<CommandAction> {
    let tokens: Vec<String> = transcript
        .split_whitespace()
        .map(normalize_token)
        .filter(|token| !token.is_empty())
        .collect();

    let mut actions = Vec::new();
    let mut position = 0;

    while position < tokens.len() {
        // Longest match first: try the largest window that fits, then shrink.
        let mut matched = None;
        for length in (1..=MAX_PHRASE_WORDS.min(tokens.len() - position)).rev() {
            let window = &tokens[position..position + length];
            if let Some((_, action)) = COMMAND_VOCABULARY.iter().find(|(phrase, _)| {
                phrase.len() == length
                    && phrase
                        .iter()
                        .zip(window)
                        .all(|(expected, actual)| *expected == actual.as_str())
            }) {
                matched = Some((*action, length));
                break;
            }
        }

        match matched {
            Some((action, length)) => {
                actions.push(action);
                position += length;
            }
            None => {
                // Unrecognized word: consume and discard it.
                position += 1;
            }
        }
    }

    actions
}

/// Apply a command-mode transcript delta to the live session buffer, in
/// delta order. The buffer-edit actions:
///
/// * `Insert` appends the literal text (punctuation, line breaks);
/// * `DeleteWord` removes the trailing word from the buffer, using the
///   same word semantics as the delete-last-word hotkey and the
///   voice-deletion "scratch that" (attached punctuation goes with the
///   word, the separating whitespace is kept for the re-join);
/// * `DeleteLine` clears the current trailing line, with the same
///   semantics as the voice-deletion "delete line": everything after the
///   last newline goes (the newline is kept so text spoken next starts on
///   the fresh line), and with no newline the whole buffer empties.
///
/// `Undo` and `Paste` act on the TARGET APP and are deliberately ignored
/// here: no keystroke is injected mid-dictation (see the module docs). A
/// blank or wholly unrecognized delta edits nothing.
pub fn apply_command_delta_to_buffer(buffer: &mut String, delta: &str) {
    for action in parse_command_transcript(delta) {
        match action {
            CommandAction::Insert(text) => buffer.push_str(text),
            CommandAction::DeleteWord => {
                *buffer = remove_trailing_word_from_buffer(buffer)
            }
            CommandAction::DeleteLine => {
                *buffer = remove_trailing_line_from_buffer(buffer)
            }
            // Target-app actions: inert during a live session.
            CommandAction::Undo | CommandAction::Paste => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_every_vocabulary_entry() {
        assert_eq!(
            parse_command_transcript("question mark"),
            vec![CommandAction::Insert("?")]
        );
        assert_eq!(
            parse_command_transcript("full stop"),
            vec![CommandAction::Insert(".")]
        );
        assert_eq!(
            parse_command_transcript("period"),
            vec![CommandAction::Insert(".")]
        );
        assert_eq!(
            parse_command_transcript("comma"),
            vec![CommandAction::Insert(",")]
        );
        assert_eq!(
            parse_command_transcript("new line"),
            vec![CommandAction::Insert("\n")]
        );
        assert_eq!(
            parse_command_transcript("new paragraph"),
            vec![CommandAction::Insert("\n\n")]
        );
        assert_eq!(
            parse_command_transcript("delete word"),
            vec![CommandAction::DeleteWord]
        );
        assert_eq!(
            parse_command_transcript("delete line"),
            vec![CommandAction::DeleteLine]
        );
        assert_eq!(parse_command_transcript("undo"), vec![CommandAction::Undo]);
        assert_eq!(
            parse_command_transcript("paste"),
            vec![CommandAction::Paste]
        );
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert_eq!(
            parse_command_transcript("UNDO Question MARK Comma"),
            vec![
                CommandAction::Undo,
                CommandAction::Insert("?"),
                CommandAction::Insert(",")
            ]
        );
    }

    #[test]
    fn attached_punctuation_does_not_block_matching() {
        // The model often glues punctuation onto spoken tokens.
        assert_eq!(
            parse_command_transcript("undo, comma. period!"),
            vec![
                CommandAction::Undo,
                CommandAction::Insert(","),
                CommandAction::Insert(".")
            ]
        );
    }

    #[test]
    fn unmatched_words_are_discarded_in_order() {
        assert_eq!(
            parse_command_transcript("um could you please comma thanks"),
            vec![CommandAction::Insert(",")]
        );
        assert_eq!(
            parse_command_transcript("hello new paragraph world undo"),
            vec![CommandAction::Insert("\n\n"), CommandAction::Undo]
        );
    }

    #[test]
    fn word_boundaries_are_respected() {
        // Longer words that merely contain a command word must not fire.
        assert!(parse_command_transcript("undoing").is_empty());
        assert!(parse_command_transcript("commando").is_empty());
        assert!(parse_command_transcript("pastel compass").is_empty());
        assert!(parse_command_transcript("periodic").is_empty());
        // "new" alone is not "new line" or "new paragraph".
        assert!(parse_command_transcript("new").is_empty());
    }

    #[test]
    fn two_word_phrases_win_over_single_words() {
        // "new paragraph" must not parse as a discarded "new" plus anything.
        assert_eq!(
            parse_command_transcript("new paragraph"),
            vec![CommandAction::Insert("\n\n")]
        );
        assert_eq!(
            parse_command_transcript("delete word"),
            vec![CommandAction::DeleteWord]
        );
    }

    #[test]
    fn empty_or_unrecognized_transcripts_produce_no_actions() {
        assert!(parse_command_transcript("").is_empty());
        assert!(parse_command_transcript("   ").is_empty());
        assert!(parse_command_transcript("the weather is lovely").is_empty());
    }

    #[test]
    fn repeated_actions_repeat() {
        assert_eq!(
            parse_command_transcript("delete word delete word delete word"),
            vec![
                CommandAction::DeleteWord,
                CommandAction::DeleteWord,
                CommandAction::DeleteWord
            ]
        );
        assert_eq!(
            parse_command_transcript("undo undo"),
            vec![CommandAction::Undo, CommandAction::Undo]
        );
    }

    // -----------------------------------------------------------------
    // Buffer-edit application (the in-session command contract).
    // -----------------------------------------------------------------

    #[test]
    fn inserts_append_literal_text_to_the_buffer() {
        let mut buffer = "hello world".to_string();
        apply_command_delta_to_buffer(&mut buffer, "comma question mark");
        assert_eq!(buffer, "hello world,?");
        apply_command_delta_to_buffer(&mut buffer, "new line");
        assert_eq!(buffer, "hello world,?\n");
        apply_command_delta_to_buffer(&mut buffer, "new paragraph");
        assert_eq!(buffer, "hello world,?\n\n\n");
    }

    #[test]
    fn delete_word_uses_the_buffer_word_semantics() {
        let mut buffer = "one two three".to_string();
        apply_command_delta_to_buffer(&mut buffer, "delete word");
        assert_eq!(buffer, "one two ");
        apply_command_delta_to_buffer(&mut buffer, "delete word");
        assert_eq!(buffer, "one ");
        // Deleting from an empty buffer stays empty.
        let mut empty = " ".to_string();
        apply_command_delta_to_buffer(&mut empty, "delete word");
        assert_eq!(empty, "");
    }

    #[test]
    fn delete_line_clears_the_trailing_line() {
        let mut buffer = "first\nsecond third".to_string();
        apply_command_delta_to_buffer(&mut buffer, "delete line");
        assert_eq!(buffer, "first\n");
        // No newline: the whole buffer empties.
        let mut single = "only line".to_string();
        apply_command_delta_to_buffer(&mut single, "delete line");
        assert_eq!(single, "");
    }

    #[test]
    fn target_app_actions_edit_nothing_during_a_session() {
        let mut buffer = "hello world".to_string();
        apply_command_delta_to_buffer(&mut buffer, "undo paste");
        assert_eq!(buffer, "hello world");
    }

    #[test]
    fn actions_apply_in_delta_order() {
        let mut buffer = "one two three".to_string();
        apply_command_delta_to_buffer(&mut buffer, "delete word comma delete word");
        assert_eq!(buffer, "one two ");
    }

    #[test]
    fn blank_or_unrecognized_delta_edits_nothing() {
        let mut buffer = "hello world".to_string();
        apply_command_delta_to_buffer(&mut buffer, "");
        apply_command_delta_to_buffer(&mut buffer, "   ");
        apply_command_delta_to_buffer(&mut buffer, "um eh nothing recognizable");
        assert_eq!(buffer, "hello world");
    }
}
