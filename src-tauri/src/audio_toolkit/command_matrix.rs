//! The command matrix: one editable phrase table feeding every command
//! consumer.
//!
//! Historically the spoken vocabulary lived in three independent
//! hard-coded tables (the spoken-punctuation rules, the voice-deletion
//! pattern, and the command-mode parser vocabulary), so a phrase added to
//! one never reached the others. The matrix is the single source: each
//! command is a [`CommandId`] with an editable phrase list
//! ([`CommandMatrixEntry`]), and [`compile_command_matrix`] derives every
//! consumer surface from it:
//!
//! * the punctuation regex + phrase-to-symbol map for
//!   [`super::text::normalize_spoken_punctuation`] (symbol-insert
//!   entries);
//! * the voice-deletion regex + phrase map for
//!   [`super::text::apply_voice_deletion`] (DeleteWord, DeleteLine,
//!   ClearAll entries, merged with the built-in "delete last N words"
//!   family);
//! * the parser table for [`super::commands::parse_command_transcript`]
//!   (every entry) with a matrix-derived `max_phrase_words`.
//!
//! All phrase lists are SORTED by word count descending before regex and
//! table construction: the regexes rely on leftmost-first alternation, and
//! user-edited phrase lists cannot be trusted to preserve longest-first
//! table order.
//!
//! The commands themselves are NOT user-definable (the operator edits
//! phrases per command); each variant maps to a fixed
//! [`super::commands::CommandAction`] through [`command_action`].

use std::collections::HashMap;
use std::sync::Arc;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use specta::Type;

use super::commands::CommandAction;

/// One command of the matrix. Thirty fixed commands; only their PHRASES
/// are editable. camelCase serde names are the persisted and TS contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum CommandId {
    Period,
    Comma,
    QuestionMark,
    Exclamation,
    Colon,
    Semicolon,
    Dash,
    NewLine,
    NewParagraph,
    AtSign,
    Hash,
    DollarSign,
    Percent,
    Star,
    Ampersand,
    Caret,
    OpenParen,
    CloseParen,
    OpenBracket,
    CloseBracket,
    OpenBrace,
    CloseBrace,
    Slash,
    Backslash,
    Pipe,
    DeleteWord,
    DeleteLine,
    ClearAll,
    Undo,
    Paste,
}

/// One matrix row: a command and its editable spoken phrases.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Type)]
pub struct CommandMatrixEntry {
    pub command: CommandId,
    pub phrases: Vec<String>,
}

/// The single CommandId-to-action mapping. Every symbol insert maps to
/// `Insert(literal)` with the fixed literal below (`Insert(&'static str)`
/// stays the payload type precisely because the symbol set is fixed);
/// DeleteWord/DeleteLine map to their existing variants; ClearAll maps to
/// the command-mode ClearAll variant; Undo and Paste map to their existing
/// inert variants.
pub fn command_action(command: CommandId) -> CommandAction {
    match command {
        CommandId::Period => CommandAction::Insert("."),
        CommandId::Comma => CommandAction::Insert(","),
        CommandId::QuestionMark => CommandAction::Insert("?"),
        CommandId::Exclamation => CommandAction::Insert("!"),
        CommandId::Colon => CommandAction::Insert(":"),
        CommandId::Semicolon => CommandAction::Insert(";"),
        CommandId::Dash => CommandAction::Insert("-"),
        CommandId::NewLine => CommandAction::Insert("\n"),
        CommandId::NewParagraph => CommandAction::Insert("\n\n"),
        CommandId::AtSign => CommandAction::Insert("@"),
        CommandId::Hash => CommandAction::Insert("#"),
        CommandId::DollarSign => CommandAction::Insert("$"),
        CommandId::Percent => CommandAction::Insert("%"),
        CommandId::Star => CommandAction::Insert("*"),
        CommandId::Ampersand => CommandAction::Insert("&"),
        CommandId::Caret => CommandAction::Insert("^"),
        CommandId::OpenParen => CommandAction::Insert("("),
        CommandId::CloseParen => CommandAction::Insert(")"),
        CommandId::OpenBracket => CommandAction::Insert("["),
        CommandId::CloseBracket => CommandAction::Insert("]"),
        CommandId::OpenBrace => CommandAction::Insert("{"),
        CommandId::CloseBrace => CommandAction::Insert("}"),
        CommandId::Slash => CommandAction::Insert("/"),
        CommandId::Backslash => CommandAction::Insert("\\"),
        CommandId::Pipe => CommandAction::Insert("|"),
        CommandId::DeleteWord => CommandAction::DeleteWord,
        CommandId::DeleteLine => CommandAction::DeleteLine,
        CommandId::ClearAll => CommandAction::ClearAll,
        CommandId::Undo => CommandAction::Undo,
        CommandId::Paste => CommandAction::Paste,
    }
}

/// The full default phrase table: today's vocabulary PLUS the extended
/// symbol set (at sign, hash, dollar sign, percent, star, ampersand,
/// caret, brackets, braces, slash, backslash, pipe), and the DeleteWord
/// unification (the voice phrases "scratch that" / "delete that" /
/// "remove that" become phrases of the same command whose command-mode
/// phrase is "delete word", so one phrase table feeds both surfaces).
pub fn default_command_matrix() -> Vec<CommandMatrixEntry> {
    use CommandId::*;
    [
        (Period, &["period", "full stop"][..]),
        (Comma, &["comma"]),
        (QuestionMark, &["question mark"]),
        (Exclamation, &["exclamation mark", "exclamation point"]),
        (Colon, &["colon"]),
        (Semicolon, &["semicolon"]),
        (Dash, &["dash"]),
        (NewLine, &["new line"]),
        (NewParagraph, &["new paragraph"]),
        (AtSign, &["at sign"]),
        (Hash, &["hash", "hash sign"]),
        (DollarSign, &["dollar sign"]),
        (Percent, &["percent", "percent sign"]),
        (Star, &["star", "asterisk"]),
        (Ampersand, &["ampersand"]),
        (Caret, &["caret"]),
        (OpenParen, &["open paren", "open parenthesis"]),
        (CloseParen, &["close paren", "close parenthesis"]),
        (OpenBracket, &["open bracket", "open square bracket"]),
        (CloseBracket, &["close bracket", "close square bracket"]),
        (OpenBrace, &["open brace", "open curly brace"]),
        (CloseBrace, &["close brace", "close curly brace"]),
        (Slash, &["slash", "forward slash"]),
        (Backslash, &["backslash"]),
        (Pipe, &["pipe", "vertical bar"]),
        (
            DeleteWord,
            &["delete word", "scratch that", "delete that", "remove that"],
        ),
        (DeleteLine, &["delete line"]),
        (
            ClearAll,
            &["delete everything", "scratch everything", "start over"],
        ),
        (Undo, &["undo"]),
        (Paste, &["paste"]),
    ]
    .into_iter()
    .map(|(command, phrases)| CommandMatrixEntry {
        command,
        phrases: phrases.iter().map(|phrase| phrase.to_string()).collect(),
    })
    .collect()
}

/// How a matched voice-deletion phrase acts on the transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VoiceDeletionKind {
    /// Remove the preceding word.
    Word,
    /// Clear the trailing line.
    Line,
    /// Discard the whole transcript (short-circuit every later pass).
    All,
}

/// The built-in "delete last N words" family. Parameterized grammar, so
/// it stays a fixed pattern fragment rather than a phrase-table entry
/// (spec F5 item 1: out of scope for the editable matrix).
pub(crate) const VOICE_DELETION_COUNT_FAMILY: &str =
    "delete last (?:one word|word|two words|three words|four words|five words|\
     six words|seven words|eight words|nine words|ten words|(?:[1-9]|10) words?)";

/// A never-matching pattern, for consumer surfaces whose phrase pool came
/// out empty (an empty alternation would match everywhere).
const NEVER_MATCH: &str = r"[^\s\S]";

/// Everything the three command consumers need, compiled once from an
/// entry list. Cheap to share: the regexes live behind an `Arc`.
pub struct CompiledCommandMatrix {
    /// Spoken-punctuation pass: word-boundary anchored, case-insensitive,
    /// with the optional trailing [,.]? the model may have attached.
    punctuation_pattern: Regex,
    /// Matched (normalized, lowercased) phrase -> inserted symbol.
    punctuation_replacements: HashMap<String, &'static str>,
    /// Voice-deletion pass: matrix phrases merged with the count family.
    voice_deletion_pattern: Regex,
    /// Matched phrase -> how it acts. Absent = the count family matched;
    /// the caller extracts N from the matched text.
    voice_deletion_kinds: HashMap<String, VoiceDeletionKind>,
    /// Command parser: normalized token vectors (sorted by word count
    /// descending) -> actions. Replaces the hard-coded parser vocabulary.
    pub(crate) parser: Vec<(Vec<String>, CommandAction)>,
    /// Longest phrase in the matrix, in words; bounds the parser's
    /// lookahead window and the holdback fragment search.
    pub(crate) max_phrase_words: usize,
}

impl CompiledCommandMatrix {
    /// Iterate the punctuation pattern's matches over `text` (word
    /// boundary anchored, case-insensitive, with the optional trailing
    /// [,.]? the model may have attached).
    pub(crate) fn punctuation_pattern_matches<'a>(
        &'a self,
        text: &'a str,
    ) -> regex::Matches<'a, 'a> {
        self.punctuation_pattern.find_iter(text)
    }

    /// The replacement for a spoken punctuation phrase match (already
    /// lowercased and stripped of the optional trailing [,.]?).
    pub(crate) fn punctuation_replacement(&self, phrase: &str) -> Option<&'static str> {
        self.punctuation_replacements.get(phrase).copied()
    }

    /// Iterate the voice-deletion pattern's matches over `text`.
    pub(crate) fn voice_deletion_pattern_matches<'a>(
        &'a self,
        text: &'a str,
    ) -> regex::Matches<'a, 'a> {
        self.voice_deletion_pattern.find_iter(text)
    }

    /// How a matched voice-deletion phrase acts; `None` means the built-in
    /// count family matched.
    pub(crate) fn voice_deletion_kind(&self, phrase: &str) -> Option<VoiceDeletionKind> {
        self.voice_deletion_kinds.get(phrase).copied()
    }
}

/// Normalize one phrase for storage and matching: lowercase, trim, and
/// collapse inner whitespace to single spaces.
pub fn normalize_phrase(phrase: &str) -> String {
    phrase
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Validate an edited matrix before persisting it. Rejects empty phrases,
/// phrases longer than 60 characters, and a phrase that already exists on
/// any command; every error names the offending command.
pub fn validate_matrix(entries: &[CommandMatrixEntry]) -> Result<(), String> {
    let mut seen: HashMap<String, String> = HashMap::new();
    for entry in entries {
        let command = format!("{:?}", entry.command);
        for phrase in &entry.phrases {
            let normalized = normalize_phrase(phrase);
            if normalized.is_empty() {
                return Err(format!("command {command} has an empty phrase"));
            }
            if normalized.chars().count() > 60 {
                return Err(format!(
                    "command {command} has a phrase longer than 60 characters: {normalized}"
                ));
            }
            if let Some(existing) = seen.get(&normalized) {
                return Err(format!(
                    "phrase '{normalized}' is already used by command {existing}"
                ));
            }
            seen.insert(normalized, command.clone());
        }
    }
    Ok(())
}

/// Normalize and validate an edited matrix for persistence (the write
/// path of the Settings editor): every phrase is normalized (lowercase,
/// trimmed, inner whitespace collapsed) and [`validate_matrix`] then
/// rejects empty entries, over-length phrases, and duplicates across
/// commands. `None` passes through unchanged (reset to the built-in
/// defaults).
pub fn normalize_and_validate_matrix(
    entries: Option<Vec<CommandMatrixEntry>>,
) -> Result<Option<Vec<CommandMatrixEntry>>, String> {
    let Some(entries) = entries else {
        return Ok(None);
    };
    let normalized: Vec<CommandMatrixEntry> = entries
        .into_iter()
        .map(|entry| CommandMatrixEntry {
            command: entry.command,
            phrases: entry.phrases.iter().map(|p| normalize_phrase(p)).collect(),
        })
        .collect();
    validate_matrix(&normalized)?;
    Ok(Some(normalized))
}

/// Sort a phrase list by word count descending (stable, so equal-length
/// phrases keep their table order). The regex consumers rely on
/// leftmost-first alternation, so the longest phrase must come first.
fn sorted_by_word_count_descending<T>(mut items: Vec<T>, words: impl Fn(&T) -> usize) -> Vec<T> {
    items.sort_by(|a, b| words(b).cmp(&words(a)));
    items
}

/// Compile an entry list into every consumer surface. Defensive against
/// imperfect stores: empty phrases are skipped (validation happens on
/// write), never panics.
pub fn compile_command_matrix(entries: &[CommandMatrixEntry]) -> CompiledCommandMatrix {
    // Normalize every phrase once; keep (phrase, action) pairs.
    let all: Vec<(String, CommandAction)> = entries
        .iter()
        .flat_map(|entry| {
            let action = command_action(entry.command);
            entry.phrases.iter().map(move |phrase| {
                let normalized = normalize_phrase(phrase);
                (normalized, action)
            })
        })
        .filter(|(phrase, _)| !phrase.is_empty())
        .collect();

    let word_count = |phrase: &str| phrase.split_whitespace().count();
    let sorted = sorted_by_word_count_descending(all, |(phrase, _)| word_count(phrase));

    // Punctuation pass: the symbol-insert entries.
    let punctuation: Vec<&(String, CommandAction)> = sorted
        .iter()
        .filter(|(_, action)| matches!(action, CommandAction::Insert(_)))
        .collect();
    let punctuation_alternation = punctuation
        .iter()
        .map(|(phrase, _)| regex::escape(phrase))
        .collect::<Vec<_>>()
        .join("|");
    let punctuation_pattern = Regex::new(&format!(
        r"(?i)\b(?:{})\b[,.]?",
        if punctuation_alternation.is_empty() {
            NEVER_MATCH
        } else {
            &punctuation_alternation
        }
    ))
    .unwrap();
    let punctuation_replacements = punctuation
        .iter()
        .map(|(phrase, action)| {
            let CommandAction::Insert(symbol) = action else {
                unreachable!("filtered to Insert above");
            };
            (phrase.clone(), *symbol)
        })
        .collect();

    // Voice-deletion pass: DeleteWord / DeleteLine / ClearAll entries
    // merged with the built-in count family. The family counts as three
    // words (its shortest form, "delete last word") for the sort.
    let mut voice: Vec<(String, Option<VoiceDeletionKind>)> = sorted
        .iter()
        .filter_map(|(phrase, action)| {
            let kind = match action {
                CommandAction::DeleteWord => VoiceDeletionKind::Word,
                CommandAction::DeleteLine => VoiceDeletionKind::Line,
                CommandAction::ClearAll => VoiceDeletionKind::All,
                _ => return None,
            };
            Some((phrase.clone(), Some(kind)))
        })
        .collect();
    voice.push((VOICE_DELETION_COUNT_FAMILY.to_string(), None));
    let voice = sorted_by_word_count_descending(voice, |(phrase, _)| {
        if phrase == VOICE_DELETION_COUNT_FAMILY {
            3
        } else {
            word_count(phrase)
        }
    });
    let voice_deletion_pattern = Regex::new(&format!(
        r"(?i)\b(?:{})\b",
        voice
            .iter()
            .map(|(phrase, _)| phrase.as_str())
            .collect::<Vec<_>>()
            .join("|")
    ))
    .unwrap();
    let voice_deletion_kinds = voice
        .iter()
        .filter_map(|(phrase, kind)| kind.map(|kind| (phrase.clone(), kind)))
        .collect();

    // Parser table: every entry, tokens split for token-wise matching.
    let parser: Vec<(Vec<String>, CommandAction)> = sorted
        .iter()
        .map(|(phrase, action)| {
            (
                phrase.split_whitespace().map(str::to_string).collect(),
                *action,
            )
        })
        .collect();
    let max_phrase_words = parser
        .iter()
        .map(|(tokens, _)| tokens.len())
        .max()
        .unwrap_or(0);

    CompiledCommandMatrix {
        punctuation_pattern,
        punctuation_replacements,
        voice_deletion_pattern,
        voice_deletion_kinds,
        parser,
        max_phrase_words,
    }
}

static DEFAULT_COMPILED: Lazy<Arc<CompiledCommandMatrix>> =
    Lazy::new(|| Arc::new(compile_command_matrix(&default_command_matrix())));

/// The compiled DEFAULT matrix, shared. The default path never rebuilds
/// regexes; only an edited matrix (Some(command_phrases)) compiles per
/// session / finalize call.
pub fn default_compiled_matrix() -> Arc<CompiledCommandMatrix> {
    Arc::clone(&DEFAULT_COMPILED)
}

/// Compile the matrix a settings store selects: the edited table when
/// present, the shared defaults when the setting is None. Never rebuilds
/// for the default path.
pub fn matrix_from_settings(settings: &crate::settings::AppSettings) -> Arc<CompiledCommandMatrix> {
    match settings.command_phrases.as_deref() {
        Some(entries) => Arc::new(compile_command_matrix(entries)),
        None => default_compiled_matrix(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_toolkit::commands::{
        apply_command_delta_to_buffer, held_prefix_len, parse_command_transcript,
    };
    use crate::audio_toolkit::text::{apply_voice_deletion, normalize_spoken_punctuation};

    fn dm() -> Arc<CompiledCommandMatrix> {
        default_compiled_matrix()
    }

    #[test]
    fn defaults_compile_with_every_surface_populated() {
        let matrix = dm();
        assert!(
            matrix.max_phrase_words == 3,
            "open square bracket is 3 words"
        );
        // Every default phrase lands in the parser table.
        let expected: usize = default_command_matrix()
            .iter()
            .map(|entry| entry.phrases.len())
            .sum();
        assert_eq!(matrix.parser.len(), expected);
        // The parser table is sorted by word count descending.
        let counts: Vec<usize> = matrix
            .parser
            .iter()
            .map(|(tokens, _)| tokens.len())
            .collect();
        let mut sorted_counts = counts.clone();
        sorted_counts.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(counts, sorted_counts);
    }

    #[test]
    fn every_default_phrase_parses_on_all_three_consumers() {
        let matrix = dm();
        for entry in default_command_matrix() {
            for phrase in &entry.phrases {
                let action = command_action(entry.command);
                // Parser surface: the phrase alone yields exactly its action.
                assert_eq!(
                    parse_command_transcript(phrase, &matrix),
                    vec![action],
                    "phrase: {phrase}"
                );

                match action {
                    CommandAction::Insert(symbol) => {
                        // Punctuation surface: "hello <phrase> world" converts
                        // to the symbol with the defined spacing semantics.
                        let converted =
                            normalize_spoken_punctuation(&format!("hello {phrase} world"), &matrix);
                        let expected = if is_sentence_ending(symbol) {
                            format!("hello{symbol} World")
                        } else if symbol == "-" {
                            "hello-world".to_string()
                        } else if symbol.starts_with('\n') {
                            format!("hello{symbol}world")
                        } else {
                            format!("hello{symbol} world")
                        };
                        assert_eq!(converted, expected, "phrase: {phrase}");
                    }
                    CommandAction::DeleteWord => {
                        let outcome =
                            apply_voice_deletion(&format!("hello world {phrase}"), &matrix);
                        assert_eq!(outcome.text, "hello", "phrase: {phrase}");
                        assert!(!outcome.cleared, "phrase: {phrase}");
                    }
                    CommandAction::DeleteLine => {
                        let outcome =
                            apply_voice_deletion(&format!("first\nsecond {phrase}"), &matrix);
                        assert_eq!(outcome.text, "first\n", "phrase: {phrase}");
                        assert!(!outcome.cleared, "phrase: {phrase}");
                    }
                    CommandAction::ClearAll => {
                        let outcome = apply_voice_deletion(&format!("hello {phrase}"), &matrix);
                        assert!(outcome.cleared, "phrase: {phrase}");
                    }
                    // Control commands act on the target app: never voice
                    // commands, so they must not touch the transcript.
                    CommandAction::Undo | CommandAction::Paste => {
                        let outcome = apply_voice_deletion(&format!("hello {phrase}"), &matrix);
                        assert_eq!(outcome.text, format!("hello {phrase}"), "phrase: {phrase}");
                        assert!(!outcome.cleared, "phrase: {phrase}");
                    }
                }
            }
        }
    }

    /// Only ".", "?" and "!" hand a capital to the next word; mirrors
    /// text.rs is_sentence_ending_punctuation for this test's expectations.
    fn is_sentence_ending(symbol: &str) -> bool {
        matches!(symbol, "." | "?" | "!")
    }

    #[test]
    fn default_matrix_three_word_phrase_wins() {
        let matrix = dm();
        // The three-word OpenBracket phrase fires; "open bracket" alone
        // never leaves strays behind.
        assert_eq!(
            normalize_spoken_punctuation("open square bracket", &matrix),
            "["
        );
        assert_eq!(
            normalize_spoken_punctuation("open square bracket hello", &matrix),
            "[ hello"
        );
        assert_eq!(
            parse_command_transcript("open square bracket", &matrix),
            vec![CommandAction::Insert("[")]
        );
    }

    #[test]
    fn phrase_lists_are_sorted_longest_first_so_user_order_cannot_break_precedence() {
        // Explicitly CUSTOM matrix with the SHORT phrase listed FIRST:
        // unsorted insertion order would let "open" match before "open
        // square bracket" and produce "(" plus stray words.
        let custom = vec![CommandMatrixEntry {
            command: CommandId::OpenBracket,
            phrases: vec!["open".to_string(), "open square bracket".to_string()],
        }];
        let matrix = compile_command_matrix(&custom);

        assert_eq!(
            normalize_spoken_punctuation("open square bracket", &matrix),
            "["
        );
        assert_eq!(
            parse_command_transcript("open square bracket", &matrix),
            vec![CommandAction::Insert("[")]
        );
        // The short phrase still works alone.
        assert_eq!(
            normalize_spoken_punctuation("open hello", &matrix),
            "[ hello"
        );
    }

    #[test]
    fn new_symbols_keep_the_space_after_the_insert() {
        // Colon-style spacing: every symbol except line breaks and the
        // dash retains the following space.
        let matrix = dm();
        assert_eq!(
            normalize_spoken_punctuation("open paren hello", &matrix),
            "( hello"
        );
        assert_eq!(
            normalize_spoken_punctuation("items colon one two", &matrix),
            "items: one two"
        );
        // Line breaks and the dash stay glued.
        assert_eq!(
            normalize_spoken_punctuation("twenty dash five", &matrix),
            "twenty-five"
        );
        assert_eq!(
            normalize_spoken_punctuation("done full stop new line next", &matrix),
            "done.\nNext"
        );
    }

    #[test]
    fn validate_matrix_rejects_bad_entries_naming_the_command() {
        let valid = default_command_matrix();
        assert!(validate_matrix(&valid).is_ok());

        let empty = vec![CommandMatrixEntry {
            command: CommandId::Comma,
            phrases: vec!["   ".to_string()],
        }];
        let err = validate_matrix(&empty).unwrap_err();
        assert!(err.contains("Comma"), "{err}");
        assert!(err.contains("empty"), "{err}");

        let too_long = vec![CommandMatrixEntry {
            command: CommandId::Paste,
            phrases: vec!["x".repeat(61)],
        }];
        let err = validate_matrix(&too_long).unwrap_err();
        assert!(err.contains("Paste"), "{err}");
        assert!(err.contains("60"), "{err}");

        let duplicate = vec![
            CommandMatrixEntry {
                command: CommandId::Comma,
                phrases: vec!["comma".to_string()],
            },
            CommandMatrixEntry {
                command: CommandId::Period,
                phrases: vec!["comma".to_string()],
            },
        ];
        let err = validate_matrix(&duplicate).unwrap_err();
        assert!(err.contains("'comma'"), "{err}");
        assert!(err.contains("Comma"), "{err}");

        // 60 characters exactly is fine; normalization lowercases and
        // collapses whitespace before the checks.
        let edge = vec![CommandMatrixEntry {
            command: CommandId::Comma,
            phrases: vec!["Kohma   Kohma".to_string()],
        }];
        assert!(validate_matrix(&edge).is_ok());
        assert_eq!(normalize_phrase("  Kohma   Kohma "), "kohma kohma");
    }

    #[test]
    fn update_path_normalizes_phrases_and_round_trips_validation() {
        use super::normalize_and_validate_matrix;

        // None passes through (reset to defaults).
        assert_eq!(normalize_and_validate_matrix(None), Ok(None));

        // Phrases are normalized before validation and storage.
        let edited = vec![CommandMatrixEntry {
            command: CommandId::Comma,
            phrases: vec!["  Kohma   KOHMA ".to_string()],
        }];
        let stored = normalize_and_validate_matrix(Some(edited))
            .unwrap()
            .unwrap();
        assert_eq!(stored[0].phrases, vec!["kohma kohma".to_string()]);
        // The stored form compiles and the phrase reaches every consumer.
        let matrix = compile_command_matrix(&stored);
        assert_eq!(
            parse_command_transcript("kohma kohma", &matrix),
            vec![CommandAction::Insert(",")]
        );

        // A phrase that normalizes to empty is rejected, naming the command.
        let empty = vec![CommandMatrixEntry {
            command: CommandId::Paste,
            phrases: vec!["   ".to_string()],
        }];
        let err = normalize_and_validate_matrix(Some(empty)).unwrap_err();
        assert!(err.contains("Paste") && err.contains("empty"), "{err}");

        // Duplicates surface through the same path (case/whitespace
        // variants collide after normalization).
        let duplicate = vec![
            CommandMatrixEntry {
                command: CommandId::Comma,
                phrases: vec!["Kohma".to_string()],
            },
            CommandMatrixEntry {
                command: CommandId::Period,
                phrases: vec!["  kohma ".to_string()],
            },
        ];
        let err = normalize_and_validate_matrix(Some(duplicate)).unwrap_err();
        assert!(err.contains("'kohma'") && err.contains("Comma"), "{err}");

        // The full default table passes its own write path unchanged.
        assert!(normalize_and_validate_matrix(Some(default_command_matrix())).is_ok());
    }

    #[test]
    fn command_mode_clear_all_clears_the_buffer_and_the_session_continues() {
        let matrix = dm();
        let mut buffer = "one two three".to_string();
        let removed = apply_command_delta_to_buffer(&mut buffer, "delete everything", &matrix);
        assert_eq!(buffer, "");
        assert_eq!(removed, None, "clear-all reports no removed chip");

        // The session continues: further commands apply on the empty buffer.
        apply_command_delta_to_buffer(&mut buffer, "comma", &matrix);
        assert_eq!(buffer, ",");
    }

    #[test]
    fn delete_word_unification_adds_voice_phrases_to_command_mode() {
        let matrix = dm();
        for phrase in ["scratch that", "delete that", "remove that", "delete word"] {
            let mut buffer = "one two three".to_string();
            let removed = apply_command_delta_to_buffer(&mut buffer, phrase, &matrix);
            assert_eq!(buffer, "one two ", "phrase: {phrase}");
            assert_eq!(removed, Some("three".to_string()), "phrase: {phrase}");
        }
    }

    #[test]
    fn holdback_and_flush_use_the_matrix_vocabulary() {
        let matrix = dm();
        // Three-word phrases can now be held mid-phrase.
        assert_eq!(held_prefix_len(" open", &matrix), 5);
        assert_eq!(held_prefix_len(" open squ", &matrix), 9);
        assert_eq!(held_prefix_len(" comma", &matrix), 0);
        // Custom phrase lists feed the holdback too.
        let custom = compile_command_matrix(&[CommandMatrixEntry {
            command: CommandId::Comma,
            phrases: vec!["kohma kohma".to_string()],
        }]);
        assert_eq!(held_prefix_len(" kohma", &custom), 6);
        assert_eq!(held_prefix_len(" kohma koh", &custom), 10);
        // A delta that completes the phrase skips the completing window
        // and holds the trailing token that opens a fresh instance of it
        // (proper-prefix semantics: a completing sequence never holds).
        assert_eq!(held_prefix_len(" kohma kohma", &custom), 6);
    }

    #[test]
    fn empty_matrix_compiles_to_inert_surfaces() {
        let matrix = compile_command_matrix(&[]);
        assert_eq!(matrix.max_phrase_words, 0);
        assert!(parse_command_transcript("comma", &matrix).is_empty());
        assert_eq!(
            normalize_spoken_punctuation("hello comma world", &matrix),
            "hello comma world"
        );
        // Matrix phrases are gone, so nothing deletes; the built-in count
        // family survives even an empty matrix.
        let outcome = apply_voice_deletion("hello scratch that", &matrix);
        assert_eq!(outcome.text, "hello scratch that");
        let outcome = apply_voice_deletion("hello delete last two words", &matrix);
        assert_eq!(outcome.text, "");
    }
}
