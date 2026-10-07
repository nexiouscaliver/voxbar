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
pub(crate) const COMMAND_VOCABULARY: &[(&[&str], CommandAction)] = &[
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

/// Byte ranges `[start, end)` of the text's whitespace-delimited tokens, in
/// order, so hold/flush decisions can map token windows back to byte spans.
fn token_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start: Option<usize> = None;
    for (index, c) in text.char_indices() {
        if c.is_whitespace() {
            if let Some(open) = start.take() {
                ranges.push((open, index));
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(open) = start {
        ranges.push((open, text.len()));
    }
    ranges
}

/// Whether the token window exactly spells out a whole vocabulary phrase
/// (every token complete, phrase length equal to the window).
fn window_completes_phrase(window: &[&str], vocabulary: &[(&[&str], CommandAction)]) -> bool {
    vocabulary.iter().any(|(phrase, _)| {
        phrase.len() == window.len()
            && phrase
                .iter()
                .zip(window)
                .all(|(expected, actual)| *expected == normalize_token(actual))
    })
}

/// Whether the token window is a PROPER PREFIX of some vocabulary phrase:
/// it matches the phrase's leading tokens without completing any phrase.
/// Every held token except the last must match its phrase token exactly;
/// the last may be a PARTIAL word (a string prefix of its phrase token,
/// strictly shorter when the window already spans the whole phrase).
fn window_is_proper_prefix(window: &[&str], vocabulary: &[(&[&str], CommandAction)]) -> bool {
    vocabulary.iter().any(|(phrase, _)| {
        if window.len() > phrase.len() {
            return false;
        }
        let split = window.len() - 1;
        let head = &window[..split];
        let last = window[split];
        let phrase_head = &phrase[..split];
        let phrase_last = phrase[split];
        if !head
            .iter()
            .zip(phrase_head)
            .all(|(actual, expected)| *expected == normalize_token(actual))
        {
            return false;
        }
        let normalized_last = normalize_token(last);
        // A punctuation-only token prefixes nothing.
        if normalized_last.is_empty() {
            return false;
        }
        if window.len() == phrase.len() {
            // Equality here would COMPLETE the phrase; require a proper
            // string prefix so the fragment still needs its tail.
            normalized_last.len() < phrase_last.len() && phrase_last.starts_with(normalized_last.as_str())
        } else {
            phrase_last.starts_with(normalized_last.as_str())
        }
    })
}

/// Byte length of the trailing fragment a command-active render tick should
/// HOLD BACK: the longest trailing token sequence (1 to the vocabulary's
/// longest phrase, last token allowed to be a PARTIAL word) that is a proper
/// prefix of some vocabulary phrase without itself completing any phrase.
///
/// The returned span INCLUDES the whitespace separator run immediately
/// preceding the held tokens, so the fragment re-enters the next delta as a
/// whole word and the interim display keeps the separator (the raw-domain
/// join collapses a duplicated separator but never inserts one). A streaming
/// delta that ends mid-word (" com" for "comma") or mid-phrase (" question"
/// for "question mark") would otherwise be consumed and silently discarded
/// by the grammar; holding it lets the next delta re-parse the whole token.
pub(crate) fn held_prefix_len(delta: &str, vocabulary: &[(&[&str], CommandAction)]) -> usize {
    let max_words = vocabulary
        .iter()
        .map(|(phrase, _)| phrase.len())
        .max()
        .unwrap_or(0);
    if max_words == 0 {
        return 0;
    }

    let tokens = token_ranges(delta);
    for window_len in (1..=max_words.min(tokens.len())).rev() {
        let window: Vec<&str> = tokens[tokens.len() - window_len..]
            .iter()
            .map(|&(start, end)| &delta[start..end])
            .collect();
        // A sequence that already completes a command applies at once and
        // never grows into a longer phrase.
        if window_completes_phrase(&window, vocabulary) {
            continue;
        }
        if window_is_proper_prefix(&window, vocabulary) {
            // The separator run before the first held token travels with
            // the fragment: the span runs from there to the end.
            let first_start = tokens[tokens.len() - window_len].0;
            let before = &delta[..first_start];
            let span_start = before.trim_end().len();
            return delta.len() - span_start;
        }
    }
    0
}

/// Byte length of the leading span the release/finalize flush consumes from
/// an unresolved held region: the region's first whitespace-delimited word
/// WITH its preceding separator, then whole words while the consumed span
/// remains a proper prefix of some vocabulary phrase (so a held " new l"
/// resolves to the whole "new line", not just " new"). Stops at the end of
/// the text, when the span completes a phrase, or when it is no longer a
/// prefix. The caller parses the consumed span through the command grammar;
/// anything beyond it stays unconsumed and flows on as normal dictation.
pub(crate) fn flush_command_prefix_len(text: &str, vocabulary: &[(&[&str], CommandAction)]) -> usize {
    let tokens = token_ranges(text);
    let Some(&(_, first_end)) = tokens.first() else {
        return 0;
    };

    // The first word travels with its preceding separator, so the span
    // always begins at the region's start (byte 0).
    let mut consumed = first_end;
    let mut count = 1;
    loop {
        let window: Vec<&str> = tokens[..count]
            .iter()
            .map(|&(start, end)| &text[start..end])
            .collect();
        if window_completes_phrase(&window, vocabulary) {
            break;
        }
        if !window_is_proper_prefix(&window, vocabulary) {
            break;
        }
        if count >= tokens.len() {
            break;
        }
        consumed = tokens[count].1;
        count += 1;
    }
    consumed
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

/// Whether an inserted literal is a coalescible symbol: a single
/// non-alphanumeric, non-newline character (the punctuation inserts).
/// Repeating a symbol command that LOOKED dead (the fragmentation this
/// module now fixes) used to double the punctuation; identical adjacent
/// symbols coalesce instead. Line breaks never coalesce ("new paragraph"
/// after "new line" must stack) and neither do words.
fn is_coalescible_symbol(text: &str) -> bool {
    let mut chars = text.chars();
    matches!(chars.next(), Some(c) if !c.is_alphanumeric() && c != '\n' && chars.next().is_none())
}

/// Apply a command-mode transcript delta to the live session buffer, in
/// delta order. The buffer-edit actions:
///
/// * `Insert` appends the literal text (punctuation, line breaks); a
///   repeated identical single-symbol insert onto a trimmed buffer tail
///   that already ends with that symbol is skipped (coalescing);
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
            CommandAction::Insert(text) => {
                // Coalesce the operator's repeated identical symbol command:
                // the buffer's trimmed tail already ends with the symbol.
                // The trim ignores spaces/tabs (a delete-word leaves them
                // behind) but NEVER a line break: "new line" then "comma"
                // must still land "\n," (spec F2 item 5), so a full
                // str::trim_end would be wrong here.
                let trimmed_tail = &buffer[..buffer.trim_end_matches([' ', '\t']).len()];
                if is_coalescible_symbol(text) && trimmed_tail.ends_with(text) {
                    continue;
                }
                buffer.push_str(text);
            }
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

    // -----------------------------------------------------------------
    // Proper-prefix holdback (streaming fragmentation).
    // -----------------------------------------------------------------

    #[test]
    fn held_prefix_len_returns_the_pinned_spans() {
        // A whole command never holds: it completes a phrase and applies.
        assert_eq!(held_prefix_len(" comma", COMMAND_VOCABULARY), 0);
        // A partial word holds WITH its preceding separator (" com").
        assert_eq!(held_prefix_len(" com", COMMAND_VOCABULARY), 4);
        // A whole word that only opens a two-word phrase holds.
        assert_eq!(held_prefix_len(" question", COMMAND_VOCABULARY), 9);
        // A partial second word of a phrase holds the whole span.
        assert_eq!(held_prefix_len(" new l", COMMAND_VOCABULARY), 6);
        // Words that prefix nothing hold nothing.
        assert_eq!(held_prefix_len(" um stuff", COMMAND_VOCABULARY), 0);
    }

    #[test]
    fn held_prefix_len_edge_spans() {
        // "new" opens two phrases but completes none: held, separator
        // included.
        assert_eq!(held_prefix_len(" new", COMMAND_VOCABULARY), 4);
        // A fragment with no separator before it holds just the token.
        assert_eq!(held_prefix_len("com", COMMAND_VOCABULARY), 3);
        // A blank delta holds nothing.
        assert_eq!(held_prefix_len("  ", COMMAND_VOCABULARY), 0);
        // Commands earlier in the delta do not stop the trailing hold.
        assert_eq!(held_prefix_len(" comma quest", COMMAND_VOCABULARY), 6);
    }

    #[test]
    fn flush_command_prefix_len_grows_to_phrase_completion() {
        // The first word completes a phrase: consume exactly it (with its
        // preceding separator); the dictation beyond stays unconsumed.
        assert_eq!(flush_command_prefix_len(" comma and more", COMMAND_VOCABULARY), 6);
        // A held phrase opener grows word by word until the phrase
        // completes, so a held " new l" resolves to the whole "new line".
        assert_eq!(flush_command_prefix_len(" new line next", COMMAND_VOCABULARY), 9);
        // Longest phrase: "new paragraph" consumes both words.
        assert_eq!(
            flush_command_prefix_len(" new paragraph tail", COMMAND_VOCABULARY),
            " new paragraph".len()
        );
        // A still-partial fragment at flush time is consumed whole and
        // then discarded by the grammar.
        assert_eq!(flush_command_prefix_len(" com", COMMAND_VOCABULARY), 4);
        // Nothing to consume.
        assert_eq!(flush_command_prefix_len("", COMMAND_VOCABULARY), 0);
        assert_eq!(flush_command_prefix_len("   ", COMMAND_VOCABULARY), 0);
    }

    // -----------------------------------------------------------------
    // Duplicate symbol insert coalescing.
    // -----------------------------------------------------------------

    #[test]
    fn duplicate_symbol_inserts_coalesce() {
        let mut buffer = "hello world".to_string();
        apply_command_delta_to_buffer(&mut buffer, "comma");
        assert_eq!(buffer, "hello world,");
        // Repeating the identical symbol command (the operator's old
        // response to a command that LOOKED dead) coalesces to one.
        apply_command_delta_to_buffer(&mut buffer, "comma");
        assert_eq!(buffer, "hello world,");
        // Same within one delta: per-token conversion this is not.
        let mut once = "x".to_string();
        apply_command_delta_to_buffer(&mut once, "comma comma");
        assert_eq!(once, "x,");
    }

    #[test]
    fn line_breaks_and_distinct_symbols_never_coalesce() {
        let mut buffer = "hello world".to_string();
        apply_command_delta_to_buffer(&mut buffer, "new line");
        apply_command_delta_to_buffer(&mut buffer, "new line");
        assert_eq!(buffer, "hello world\n\n");
        // "new line" then "comma" still lands the comma after the break.
        apply_command_delta_to_buffer(&mut buffer, "comma");
        assert_eq!(buffer, "hello world\n\n,");
        // Distinct symbols stack.
        let mut mixed = "x".to_string();
        apply_command_delta_to_buffer(&mut mixed, "comma question mark");
        assert_eq!(mixed, "x,?");
        // A period after a comma is a different symbol: both land.
        apply_command_delta_to_buffer(&mut mixed, "period");
        assert_eq!(mixed, "x,?.");
    }
}
