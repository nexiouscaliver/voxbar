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
//! The vocabulary is the editable COMMAND MATRIX (see [`super::command_matrix`]):
//! every matrix phrase parses to its command's action, so "scratch that"
//! also deletes a word here (the DeleteWord unification) and the extended
//! symbol set (brackets, braces, at sign, hash, ...) is available verbatim.
//! Spoken phrase (case-insensitive, matched on whole tokens) | Action
//! --------------------------------------------------------- | ------
//! "question mark"                                            | Insert("?")
//! "full stop" or "period"                                    | Insert(".")
//! "comma"                                                    | Insert(",")
//! "new line"                                                 | Insert("\n")
//! "new paragraph"                                            | Insert("\n\n")
//! "delete word" (also "scratch that", "delete that", ...)    | DeleteWord
//! "delete line"                                              | DeleteLine
//! "delete everything" (also "scratch everything", ...)       | ClearAll
//! "undo"                                                     | Undo
//! "paste"                                                    | Paste
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

use super::command_matrix::CompiledCommandMatrix;
use super::text::{
    remove_trailing_line_from_buffer_reporting, remove_trailing_word_from_buffer_reporting,
};
use strsim::levenshtein;

/// One parsed command-mode action, in transcript order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandAction {
    /// Insert literal text (punctuation, line breaks).
    Insert(&'static str),
    /// Delete the word before the caret.
    DeleteWord,
    /// Delete the current line.
    DeleteLine,
    /// Discard everything dictated so far; the session continues (the
    /// same semantics as the Undo binding's in-session start-over).
    ClearAll,
    /// Undo the last edit. Target-app action: ignored during a session.
    Undo,
    /// Send a plain paste chord. Target-app action: ignored during a
    /// session.
    Paste,
}

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

/// Whether the token window exactly spells out a whole matrix phrase
/// (every token complete, phrase length equal to the window).
fn window_completes_phrase(window: &[&str], matrix: &CompiledCommandMatrix) -> bool {
    matrix.parser.iter().any(|(phrase, _)| {
        phrase.len() == window.len()
            && phrase
                .iter()
                .zip(window)
                .all(|(expected, actual)| expected == &normalize_token(actual))
    })
}

/// Whether the token window is a PROPER PREFIX of some matrix phrase:
/// it matches the phrase's leading tokens without completing any phrase.
/// Every held token except the last must match its phrase token exactly;
/// the last may be a PARTIAL word (a string prefix of its phrase token,
/// strictly shorter when the window already spans the whole phrase).
fn window_is_proper_prefix(window: &[&str], matrix: &CompiledCommandMatrix) -> bool {
    matrix.parser.iter().any(|(phrase, _)| {
        if window.len() > phrase.len() {
            return false;
        }
        let split = window.len() - 1;
        let head = &window[..split];
        let last = window[split];
        let phrase_head = &phrase[..split];
        let phrase_last = &phrase[split];
        if !head
            .iter()
            .zip(phrase_head)
            .all(|(actual, expected)| expected == &normalize_token(actual))
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
            normalized_last.len() < phrase_last.len()
                && phrase_last.starts_with(normalized_last.as_str())
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
pub(crate) fn held_prefix_len(delta: &str, matrix: &CompiledCommandMatrix) -> usize {
    let max_words = matrix.max_phrase_words;
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
        if window_completes_phrase(&window, matrix) {
            continue;
        }
        if window_is_proper_prefix(&window, matrix) {
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
/// resolves to the whole "new line", not just " new"). The first word must
/// itself complete a phrase or open one (a proper prefix): an ordinary word
/// that merely STARTS like a command word ("computer" of "comma") consumes
/// NOTHING, so the whole region flows back as dictation instead of being
/// eaten and then discarded by the grammar. Stops at the end of the text,
/// when the span completes a phrase, or when it is no longer a prefix. The
/// caller parses the consumed span through the command grammar; anything
/// beyond it stays unconsumed and flows on as normal dictation.
pub(crate) fn flush_command_prefix_len(text: &str, matrix: &CompiledCommandMatrix) -> usize {
    let tokens = token_ranges(text);
    let Some(&(_, first_end)) = tokens.first() else {
        return 0;
    };

    // Gate on the first word BEFORE committing to any consumption: a flush
    // region led by a non-command word is really release-snapshot
    // dictation, not a held fragment's completion.
    let first_window = [&text[tokens[0].0..tokens[0].1]];
    if !window_completes_phrase(&first_window, matrix)
        && !window_is_proper_prefix(&first_window, matrix)
    {
        return 0;
    }

    // The first word travels with its preceding separator, so the span
    // always begins at the region's start (byte 0).
    let mut consumed = first_end;
    let mut count = 1;
    loop {
        let window: Vec<&str> = tokens[..count]
            .iter()
            .map(|&(start, end)| &text[start..end])
            .collect();
        if window_completes_phrase(&window, matrix) {
            break;
        }
        if !window_is_proper_prefix(&window, matrix) {
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

/// Minimum length (characters) a single-word phrase must have before a
/// command-mode token may fuzzy match it at edit distance 1. Long enough
/// that everyday near-misses of short phrases ("pas" vs "paste") never
/// fire, short enough to catch whisper's real misspellings ("coma" for
/// "comma").
const FUZZY_MIN_PHRASE_CHARS: usize = 5;

/// Command-mode-only fuzzy resolution: the action of a single-word SYMBOL
/// phrase (an Insert) of at least [`FUZZY_MIN_PHRASE_CHARS`] characters
/// within edit distance 1 of `token`. The modifier being held says "this
/// is a command", so a near-miss of an unambiguous spoken symbol command
/// resolves to it; normal dictation (the text pass) never fuzzy matches.
/// Deliberately scoped to Insert actions: the control commands (Paste is
/// 5 characters) sit inside ordinary vocabulary ("pastel"), and a spurious
/// target-app command is worse than a discarded filler word.
fn fuzzy_single_token_action(token: &str, matrix: &CompiledCommandMatrix) -> Option<CommandAction> {
    matrix
        .parser
        .iter()
        .filter(|(phrase, action)| {
            phrase.len() == 1 && matches!(action, CommandAction::Insert(_))
        })
        .find(|(phrase, _)| {
            phrase[0].chars().count() >= FUZZY_MIN_PHRASE_CHARS
                && levenshtein(token, &phrase[0]) == 1
        })
        .map(|(_, action)| *action)
}

/// Parse a raw transcript into the command sequence it spells out, in
/// transcript order. The vocabulary is the compiled command matrix;
/// unmatched words are discarded; a transcript with no recognized
/// commands yields an empty vector. A single token within edit distance 1
/// of a >= 5-character single-word phrase resolves to that command
/// (command mode is explicitly commanding; see
/// [`fuzzy_single_token_action`]).
pub fn parse_command_transcript(
    transcript: &str,
    matrix: &CompiledCommandMatrix,
) -> Vec<CommandAction> {
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
        for length in (1..=matrix.max_phrase_words.min(tokens.len() - position)).rev() {
            let window = &tokens[position..position + length];
            if let Some((_, action)) = matrix.parser.iter().find(|(phrase, _)| {
                phrase.len() == length
                    && phrase
                        .iter()
                        .zip(window)
                        .all(|(expected, actual)| expected == actual)
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
            None => match fuzzy_single_token_action(&tokens[position], matrix) {
                Some(action) => {
                    actions.push(action);
                    position += 1;
                }
                None => {
                    // Unrecognized word: consume and discard it.
                    position += 1;
                }
            },
        }
    }

    actions
}

/// Whether an inserted literal is a coalescible symbol: a single
/// non-alphanumeric, non-newline character (the punctuation inserts).
/// Repeating a symbol command that LOOKED dead (the fragmentation this
/// module now fixes) used to double the punctuation; identical adjacent
/// symbols coalesce instead. Line breaks never coalesce ("new paragraph"
/// after "new line" must stack) and neither do words. Shared with the
/// spoken-punctuation text pass, whose mark+word dedup follows the same
/// semantics (text.rs).
pub(crate) fn is_coalescible_symbol(text: &str) -> bool {
    let mut chars = text.chars();
    matches!(chars.next(), Some(c) if !c.is_alphanumeric() && c != '\n' && chars.next().is_none())
}

/// Apply a command-mode transcript delta to the live session buffer, in
/// delta order, with the compiled command matrix as vocabulary. The
/// buffer-edit actions:
///
/// * `Insert` appends the literal text (punctuation, line breaks); a
///   repeated identical single-symbol insert onto a trimmed buffer tail
///   that already ends with that symbol is skipped (coalescing), UNLESS
///   the immediately preceding action in THIS delta inserted the same
///   symbol: same-delta repeats are intentional doubles ("comma comma"
///   said in one breath lands ",,") while the cross-delta repeat is the
///   "looked dead" retry that coalesces;
/// * `DeleteWord` removes the trailing word from the buffer, using the
///   same word semantics as the delete-last-word hotkey and the
///   voice-deletion "scratch that" (attached punctuation goes with the
///   word, the separating whitespace is kept for the re-join);
/// * `DeleteLine` clears the current trailing line, with the same
///   semantics as the voice-deletion "delete line": everything after the
///   last newline goes (the newline is kept so text spoken next starts on
///   the fresh line), and with no newline the whole buffer empties;
/// * `ClearAll` clears the buffer and the session continues, mirroring
///   the Undo binding's in-session start-over (the voice-deletion surface
///   of the same phrases keeps its own short-circuit semantics instead:
///   paste nothing, skip every later pass).
///
/// `Undo` and `Paste` act on the TARGET APP and are deliberately ignored
/// here: no keystroke is injected mid-dictation (see the module docs). A
/// blank or wholly unrecognized delta edits nothing.
///
/// Returns the text removed by the buffer-side word/line deletions in
/// this delta (multiple removals join with a single space), or `None`
/// when nothing was deleted, so the caller can surface it on the next
/// stream-text event ("Removed:" chip). ClearAll reports nothing (it
/// matches the un-instrumented in-session start-over).
pub fn apply_command_delta_to_buffer(
    buffer: &mut String,
    delta: &str,
    matrix: &CompiledCommandMatrix,
) -> Option<String> {
    let mut removed: Option<String> = None;
    // The immediately preceding action in THIS delta: an intentional
    // same-delta repeat of the same symbol must land, while the same
    // symbol arriving in a LATER delta is the "looked dead" retry that
    // coalesces.
    let mut previous_action: Option<CommandAction> = None;
    for action in parse_command_transcript(delta, matrix) {
        match action {
            CommandAction::Insert(text) => {
                // Coalesce the operator's repeated identical symbol command:
                // the buffer's trimmed tail already ends with the symbol.
                // The trim ignores spaces/tabs (a delete-word leaves them
                // behind) but NEVER a line break: "new line" then "comma"
                // must still land "\n," (spec F2 item 5), so a full
                // str::trim_end would be wrong here. An insert that
                // directly follows the SAME insert in this delta is an
                // intentional double and skips the coalescing check.
                let same_delta_double = previous_action == Some(CommandAction::Insert(text));
                let trimmed_tail = &buffer[..buffer.trim_end_matches([' ', '\t']).len()];
                if !same_delta_double
                    && is_coalescible_symbol(text)
                    && trimmed_tail.ends_with(text)
                {
                    previous_action = Some(action);
                    continue;
                }
                buffer.push_str(text);
            }
            CommandAction::DeleteWord => {
                let (next, word) = remove_trailing_word_from_buffer_reporting(buffer);
                *buffer = next;
                record_removal(&mut removed, word);
            }
            CommandAction::DeleteLine => {
                let (next, line) = remove_trailing_line_from_buffer_reporting(buffer);
                *buffer = next;
                record_removal(&mut removed, line);
            }
            CommandAction::ClearAll => buffer.clear(),
            // Target-app actions: inert during a live session.
            CommandAction::Undo | CommandAction::Paste => {}
        }
        previous_action = Some(action);
    }
    removed
}

/// Accumulates one deletion's removed text into the delta's running report.
fn record_removal(removed: &mut Option<String>, text: Option<String>) {
    if let Some(text) = text {
        match removed {
            Some(existing) => {
                existing.push(' ');
                existing.push_str(&text);
            }
            None => *removed = Some(text),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::command_matrix::default_compiled_matrix;
    use super::*;

    /// The compiled DEFAULT matrix (shared; no rebuild per test).
    fn dm() -> std::sync::Arc<super::super::command_matrix::CompiledCommandMatrix> {
        default_compiled_matrix()
    }

    #[test]
    fn recognizes_every_vocabulary_entry() {
        assert_eq!(
            parse_command_transcript("question mark", &dm()),
            vec![CommandAction::Insert("?")]
        );
        assert_eq!(
            parse_command_transcript("full stop", &dm()),
            vec![CommandAction::Insert(".")]
        );
        assert_eq!(
            parse_command_transcript("period", &dm()),
            vec![CommandAction::Insert(".")]
        );
        assert_eq!(
            parse_command_transcript("comma", &dm()),
            vec![CommandAction::Insert(",")]
        );
        assert_eq!(
            parse_command_transcript("new line", &dm()),
            vec![CommandAction::Insert("\n")]
        );
        assert_eq!(
            parse_command_transcript("new paragraph", &dm()),
            vec![CommandAction::Insert("\n\n")]
        );
        assert_eq!(
            parse_command_transcript("delete word", &dm()),
            vec![CommandAction::DeleteWord]
        );
        assert_eq!(
            parse_command_transcript("delete line", &dm()),
            vec![CommandAction::DeleteLine]
        );
        assert_eq!(
            parse_command_transcript("undo", &dm()),
            vec![CommandAction::Undo]
        );
        assert_eq!(
            parse_command_transcript("paste", &dm()),
            vec![CommandAction::Paste]
        );
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert_eq!(
            parse_command_transcript("UNDO Question MARK Comma", &dm()),
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
            parse_command_transcript("undo, comma. period!", &dm()),
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
            parse_command_transcript("um could you please comma thanks", &dm()),
            vec![CommandAction::Insert(",")]
        );
        assert_eq!(
            parse_command_transcript("hello new paragraph world undo", &dm()),
            vec![CommandAction::Insert("\n\n"), CommandAction::Undo]
        );
    }

    #[test]
    fn word_boundaries_are_respected() {
        // Longer words that merely contain a command word must not fire.
        assert!(parse_command_transcript("undoing", &dm()).is_empty());
        assert!(parse_command_transcript("commando", &dm()).is_empty());
        assert!(parse_command_transcript("pastel compass", &dm()).is_empty());
        assert!(parse_command_transcript("periodic", &dm()).is_empty());
        // "new" alone is not "new line" or "new paragraph".
        assert!(parse_command_transcript("new", &dm()).is_empty());
    }

    #[test]
    fn two_word_phrases_win_over_single_words() {
        // "new paragraph" must not parse as a discarded "new" plus anything.
        assert_eq!(
            parse_command_transcript("new paragraph", &dm()),
            vec![CommandAction::Insert("\n\n")]
        );
        assert_eq!(
            parse_command_transcript("delete word", &dm()),
            vec![CommandAction::DeleteWord]
        );
    }

    #[test]
    fn empty_or_unrecognized_transcripts_produce_no_actions() {
        assert!(parse_command_transcript("", &dm()).is_empty());
        assert!(parse_command_transcript("   ", &dm()).is_empty());
        assert!(parse_command_transcript("the weather is lovely", &dm()).is_empty());
    }

    #[test]
    fn repeated_actions_repeat() {
        assert_eq!(
            parse_command_transcript("delete word delete word delete word", &dm()),
            vec![
                CommandAction::DeleteWord,
                CommandAction::DeleteWord,
                CommandAction::DeleteWord
            ]
        );
        assert_eq!(
            parse_command_transcript("undo undo", &dm()),
            vec![CommandAction::Undo, CommandAction::Undo]
        );
    }

    // -----------------------------------------------------------------
    // Buffer-edit application (the in-session command contract).
    // -----------------------------------------------------------------

    #[test]
    fn inserts_append_literal_text_to_the_buffer() {
        let mut buffer = "hello world".to_string();
        apply_command_delta_to_buffer(&mut buffer, "comma question mark", &dm());
        assert_eq!(buffer, "hello world,?");
        apply_command_delta_to_buffer(&mut buffer, "new line", &dm());
        assert_eq!(buffer, "hello world,?\n");
        apply_command_delta_to_buffer(&mut buffer, "new paragraph", &dm());
        assert_eq!(buffer, "hello world,?\n\n\n");
    }

    #[test]
    fn delete_word_uses_the_buffer_word_semantics() {
        let mut buffer = "one two three".to_string();
        apply_command_delta_to_buffer(&mut buffer, "delete word", &dm());
        assert_eq!(buffer, "one two ");
        apply_command_delta_to_buffer(&mut buffer, "delete word", &dm());
        assert_eq!(buffer, "one ");
        // Deleting from an empty buffer stays empty.
        let mut empty = " ".to_string();
        apply_command_delta_to_buffer(&mut empty, "delete word", &dm());
        assert_eq!(empty, "");
    }

    #[test]
    fn delete_line_clears_the_trailing_line() {
        let mut buffer = "first\nsecond third".to_string();
        apply_command_delta_to_buffer(&mut buffer, "delete line", &dm());
        assert_eq!(buffer, "first\n");
        // No newline: the whole buffer empties.
        let mut single = "only line".to_string();
        apply_command_delta_to_buffer(&mut single, "delete line", &dm());
        assert_eq!(single, "");
    }

    #[test]
    fn target_app_actions_edit_nothing_during_a_session() {
        let mut buffer = "hello world".to_string();
        apply_command_delta_to_buffer(&mut buffer, "undo paste", &dm());
        assert_eq!(buffer, "hello world");
    }

    #[test]
    fn actions_apply_in_delta_order() {
        let mut buffer = "one two three".to_string();
        apply_command_delta_to_buffer(&mut buffer, "delete word comma delete word", &dm());
        assert_eq!(buffer, "one two ");
    }

    #[test]
    fn blank_or_unrecognized_delta_edits_nothing() {
        let mut buffer = "hello world".to_string();
        apply_command_delta_to_buffer(&mut buffer, "", &dm());
        apply_command_delta_to_buffer(&mut buffer, "   ", &dm());
        apply_command_delta_to_buffer(&mut buffer, "um eh nothing recognizable", &dm());
        assert_eq!(buffer, "hello world");
    }

    // -----------------------------------------------------------------
    // Proper-prefix holdback (streaming fragmentation).
    // -----------------------------------------------------------------

    #[test]
    fn held_prefix_len_returns_the_pinned_spans() {
        // A whole command never holds: it completes a phrase and applies.
        assert_eq!(held_prefix_len(" comma", &dm()), 0);
        // A partial word holds WITH its preceding separator (" com").
        assert_eq!(held_prefix_len(" com", &dm()), 4);
        // A whole word that only opens a two-word phrase holds.
        assert_eq!(held_prefix_len(" question", &dm()), 9);
        // A partial second word of a phrase holds the whole span.
        assert_eq!(held_prefix_len(" new l", &dm()), 6);
        // Words that prefix nothing hold nothing.
        assert_eq!(held_prefix_len(" um stuff", &dm()), 0);
    }

    #[test]
    fn held_prefix_len_edge_spans() {
        // "new" opens two phrases but completes none: held, separator
        // included.
        assert_eq!(held_prefix_len(" new", &dm()), 4);
        // A fragment with no separator before it holds just the token.
        assert_eq!(held_prefix_len("com", &dm()), 3);
        // A blank delta holds nothing.
        assert_eq!(held_prefix_len("  ", &dm()), 0);
        // Commands earlier in the delta do not stop the trailing hold.
        assert_eq!(held_prefix_len(" comma quest", &dm()), 6);
    }

    #[test]
    fn flush_command_prefix_len_grows_to_phrase_completion() {
        // The first word completes a phrase: consume exactly it (with its
        // preceding separator); the dictation beyond stays unconsumed.
        assert_eq!(flush_command_prefix_len(" comma and more", &dm()), 6);
        // A held phrase opener grows word by word until the phrase
        // completes, so a held " new l" resolves to the whole "new line".
        assert_eq!(flush_command_prefix_len(" new line next", &dm()), 9);
        // Longest phrase: "new paragraph" consumes both words.
        assert_eq!(
            flush_command_prefix_len(" new paragraph tail", &dm()),
            " new paragraph".len()
        );
        // A still-partial fragment at flush time is consumed whole and
        // then discarded by the grammar.
        assert_eq!(flush_command_prefix_len(" com", &dm()), 4);
        // Nothing to consume.
        assert_eq!(flush_command_prefix_len("", &dm()), 0);
        assert_eq!(flush_command_prefix_len("   ", &dm()), 0);
    }

    #[test]
    fn flush_command_prefix_len_returns_innocent_words_to_dictation() {
        // An ordinary word that merely STARTS like a command word is not a
        // command fragment: consume nothing, so the whole region flows back
        // as dictation instead of being eaten and discarded by the grammar.
        assert_eq!(flush_command_prefix_len(" computer science rocks", &dm()), 0);
        assert_eq!(flush_command_prefix_len(" periodical", &dm()), 0);
        // Still a command fragment: resolves through the grammar (exact or,
        // per the fuzzy contract, "perio" at distance 1 from "period").
        assert_eq!(flush_command_prefix_len(" comma please", &dm()), 6);
        assert_eq!(flush_command_prefix_len(" perio", &dm()), 6);
    }

    // -----------------------------------------------------------------
    // Duplicate symbol insert coalescing.
    // -----------------------------------------------------------------

    #[test]
    fn duplicate_symbol_inserts_coalesce() {
        let mut buffer = "hello world".to_string();
        apply_command_delta_to_buffer(&mut buffer, "comma", &dm());
        assert_eq!(buffer, "hello world,");
        // Repeating the identical symbol command across DELTAS (the
        // operator's old response to a command that LOOKED dead) still
        // coalesces to one.
        apply_command_delta_to_buffer(&mut buffer, "comma", &dm());
        assert_eq!(buffer, "hello world,");
        // Same-delta repeats are INTENTIONAL doubles ("comma comma" said
        // in one breath): both land.
        let mut twice = "x".to_string();
        apply_command_delta_to_buffer(&mut twice, "comma comma", &dm());
        assert_eq!(twice, "x,,");
        // Three in a row stack three.
        let mut triple = "x".to_string();
        apply_command_delta_to_buffer(&mut triple, "comma comma comma", &dm());
        assert_eq!(triple, "x,,,");
    }

    #[test]
    fn line_breaks_and_distinct_symbols_never_coalesce() {
        let mut buffer = "hello world".to_string();
        apply_command_delta_to_buffer(&mut buffer, "new line", &dm());
        apply_command_delta_to_buffer(&mut buffer, "new line", &dm());
        assert_eq!(buffer, "hello world\n\n");
        // "new line" then "comma" still lands the comma after the break.
        apply_command_delta_to_buffer(&mut buffer, "comma", &dm());
        assert_eq!(buffer, "hello world\n\n,");
        // Distinct symbols stack.
        let mut mixed = "x".to_string();
        apply_command_delta_to_buffer(&mut mixed, "comma question mark", &dm());
        assert_eq!(mixed, "x,?");
        // A period after a comma is a different symbol: both land.
        apply_command_delta_to_buffer(&mut mixed, "period", &dm());
        assert_eq!(mixed, "x,?.");
    }

    #[test]
    fn command_mode_fuzzy_single_word_near_miss_resolves() {
        // Command mode only: a single token within edit distance 1 of a
        // single-word phrase of at least 5 characters resolves to it. The
        // held modifier says "this is a command", so "coma" lands the comma
        // whisper refused to spell straight. Normal dictation never fuzzy
        // matches (pinned in text.rs: "a coma patient").
        let mut buffer = "hello".to_string();
        apply_command_delta_to_buffer(&mut buffer, "coma", &dm());
        assert_eq!(buffer, "hello,");
        // Distance 2 ("com" vs "comma") still does not fire.
        let mut far = "hello".to_string();
        apply_command_delta_to_buffer(&mut far, "com", &dm());
        assert_eq!(far, "hello");
        // Near-misses of phrases shorter than 5 characters never fire
        // either ("pas" is 2 edits from "paste", 3 from "hash").
        let mut none = "hello".to_string();
        apply_command_delta_to_buffer(&mut none, "pas", &dm());
        assert_eq!(none, "hello");
    }

    #[test]
    fn command_mode_everyday_words_are_not_gated() {
        // The utterance-final gate lives in the normal-dictation text pass
        // only: with the modifier held, an explicit everyday-word command
        // always inserts its symbol.
        let mut buffer = "hello".to_string();
        apply_command_delta_to_buffer(&mut buffer, "period star percent pipe", &dm());
        assert_eq!(buffer, "hello.*%|");
    }

    #[test]
    fn matrix_unification_adds_voice_phrases_and_extended_symbols() {
        // The DeleteWord unification: the voice phrases also parse in
        // command mode.
        assert_eq!(
            parse_command_transcript("scratch that", &dm()),
            vec![CommandAction::DeleteWord]
        );
        assert_eq!(
            parse_command_transcript("remove that", &dm()),
            vec![CommandAction::DeleteWord]
        );
        // ClearAll is a command-mode action now: clear, session continues.
        assert_eq!(
            parse_command_transcript("delete everything", &dm()),
            vec![CommandAction::ClearAll]
        );
        // "start over" left the default table: it is ordinary English, so
        // in command mode too it no longer parses (nothing fires).
        assert!(parse_command_transcript("start over", &dm()).is_empty());
        // Extended symbol set from the default matrix.
        assert_eq!(
            parse_command_transcript("open square bracket", &dm()),
            vec![CommandAction::Insert("[")]
        );
        assert_eq!(
            parse_command_transcript("at sign", &dm()),
            vec![CommandAction::Insert("@")]
        );
        assert_eq!(
            parse_command_transcript("vertical bar", &dm()),
            vec![CommandAction::Insert("|")]
        );
    }
}
