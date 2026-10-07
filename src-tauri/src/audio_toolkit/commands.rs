//! Command mode: parse a whole transcript as a sequence of editing commands.
//!
//! Command mode is a second recording trigger, separate from normal
//! dictation. Audio is captured exactly like push-to-talk, but at finalize
//! the transcript is treated as a command sequence instead of text to paste.
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
//! nothing unrecognized ever reaches the target app, so the user can speak
//! filler ("um", "eh", half-sentences) while issuing commands and nothing
//! stray is typed. A transcript with no recognized commands produces an
//! empty action list and nothing happens.
//!
//! Matching follows the phrase style of the spoken-punctuation normalizer
//! in [`super::text`]: phrases are matched whole (two-word entries before
//! any shorter rival, so "new paragraph" can never fire on "new"), tokens
//! are compared case-insensitively, and punctuation the model attached to a
//! token (a trailing comma or period) is stripped before comparison so
//! "undo," still reads as "undo". Unlike the normalizer, unmatched words are
//! dropped rather than kept, because here the transcript is a program, not
//! prose.

use crate::paste_tx::key_send::EditAction;

/// One parsed command-mode action, in transcript order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandAction {
    /// Insert literal text (punctuation, line breaks).
    Insert(&'static str),
    /// Delete the word before the caret.
    DeleteWord,
    /// Delete the current line.
    DeleteLine,
    /// Undo the last edit.
    Undo,
    /// Send a plain paste chord.
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

/// One executable step after coalescing. Consecutive [`CommandAction::Insert`]
/// values join into a single paste so "comma question mark" pastes ",?" once
/// instead of racing two clipboard transactions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutionStep {
    /// Paste this literal text through the paste path.
    PasteText(String),
    /// Send one editing chord.
    Edit(EditAction),
}

fn edit_action_for(action: &CommandAction) -> Option<EditAction> {
    match action {
        CommandAction::DeleteWord => Some(EditAction::DeleteLastWord),
        CommandAction::DeleteLine => Some(EditAction::DeleteLine),
        CommandAction::Undo => Some(EditAction::Undo),
        CommandAction::Paste => Some(EditAction::Paste),
        CommandAction::Insert(_) => None,
    }
}

/// Turn parsed actions into executable steps: consecutive inserts coalesce
/// into one paste, key actions stay separate, and transcript order is
/// preserved. An empty action list yields an empty plan (nothing happens).
pub fn plan_execution(actions: &[CommandAction]) -> Vec<ExecutionStep> {
    let mut steps: Vec<ExecutionStep> = Vec::new();
    let mut pending_text: Option<String> = None;

    for action in actions {
        match action {
            CommandAction::Insert(text) => {
                pending_text.get_or_insert_with(String::new).push_str(text);
            }
            other => {
                if let Some(text) = pending_text.take() {
                    steps.push(ExecutionStep::PasteText(text));
                }
                if let Some(edit) = edit_action_for(other) {
                    steps.push(ExecutionStep::Edit(edit));
                }
            }
        }
    }

    if let Some(text) = pending_text {
        steps.push(ExecutionStep::PasteText(text));
    }

    steps
}

/// Parse and plan in one call: transcript -> executable steps.
pub fn plan_command_transcript(transcript: &str) -> Vec<ExecutionStep> {
    plan_execution(&parse_command_transcript(transcript))
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

    #[test]
    fn consecutive_inserts_coalesce_into_one_paste() {
        let steps = plan_execution(&[
            CommandAction::Insert(","),
            CommandAction::Insert("?"),
            CommandAction::Insert("\n"),
        ]);
        assert_eq!(steps, vec![ExecutionStep::PasteText(",?\n".to_string())]);
    }

    #[test]
    fn inserts_surrounding_an_edit_split_at_the_edit() {
        let steps = plan_execution(&[
            CommandAction::Insert("."),
            CommandAction::Undo,
            CommandAction::Insert(","),
        ]);
        assert_eq!(
            steps,
            vec![
                ExecutionStep::PasteText(".".to_string()),
                ExecutionStep::Edit(EditAction::Undo),
                ExecutionStep::PasteText(",".to_string()),
            ]
        );
    }

    #[test]
    fn transcript_order_is_preserved_end_to_end() {
        let steps = plan_command_transcript(
            "hello comma question mark delete word undo new line paste period",
        );
        assert_eq!(
            steps,
            vec![
                ExecutionStep::PasteText(",?".to_string()),
                ExecutionStep::Edit(EditAction::DeleteLastWord),
                ExecutionStep::Edit(EditAction::Undo),
                ExecutionStep::PasteText("\n".to_string()),
                ExecutionStep::Edit(EditAction::Paste),
                ExecutionStep::PasteText(".".to_string()),
            ]
        );
    }

    #[test]
    fn empty_action_list_plans_nothing() {
        assert!(plan_execution(&[]).is_empty());
        assert!(plan_command_transcript("nothing recognizable here").is_empty());
    }

    #[test]
    fn key_actions_map_to_their_edit_chords() {
        assert_eq!(
            edit_action_for(&CommandAction::DeleteWord),
            Some(EditAction::DeleteLastWord)
        );
        assert_eq!(
            edit_action_for(&CommandAction::DeleteLine),
            Some(EditAction::DeleteLine)
        );
        assert_eq!(
            edit_action_for(&CommandAction::Undo),
            Some(EditAction::Undo)
        );
        assert_eq!(
            edit_action_for(&CommandAction::Paste),
            Some(EditAction::Paste)
        );
        assert_eq!(edit_action_for(&CommandAction::Insert("?")), None);
    }
}
