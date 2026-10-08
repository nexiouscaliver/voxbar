use super::command_matrix::{CompiledCommandMatrix, VoiceDeletionKind};
use super::commands::is_coalescible_symbol;
use natural::phonetics::soundex;
use once_cell::sync::Lazy;
use regex::Regex;
use strsim::levenshtein;

/// Builds an n-gram string by cleaning and concatenating words
///
/// Strips punctuation from each word, lowercases, and joins without spaces.
/// This allows matching "Charge B" against "ChargeBee".
fn build_ngram(words: &[&str]) -> String {
    words
        .iter()
        .map(|w| build_match_key(w))
        .collect::<Vec<_>>()
        .concat()
}

fn build_match_key(word: &str) -> String {
    word.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

struct CustomWordMatchKey {
    word_index: usize,
    key: String,
}

fn build_custom_word_match_keys(word: &str, word_index: usize) -> Vec<CustomWordMatchKey> {
    let primary_key = build_match_key(word);
    let mut keys = Vec::with_capacity(2);

    // The fallback matcher is intentionally limited to ASCII terms. Its
    // whitespace tokenization and Soundex scoring are not suitable for CJK
    // scripts. Unicode custom words remain available to models that accept
    // them as native decode prompts; they are simply skipped by this fallback.
    if is_supported_fuzzy_key(&primary_key) {
        keys.push(CustomWordMatchKey {
            word_index,
            key: primary_key.clone(),
        });
    }

    if word.contains('&') {
        let expanded_key = build_match_key(&word.replace('&', " and "));
        if is_supported_fuzzy_key(&expanded_key) && expanded_key != primary_key {
            keys.push(CustomWordMatchKey {
                word_index,
                key: expanded_key,
            });
        }
    }

    keys
}

fn is_supported_fuzzy_key(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric())
}

fn supports_soundex(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|c| c.is_ascii_alphabetic())
}

/// Finds the best matching custom word for a candidate string
///
/// Uses Levenshtein distance and Soundex phonetic matching to find
/// the best match above the given threshold.
///
/// # Arguments
/// * `candidate` - The cleaned/lowercased candidate string to match
/// * `custom_words` - Original custom words (for returning the replacement)
/// * `custom_word_match_keys` - Normalized custom-word keys for comparison
/// * `threshold` - Maximum similarity score to accept
///
/// # Returns
/// The best matching custom word and its score, if any match was found
fn find_best_match<'a>(
    candidate: &str,
    custom_words: &'a [String],
    custom_word_match_keys: &[CustomWordMatchKey],
    threshold: f64,
) -> Option<(&'a String, f64)> {
    if !is_supported_fuzzy_key(candidate) || candidate.chars().count() > 50 {
        return None;
    }

    let mut best_match: Option<&String> = None;
    let mut best_score = f64::MAX;

    for custom_word_key in custom_word_match_keys {
        // Skip if lengths are too different (optimization + prevents over-matching)
        // Use percentage-based check: max 25% length difference (prevents n-grams from
        // matching significantly shorter custom words, e.g., "openaigpt" vs "openai")
        let candidate_len = candidate.chars().count();
        let custom_word_len = custom_word_key.key.chars().count();
        let len_diff = candidate_len.abs_diff(custom_word_len) as f64;
        let max_len = candidate_len.max(custom_word_len) as f64;
        let max_allowed_diff = (max_len * 0.25).max(2.0); // At least 2 chars difference allowed
        if len_diff > max_allowed_diff {
            continue;
        }

        // Calculate Levenshtein distance (normalized by length)
        let levenshtein_dist = levenshtein(candidate, &custom_word_key.key);
        let levenshtein_score = if max_len > 0.0 {
            levenshtein_dist as f64 / max_len
        } else {
            1.0
        };

        // Soundex is an English/ASCII phonetic algorithm. Numeric terms can
        // still use edit distance, but must not receive a phonetic boost.
        let phonetic_match = supports_soundex(candidate)
            && supports_soundex(&custom_word_key.key)
            && soundex(candidate, &custom_word_key.key);

        // Combine scores: favor phonetic matches, but also consider string similarity
        let combined_score = if phonetic_match {
            levenshtein_score * 0.3 // Give significant boost to phonetic matches
        } else {
            levenshtein_score
        };

        // Accept if the score is good enough (configurable threshold)
        if combined_score < threshold && combined_score < best_score {
            best_match = Some(&custom_words[custom_word_key.word_index]);
            best_score = combined_score;
        }
    }

    best_match.map(|m| (m, best_score))
}

/// Applies custom word corrections to transcribed text using fuzzy matching
///
/// This function corrects words in the input text by finding the best matches
/// from a list of custom words using a combination of:
/// - Levenshtein distance for string similarity
/// - Soundex phonetic matching for pronunciation similarity
/// - N-gram matching for multi-word speech artifacts (e.g., "Charge B" -> "ChargeBee")
///
/// # Arguments
/// * `text` - The input text to correct
/// * `custom_words` - List of custom words to match against
/// * `threshold` - Maximum similarity score to accept (0.0 = exact match, 1.0 = any match)
///
/// # Returns
/// The corrected text with custom words applied
pub fn apply_custom_words(text: &str, custom_words: &[String], threshold: f64) -> String {
    if custom_words.is_empty() {
        return text.to_string();
    }

    // Pre-compute normalized comparison keys to avoid repeated allocations.
    let custom_word_match_keys: Vec<CustomWordMatchKey> = custom_words
        .iter()
        .enumerate()
        .flat_map(|(index, word)| build_custom_word_match_keys(word, index))
        .collect();

    // Line-preserving: the correction runs per line, so an n-gram can never
    // consume the first word of the next line across a line break, and the
    // breaks themselves survive (a split_whitespace rebuild would flatten
    // them). This pass runs on the DEFAULT configuration (the custom-words
    // seed is non-empty out of the box), so it must not destroy layout.
    text.split('\n')
        .map(|line| {
            apply_custom_words_in_line(line, custom_words, &custom_word_match_keys, threshold)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn apply_custom_words_in_line(
    line: &str,
    custom_words: &[String],
    custom_word_match_keys: &[CustomWordMatchKey],
    threshold: f64,
) -> String {
    let words: Vec<&str> = line.split_whitespace().collect();
    let mut result = Vec::new();
    let mut i = 0;

    while i < words.len() {
        let mut best_match: Option<(usize, &String, f64)> = None;

        // Consider n-grams up to three words and choose the closest match. A
        // longest-first match can consume a following ordinary word when both
        // candidates happen to share a Soundex code (for example,
        // "Charge B, che" matching "ChargeBee").
        for n in (1..=3).rev() {
            if i + n > words.len() {
                continue;
            }

            let ngram_words = &words[i..i + n];
            // Do not consume across a punctuation boundary. In
            // "Charge B, che", the comma closes the candidate at "B,".
            if ngram_words[..n.saturating_sub(1)]
                .iter()
                .any(|word| !extract_punctuation(word).1.is_empty())
            {
                continue;
            }
            let ngram = build_ngram(ngram_words);

            if let Some((replacement, score)) =
                find_best_match(&ngram, custom_words, custom_word_match_keys, threshold)
            {
                let is_better = best_match
                    .as_ref()
                    .is_none_or(|(_, _, best_score)| score < *best_score);
                if is_better {
                    best_match = Some((n, replacement, score));
                }
            }
        }

        if let Some((n, replacement, _)) = best_match {
            let ngram_words = &words[i..i + n];
            // Extract punctuation from first and last words of the n-gram.
            let (prefix, _) = extract_punctuation(ngram_words[0]);
            let (_, suffix) = extract_punctuation(ngram_words[n - 1]);

            // Preserve case from first word.
            let corrected = preserve_case_pattern(ngram_words[0], replacement);

            result.push(format!("{}{}{}", prefix, corrected, suffix));
            i += n;
        } else {
            result.push(words[i].to_string());
            i += 1;
        }
    }

    result.join(" ")
}

/// Preserves the case pattern of the original word when applying a replacement
fn preserve_case_pattern(original: &str, replacement: &str) -> String {
    if original.chars().all(|c| c.is_uppercase()) {
        replacement.to_uppercase()
    } else if original.chars().next().is_some_and(|c| c.is_uppercase()) {
        let mut chars: Vec<char> = replacement.chars().collect();
        if let Some(first_char) = chars.get_mut(0) {
            *first_char = first_char.to_uppercase().next().unwrap_or(*first_char);
        }
        chars.into_iter().collect()
    } else {
        replacement.to_string()
    }
}

/// Extracts punctuation prefix and suffix from a word
fn extract_punctuation(word: &str) -> (&str, &str) {
    // String slices use byte offsets. Derive both boundaries from char_indices
    // so multibyte punctuation such as `。` and `「」` can never be split.
    let prefix_end = word
        .char_indices()
        .find(|(_, c)| c.is_alphanumeric())
        .map(|(index, _)| index)
        .unwrap_or(word.len());
    let suffix_start = word
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_alphanumeric())
        .map(|(index, c)| index + c.len_utf8())
        .unwrap_or(0);

    let prefix = if prefix_end > 0 {
        &word[..prefix_end]
    } else {
        ""
    };

    let suffix = if suffix_start < word.len() {
        &word[suffix_start..]
    } else {
        ""
    };

    (prefix, suffix)
}

/// Evidence for the language of the text being cleaned.
///
/// This intentionally describes the transcription output, not Handy's UI
/// language. Unknown output languages fail closed: built-in filler removal is
/// skipped rather than applying a language profile speculatively.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputLanguageEvidence {
    UserSelected(String),
    ModelConstrained(String),
    /// The transcription model itself identified the language (audio-based
    /// LID, e.g. Whisper in auto mode).
    ModelDetected(String),
    /// Detected from the transcribed text with high confidence, constrained to
    /// the model's supported languages. Weakest accepted evidence.
    TextDetected(String),
    TranslatedToEnglish,
    Unknown,
}

impl OutputLanguageEvidence {
    pub(crate) fn language(&self) -> Option<&str> {
        match self {
            Self::UserSelected(language)
            | Self::ModelConstrained(language)
            | Self::ModelDetected(language)
            | Self::TextDetected(language) => Some(language),
            Self::TranslatedToEnglish => Some("en"),
            Self::Unknown => None,
        }
    }
}

/// Filler tokens that are not lexical words in any language Handy's models can
/// output, so removing them cannot corrupt text regardless of the (possibly
/// unknown) output language. Kept deliberately conservative: anything that is a
/// real word somewhere ("um" pt/de, "ha" es, "ah"/"eh" interjections, "mm"
/// millimetres) belongs in the language-gated lists instead.
const UNIVERSAL_FILLER_WORDS: &[&str] = &[
    "uh", "uhm", "umm", "uhh", "uhhh", "ehh", "ehm", "ahm", "hmm", "hm", "mmm", "хм", "ммм",
];

/// Filler words that are only safe to remove with evidence for the output
/// language, because the same token is a real word elsewhere (e.g. Portuguese
/// "um" = "a/an", German "um" = "at/around", Spanish "ha" = "has").
fn gated_filler_words_for_language(lang: &str) -> &'static [&'static str] {
    let base_lang = lang.split(&['-', '_'][..]).next().unwrap_or(lang);

    match base_lang {
        "en" => &["um", "ah", "eh"],
        "de" => &["äh", "ähm"],
        "fr" => &["euh"],
        _ => &[],
    }
}

/// Runs of spaces/tabs (any whitespace except newlines). Newline runs are
/// layout the operator spoke ("new line" / "new paragraph"), so they are
/// never collapsed by the whitespace cleanup.
static MULTI_SPACE_PATTERN: Lazy<Regex> = Lazy::new(|| Regex::new(r"[^\S\n]{2,}").unwrap());

/// Collapses repeated words (3+ repetitions) to a single instance.
/// E.g., "wh wh wh wh" -> "wh", "I I I I" -> "I"
///
/// Line-preserving: the repetition collapse runs per line, so "\n" and "\n\n"
/// survive instead of being flattened by the word rebuild.
fn collapse_stutters(text: &str) -> String {
    text.split('\n')
        .map(collapse_stutters_in_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn collapse_stutters_in_line(line: &str) -> String {
    let words: Vec<&str> = line.split_whitespace().collect();
    if words.is_empty() {
        return line.to_string();
    }

    let mut result: Vec<&str> = Vec::new();
    let mut i = 0;

    while i < words.len() {
        let word = words[i];
        let word_lower = word.to_lowercase();

        if word_lower.chars().all(|c| c.is_alphabetic()) {
            // Count consecutive repetitions (case-insensitive)
            let mut count = 1;
            while i + count < words.len() && words[i + count].to_lowercase() == word_lower {
                count += 1;
            }

            // If 3+ repetitions, collapse to single instance
            if count >= 3 {
                result.push(word);
                i += count;
            } else {
                result.push(word);
                i += 1;
            }
        } else {
            result.push(word);
            i += 1;
        }
    }

    result.join(" ")
}

/// Whether a word appended to `kept` would open a sentence: nothing but
/// whitespace so far, or the last visible character ends a sentence.
fn opens_sentence(kept: &str) -> bool {
    kept.trim_end()
        .chars()
        .next_back()
        .is_none_or(|c| matches!(c, '.' | '!' | '?' | '…'))
}

/// Appends `segment` to `kept`. While `capital_owed` is set, the first
/// alphanumeric character of `segment` is uppercased and the debt is settled.
fn push_restoring_capital(kept: &mut String, segment: &str, capital_owed: &mut bool) {
    if *capital_owed {
        if let Some((index, first)) = segment.char_indices().find(|(_, c)| c.is_alphanumeric()) {
            *capital_owed = false;
            kept.push_str(&segment[..index]);
            kept.extend(first.to_uppercase());
            kept.push_str(&segment[index + first.len_utf8()..]);
            return;
        }
    }
    kept.push_str(segment);
}

/// Deletes every match of one filler pattern. A capitalized filler that opened
/// a sentence hands its capital to the word that takes its place, so
/// "Um, so I think" becomes "So I think" rather than "so I think".
fn remove_filler_matches(text: &str, pattern: &Regex) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut resume = 0;
    let mut capital_owed = false;

    for filler in pattern.find_iter(text) {
        push_restoring_capital(&mut kept, &text[resume..filler.start()], &mut capital_owed);
        let capitalized = filler.as_str().starts_with(char::is_uppercase);
        capital_owed |= capitalized && opens_sentence(&kept);
        resume = filler.end();
    }
    push_restoring_capital(&mut kept, &text[resume..], &mut capital_owed);

    kept
}

/// Removes filler words from transcription output when enabled.
///
/// Built-in removal is two-tiered: [`UNIVERSAL_FILLER_WORDS`] apply regardless
/// of language evidence, while [`gated_filler_words_for_language`] tokens are
/// only removed when the output language is known. A custom list is an
/// explicit user override and replaces both tiers without requiring language
/// evidence. `Some(empty vec)` disables removal, preserving the legacy
/// power-user setting. The master toggle takes precedence over both built-in
/// and custom lists.
///
/// # Arguments
/// * `text` - The raw transcription text to filter
/// * `language` - Evidence for the language of the transcription output
/// * `custom_filler_words` - Optional user-provided filler word list. `Some(vec)` overrides
///   language defaults; `Some(empty vec)` disables filtering; `None` uses language defaults.
/// * `enabled` - Whether filler-word removal is enabled
///
/// # Returns
/// The text with configured filler words removed
pub fn remove_filler_words(
    text: &str,
    language: &OutputLanguageEvidence,
    custom_filler_words: &Option<Vec<String>>,
    enabled: bool,
) -> String {
    if !enabled {
        return text.to_string();
    }

    // Build filler patterns from custom list or the built-in tiers
    let patterns: Vec<Regex> = match custom_filler_words {
        Some(words) => words
            .iter()
            .filter_map(|word| Regex::new(&format!(r"(?i)\b{}\b[,.]?", regex::escape(word))).ok())
            .collect(),
        None => UNIVERSAL_FILLER_WORDS
            .iter()
            .chain(
                language
                    .language()
                    .map(gated_filler_words_for_language)
                    .unwrap_or_default(),
            )
            .map(|word| Regex::new(&format!(r"(?i)\b{}\b[,.]?", regex::escape(word))).unwrap())
            .collect(),
    };

    // Remove filler words
    let mut filtered = text.to_string();
    for pattern in &patterns {
        filtered = remove_filler_matches(&filtered, pattern);
    }

    filtered
}

/// Whether the inserted mark ends a sentence and owes the next word a
/// capital.
fn is_sentence_ending_punctuation(mark: &str) -> bool {
    matches!(mark, "." | "?" | "!")
}

/// Trims trailing spaces/tabs so inserted punctuation attaches to the
/// preceding word.
fn trim_trailing_spaces(text: &mut String) {
    while text.ends_with([' ', '\t']) {
        text.pop();
    }
}

/// Converts standalone spoken punctuation tokens into real punctuation.
///
/// The vocabulary is the command matrix's symbol-insert entries (compiled
/// by [`super::command_matrix`]): "full stop" and "period" become ".",
/// "comma" becomes ",", "question mark" becomes "?", the extended symbol
/// set (at sign, brackets, slash, ...) inserts its literal, "new line"
/// becomes a line break, "new paragraph" a blank line, and "dash" a plain
/// ASCII hyphen. Matching is case-insensitive, word-boundary anchored, and
/// phrase-aware: the matched token is consumed and the word after
/// sentence-ending punctuation (".", "?", "!") is capitalized. A symbol
/// whose mark is already at the kept tail (the model wrote the mark AND the
/// command word) coalesces instead of doubling. All other
/// text, including existing newlines, is preserved byte-for-byte.
pub fn normalize_spoken_punctuation(text: &str, matrix: &CompiledCommandMatrix) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut resume = 0;
    let mut capital_owed = false;
    // Set after a hyphen or line break: the whitespace that followed the
    // spoken token is dropped so "twenty dash five" joins into
    // "twenty-five" and "new line" starts the next line cleanly.
    let mut skip_leading_space = false;

    for token in matrix.punctuation_pattern_matches(text) {
        // Strip the optional trailing [,.]? the pattern may have consumed so
        // the lookup key is the pure spoken phrase.
        let phrase = token.as_str().trim_end_matches([',', '.']);
        let Some(replacement) = matrix.punctuation_replacement(&phrase.to_lowercase()) else {
            continue;
        };

        let mut span = &text[resume..token.start()];
        if skip_leading_space {
            span = span.trim_start_matches([' ', '\t']);
        }
        skip_leading_space = false;
        push_restoring_capital(&mut kept, span, &mut capital_owed);

        trim_trailing_spaces(&mut kept);
        // The model often writes BOTH the literal mark and the command word
        // ("hello, comma world"): when the kept tail already ends with the
        // exact symbol, the replacement coalesces into it instead of
        // appending a second mark. Scoped to single symbols through
        // is_coalescible_symbol, so line-break inserts ("\n", "\n\n") keep
        // stacking and the trailing-space trim never touches a newline.
        let already_marked =
            is_coalescible_symbol(replacement) && kept.ends_with(replacement);
        if !already_marked {
            kept.push_str(replacement);
        }
        if is_sentence_ending_punctuation(replacement) {
            capital_owed = true;
        }
        if replacement.starts_with('\n') || replacement == "-" {
            skip_leading_space = true;
        }
        resume = token.end();
    }

    let mut tail = &text[resume..];
    if skip_leading_space {
        tail = tail.trim_start_matches([' ', '\t']);
    }
    push_restoring_capital(&mut kept, tail, &mut capital_owed);

    kept
}

/// Openers that mark a transcript as a question when it is the first word.
const INTERROGATIVE_OPENERS: &[&str] = &[
    "what", "why", "how", "when", "who", "where", "which", "is", "are", "do", "does", "can",
    "could", "would", "should", "will",
];

fn starts_with_interrogative(text: &str) -> bool {
    text.split_whitespace().next().is_some_and(|first| {
        let key: String = first
            .chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(|c| c.to_lowercase())
            .collect();
        INTERROGATIVE_OPENERS.contains(&key.as_str())
    })
}

/// Appends terminal punctuation when a transcript does not end with any.
///
/// If the final character is alphanumeric, "?" is appended when the first
/// word is an interrogative ([`INTERROGATIVE_OPENERS`]), otherwise ".".
/// Existing terminal punctuation is never doubled and empty/whitespace-only
/// text is returned unchanged. The check runs on the trimmed text so a
/// trailing space cannot swallow the appended mark (the trailing whitespace
/// itself is dropped, matching the downstream trim in
/// [`normalize_transcription_output`]). A trailing newline run is layout the
/// operator spoke, so the mark is inserted BEFORE it: "hello\n" becomes
/// "hello.\n" and "para one\n\n" becomes "para one.\n\n".
pub fn apply_terminal_punctuation(text: &str) -> String {
    let body_end = text.trim_end_matches('\n').len();
    let (body, newline_run) = text.split_at(body_end);
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return text.to_string();
    }

    if !trimmed
        .chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric())
    {
        return text.to_string();
    }

    let mark = if starts_with_interrogative(trimmed) {
        "?"
    } else {
        "."
    };

    format!("{trimmed}{mark}{newline_run}")
}

/// Outcome of applying voice deletion commands to a transcript.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VoiceDeletionOutcome {
    /// The text with every matched command consumed and its deletion applied.
    pub text: String,
    /// True when a "delete everything" style command discarded the whole
    /// transcript (including anything spoken after the command). The pipeline
    /// then skips every later text pass and pastes nothing.
    pub cleared: bool,
}

/// The voice-deletion vocabulary is the command matrix's DeleteWord /
/// DeleteLine / ClearAll entries merged with the built-in "delete last N
/// words" family, compiled by [`super::command_matrix`]. Word-boundary
/// anchored and case-insensitive like the spoken-punctuation pattern, so
/// nothing fires inside a larger word ("deleted", "underscratch") and
/// phrases always match whole. The digit form accepts 1 through 10 only;
/// "delete last 0 words" and "delete last 99 words" are not commands and
/// stay verbatim.

/// Word count for a matched "delete last <count> words" command. The pattern
/// guarantees the count token is one of the accepted forms.
fn voice_deletion_word_count(phrase: &str) -> usize {
    let count_word = phrase.split_whitespace().nth(2).unwrap_or_default();
    if let Ok(number) = count_word.parse::<usize>() {
        return number;
    }
    match count_word {
        "word" | "one" => 1,
        "two" => 2,
        "three" => 3,
        "four" => 4,
        "five" => 5,
        "six" => 6,
        "seven" => 7,
        "eight" => 8,
        "nine" => 9,
        _ => 10,
    }
}

/// Removes the last `count` whitespace-delimited words from the end of
/// `text`, deleting fewer when the text runs out. A word is any trailing
/// non-whitespace run, so attached punctuation ("world.") is removed with
/// its word.
fn remove_trailing_words(text: &mut String, count: usize) {
    for _ in 0..count {
        while text.ends_with(char::is_whitespace) {
            text.pop();
        }
        if text.is_empty() {
            return;
        }
        while !text.ends_with(char::is_whitespace) {
            text.pop();
            if text.is_empty() {
                return;
            }
        }
    }
}

/// Removes the trailing word from a raw session buffer, using the same word
/// semantics as the voice-deletion pass (a word is a trailing non-whitespace
/// run, attached punctuation included). Unlike the voice-deletion join logic,
/// the whitespace that separated the removed word from the previous one is
/// kept, so material streamed after the edit joins with correct spacing.
pub fn remove_trailing_word_from_buffer(text: &str) -> String {
    remove_trailing_word_from_buffer_reporting(text).0
}

/// [`remove_trailing_word_from_buffer`] with reporting: also returns the
/// word that was removed (exactly the trailing non-whitespace run, attached
/// punctuation included, no surrounding whitespace), or `None` when the
/// buffer held no word to remove. Pure; the overlay shows the reported
/// text on a "Removed:" chip so a deletion is never invisible.
pub fn remove_trailing_word_from_buffer_reporting(text: &str) -> (String, Option<String>) {
    let mut buffer = text.to_string();
    remove_trailing_words(&mut buffer, 1);
    let removed = text.split_whitespace().next_back().map(str::to_string);
    (buffer, removed)
}

/// Clears the current trailing line of a raw session buffer, with the same
/// semantics as the voice-deletion "delete line" (and therefore the
/// in-session command of the same name): everything after the last newline
/// goes, the newline itself is kept so text arriving after the edit starts
/// on the fresh line, and with no newline the whole buffer is the trailing
/// line and empties entirely.
pub fn remove_trailing_line_from_buffer(text: &str) -> String {
    remove_trailing_line_from_buffer_reporting(text).0
}

/// [`remove_trailing_line_from_buffer`] with reporting: also returns the
/// text that was cleared (everything after the last newline, or the whole
/// buffer when there is no newline), or `None` when that span held nothing
/// visible to remove. Pure.
pub fn remove_trailing_line_from_buffer_reporting(text: &str) -> (String, Option<String>) {
    match text.rfind('\n') {
        Some(newline) => {
            let mut buffer = text.to_string();
            buffer.truncate(newline + 1);
            let removed = &text[newline + 1..];
            (
                buffer,
                (!removed.trim().is_empty()).then(|| removed.to_string()),
            )
        }
        None => (
            String::new(),
            (!text.trim().is_empty()).then(|| text.to_string()),
        ),
    }
}

/// Display transform for interim (mid-stream) overlay text.
///
/// Runs exactly the first two text passes of the finalize pipeline, in the
/// finalize order: the spoken-punctuation normalizer, then voice deletion.
/// The transform deliberately STOPS there:
///
/// * no terminal-punctuation fallback: a mid-sentence buffer must not grow a
///   period on every tick, and the fallback only makes sense on a finished
///   sentence;
/// * no custom-word correction, filler removal, or whitespace
///   normalization: those passes run once at finalize over the raw
///   transcript, and fuzzy correction on a half-spoken trailing word would
///   mis-rewrite text the model is still revising.
///
/// The transform is applied to the FULL raw buffer, recomputed from scratch
/// on every tick (never incrementally), so a spoken phrase split across
/// stream-chunk boundaries ("full" in one chunk, "stop" in the next) still
/// converts. Recomputation from the raw buffer also makes the transform
/// idempotent by construction: the raw accumulator is never itself
/// transformed, so applying the transform twice to the same raw input
/// produces the same output (asserted in tests).
pub fn interim_display_transform(
    text: &str,
    spoken_punctuation: bool,
    voice_deletion: bool,
    matrix: &CompiledCommandMatrix,
) -> String {
    let punctuated = if spoken_punctuation {
        normalize_spoken_punctuation(text, matrix)
    } else {
        text.to_string()
    };

    let deleted = if voice_deletion {
        apply_voice_deletion(&punctuated, matrix)
    } else {
        VoiceDeletionOutcome {
            text: punctuated,
            cleared: false,
        }
    };

    if deleted.cleared {
        String::new()
    } else {
        deleted.text
    }
}

/// Appends a span of untouched text. While `pending_space` is set (a deletion
/// just removed the preceding word), leading spaces/tabs are dropped and a
/// single separating space is inserted, so deletions collapse doubled spaces.
fn push_deletion_span(
    kept: &mut String,
    span: &str,
    pending_space: &mut bool,
    capital_owed: &mut bool,
) {
    if !*pending_space {
        push_restoring_capital(kept, span, capital_owed);
        return;
    }

    let trimmed = span.trim_start_matches([' ', '\t']);
    if trimmed.is_empty() {
        // Whitespace only: the separation is still owed to the next span.
        return;
    }
    if !kept.is_empty() && !kept.ends_with('\n') && !trimmed.starts_with('\n') {
        kept.push(' ');
    }
    *pending_space = false;
    push_restoring_capital(kept, trimmed, capital_owed);
}

/// Applies voice deletion commands to already-punctuated transcript text.
///
/// The vocabulary is the command matrix's DeleteWord / DeleteLine /
/// ClearAll entries plus the built-in count family: the DeleteWord phrases
/// ("delete word", "scratch that", "delete that", "remove that", and any
/// the operator added) delete the preceding word (the command is consumed
/// even when no word precedes it); "delete last word" through "delete last
/// ten words", including digit forms like "delete last 3 words", delete
/// that many preceding words; the DeleteLine phrases clear the current
/// trailing line (everything after the last newline, so with no newline
/// the whole buffer empties and the outcome is `cleared`, matching the
/// ClearAll semantics); the ClearAll phrases ("delete everything",
/// "scratch everything", "start over", ...) discard the whole transcript
/// and set [`VoiceDeletionOutcome::cleared`]. Commands apply left to
/// right, each seeing the result of the previous one.
///
/// After a deletion the next word is capitalized when it lands at a sentence
/// start (nothing kept yet, or the kept text ends a sentence). Text without
/// any command is preserved byte-for-byte except that leading spaces/tabs
/// left behind by a command consumed at the very start are trimmed.
pub fn apply_voice_deletion(text: &str, matrix: &CompiledCommandMatrix) -> VoiceDeletionOutcome {
    let mut kept = String::with_capacity(text.len());
    let mut resume = 0;
    let mut capital_owed = false;
    // Set by each deletion: the next span joins with exactly one space.
    let mut pending_space = false;

    for command in matrix.voice_deletion_pattern_matches(text) {
        let phrase = command.as_str().to_lowercase();

        // A matrix phrase maps to its deletion kind; a match outside the
        // map is the built-in "delete last N words" family.
        let count = match matrix.voice_deletion_kind(&phrase) {
            Some(VoiceDeletionKind::All) => {
                return VoiceDeletionOutcome {
                    text: String::new(),
                    cleared: true,
                };
            }
            Some(VoiceDeletionKind::Line) => {
                push_deletion_span(
                    &mut kept,
                    &text[resume..command.start()],
                    &mut pending_space,
                    &mut capital_owed,
                );
                // Clear the current trailing line: everything after the last
                // newline. The newline itself is kept so text spoken next
                // starts on the fresh line rather than joining the previous
                // one. With no newline the whole buffer is the trailing
                // line, so clearing it empties everything: report `cleared`
                // exactly like a ClearAll phrase (the pipeline then skips
                // later passes and pastes nothing).
                match kept.rfind('\n') {
                    Some(newline) => kept.truncate(newline + 1),
                    None => {
                        return VoiceDeletionOutcome {
                            text: String::new(),
                            cleared: true,
                        };
                    }
                }
                trim_trailing_spaces(&mut kept);
                pending_space = true;
                capital_owed |= opens_sentence(&kept);
                resume = command.end();
                continue;
            }
            Some(VoiceDeletionKind::Word) => 1,
            None => voice_deletion_word_count(&phrase),
        };

        push_deletion_span(
            &mut kept,
            &text[resume..command.start()],
            &mut pending_space,
            &mut capital_owed,
        );

        remove_trailing_words(&mut kept, count);
        trim_trailing_spaces(&mut kept);
        pending_space = true;
        capital_owed |= opens_sentence(&kept);
        resume = command.end();
    }

    let tail = &text[resume..];
    if pending_space {
        let mut pending = true;
        push_deletion_span(&mut kept, tail, &mut pending, &mut capital_owed);
    } else {
        push_restoring_capital(&mut kept, tail, &mut capital_owed);
    }

    VoiceDeletionOutcome {
        text: kept.trim_start_matches([' ', '\t']).to_string(),
        cleared: false,
    }
}

/// Applies non-filler transcription cleanup.
///
/// Kept separate from [`remove_filler_words`] so disabling filler deletion
/// does not also disable the existing repeated-word and whitespace cleanup.
///
/// Layout-preserving: runs of spaces/tabs collapse, but newline runs survive,
/// the leading trim is unchanged, and the right edge trims spaces/tabs only
/// while retaining any trailing "\n" run the operator spoke.
pub fn normalize_transcription_output(text: &str) -> String {
    let mut normalized = collapse_stutters(text);

    // Clean up multiple spaces/tabs to a single space; newline runs are
    // never touched by the pattern itself.
    normalized = MULTI_SPACE_PATTERN
        .replace_all(&normalized, " ")
        .to_string();

    // Trim leading whitespace as before; on the right edge trim spaces and
    // tabs only so a trailing newline run is retained.
    let trimmed_start = normalized.trim_start();
    let body_end = trimmed_start.trim_end_matches([' ', '\t']).len();
    trimmed_start[..body_end].to_string()
}

#[cfg(test)]
mod tests {
    use super::super::command_matrix::default_compiled_matrix;
    use super::*;

    /// The compiled DEFAULT matrix (shared; no rebuild per test).
    fn dm() -> std::sync::Arc<super::super::command_matrix::CompiledCommandMatrix> {
        default_compiled_matrix()
    }

    /// Exercise the complete cleanup sequence with an explicitly selected
    /// language. Individual tests below predate the split between filler
    /// removal and non-filler normalization.
    fn filter_transcription_output(
        text: &str,
        language: &str,
        custom_filler_words: &Option<Vec<String>>,
    ) -> String {
        let language = OutputLanguageEvidence::UserSelected(language.to_string());
        let filtered = remove_filler_words(text, &language, custom_filler_words, true);
        normalize_transcription_output(&filtered)
    }

    #[test]
    fn test_apply_custom_words_exact_match() {
        let text = "hello world";
        let custom_words = vec!["Hello".to_string(), "World".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.5);
        assert_eq!(result, "Hello World");
    }

    #[test]
    fn test_apply_custom_words_fuzzy_match() {
        let text = "helo wrold";
        let custom_words = vec!["hello".to_string(), "world".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.5);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_builtin_voxbar_seed_catches_near_variants_only() {
        // The built-in dictionary seed is a single "VoxBar" entry (see
        // default_custom_words in settings.rs). At the default 0.18
        // correction threshold the fuzzy pass catches spellings one edit
        // away, such as "woxbar" (score 0.17)...
        let custom_words = vec!["VoxBar".to_string()];
        assert_eq!(
            apply_custom_words("open woxbar please", &custom_words, 0.18),
            "open VoxBar please"
        );
        // ...while farther mishearings ("woksbar" scores 0.43, "worksbar"
        // 0.5) are left alone here. They are handled at decode time by the
        // whisper initial-prompt bias instead; raising the global threshold
        // to catch them would mis-correct ordinary words.
        assert_eq!(
            apply_custom_words("open woksbar please", &custom_words, 0.18),
            "open woksbar please"
        );
    }

    #[test]
    fn test_apply_custom_words_never_matches_across_a_newline() {
        // The correction runs per line: the fuzzy match still fires inside a
        // line, the break survives, and an n-gram can never consume the first
        // word of the next line across the boundary.
        let custom_words = vec!["VoxBar".to_string()];
        assert_eq!(
            apply_custom_words("line one\nvoxbar line two", &custom_words, 0.5),
            "line one\nVoxBar line two"
        );

        // "one voxbar" spells the custom word only if the newline is ignored;
        // with line-preserving matching nothing fires and the layout stays.
        let phrase = vec!["OneVoxBar".to_string()];
        assert_eq!(
            apply_custom_words("line one\nvoxbar tail", &phrase, 0.3),
            "line one\nvoxbar tail"
        );
    }

    #[test]
    fn test_preserve_case_pattern() {
        assert_eq!(preserve_case_pattern("HELLO", "world"), "WORLD");
        assert_eq!(preserve_case_pattern("Hello", "world"), "World");
        assert_eq!(preserve_case_pattern("hello", "WORLD"), "WORLD");
    }

    #[test]
    fn test_extract_punctuation() {
        assert_eq!(extract_punctuation("hello"), ("", ""));
        assert_eq!(extract_punctuation("!hello?"), ("!", "?"));
        assert_eq!(extract_punctuation("...hello..."), ("...", "..."));
    }

    #[test]
    fn test_extract_punctuation_uses_unicode_boundaries() {
        assert_eq!(extract_punctuation("你好。"), ("", "。"));
        assert_eq!(extract_punctuation("「你好」"), ("「", "」"));
        assert_eq!(extract_punctuation("你好！"), ("", "！"));
    }

    #[test]
    fn test_empty_custom_words() {
        let text = "hello world";
        let custom_words = vec![];
        let result = apply_custom_words(text, &custom_words, 0.5);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_filter_filler_words() {
        let text = "So uhm I was thinking uh about this";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "So I was thinking about this");
    }

    #[test]
    fn test_filter_filler_words_case_insensitive() {
        let text = "UHM this is UH a test";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "This is a test");
    }

    #[test]
    fn test_filter_filler_words_with_punctuation() {
        let text = "Well, uhm, I think, uh. that's right";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "Well, I think, that's right");
    }

    #[test]
    fn test_filter_cleans_whitespace() {
        let text = "Hello    world   test";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "Hello world test");
    }

    #[test]
    fn test_filter_trims() {
        let text = "  Hello world  ";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "Hello world");
    }

    #[test]
    fn test_filter_combined() {
        let text = "  Uhm, so I was, uh, thinking about this  ";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "So I was, thinking about this");
    }

    #[test]
    fn test_filter_leading_filler_keeps_sentence_capital() {
        let result = filter_transcription_output("Um, so I think we should ship it.", "en", &None);
        assert_eq!(result, "So I think we should ship it.");

        let result = filter_transcription_output("That works. Um, let me check.", "en", &None);
        assert_eq!(result, "That works. Let me check.");

        // Mid-sentence there is no capital to hand over.
        let result = filter_transcription_output("He said, Um, not today.", "en", &None);
        assert_eq!(result, "He said, not today.");
    }

    #[test]
    fn test_filter_preserves_valid_text() {
        let text = "This is a completely normal sentence.";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "This is a completely normal sentence.");
    }

    #[test]
    fn test_filter_stutter_collapse() {
        let text = "w wh wh wh wh wh wh wh wh wh why";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "w wh why");
    }

    #[test]
    fn test_filter_stutter_short_words() {
        let text = "I I I I think so so so so";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "I think so");
    }

    #[test]
    fn test_filter_stutter_longer_words() {
        let text = "Check data doc doc doc doc documentation.";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "Check data doc documentation.");
    }

    #[test]
    fn test_filter_stutter_mixed_case() {
        let text = "No NO no NO no";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "No");
    }

    #[test]
    fn test_filter_stutter_preserves_two_repetitions() {
        let text = "no no is fine";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "no no is fine");
    }

    #[test]
    fn test_stutter_collapse_is_per_line_keeping_newlines() {
        // Repetitions collapse within each line; the break survives.
        assert_eq!(
            collapse_stutters("the the the\none one one one"),
            "the\none"
        );
        // Repetition counting never crosses a line boundary: two pairs on
        // separate lines stay two pairs (a flattened count of four would
        // collapse them to one).
        assert_eq!(collapse_stutters("go go\ngo go"), "go go\ngo go");
        // Lines without stutters pass through with their layout intact.
        assert_eq!(
            collapse_stutters("hello world\nbye world"),
            "hello world\nbye world"
        );
    }

    #[test]
    fn test_normalize_preserves_newline_layout() {
        // Newline runs are layout, not whitespace noise: paragraphs survive
        // verbatim instead of being collapsed to a single space.
        assert_eq!(
            normalize_transcription_output("para one\n\npara two"),
            "para one\n\npara two"
        );
        // Runs of spaces still collapse.
        assert_eq!(
            normalize_transcription_output("hello  world"),
            "hello world"
        );
        // The leading trim is unchanged and a trailing newline run is
        // retained on the right edge.
        assert_eq!(normalize_transcription_output("  hello.\n"), "hello.\n");
    }

    #[test]
    fn test_filter_english_removes_um() {
        let text = "um I think um this is good";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "I think this is good");
    }

    #[test]
    fn test_filter_portuguese_preserves_um() {
        // "um" means "a/an" in Portuguese
        let text = "um gato bonito";
        let result = filter_transcription_output(text, "pt", &None);
        assert_eq!(result, "um gato bonito");
    }

    #[test]
    fn test_filter_spanish_preserves_ha() {
        // "ha" means "has" in Spanish
        let text = "ha sido un buen día";
        let result = filter_transcription_output(text, "es", &None);
        assert_eq!(result, "ha sido un buen día");
    }

    #[test]
    fn test_filter_language_code_with_region() {
        // "pt-BR" should normalize to "pt"
        let text = "um gato bonito";
        let result = filter_transcription_output(text, "pt-BR", &None);
        assert_eq!(result, "um gato bonito");
    }

    #[test]
    fn test_filter_custom_filler_words_override() {
        let custom = Some(vec!["okay".to_string(), "right".to_string()]);
        let text = "okay so I think right this works";
        let result = filter_transcription_output(text, "en", &custom);
        assert_eq!(result, "so I think this works");
    }

    #[test]
    fn test_filter_custom_filler_words_empty_disables() {
        let custom = Some(vec![]);
        let text = "So uhm I was thinking uh about this";
        let result = filter_transcription_output(text, "en", &custom);
        // No filler words removed since custom list is empty
        assert_eq!(result, "So uhm I was thinking uh about this");
    }

    #[test]
    fn test_filter_unknown_language_still_removes_universal_fillers() {
        let text = "uh I think uhm this works";
        let result = filter_transcription_output(text, "xx", &None);
        assert_eq!(result, "I think this works");
    }

    #[test]
    fn test_filter_unknown_language_does_not_remove_um() {
        let text = "um I think this works";
        let result = filter_transcription_output(text, "xx", &None);
        assert_eq!(result, "um I think this works");
    }

    #[test]
    fn test_filter_unknown_evidence_removes_universal_keeps_gated() {
        let filtered = remove_filler_words(
            "uhh bueno hmm creo que um ha llegado",
            &OutputLanguageEvidence::Unknown,
            &None,
            true,
        );
        assert_eq!(
            normalize_transcription_output(&filtered),
            "bueno creo que um ha llegado"
        );

        let cyrillic = remove_filler_words(
            "хм я думаю ммм это работает",
            &OutputLanguageEvidence::Unknown,
            &None,
            true,
        );
        assert_eq!(
            normalize_transcription_output(&cyrillic),
            "я думаю это работает"
        );
    }

    #[test]
    fn test_filter_german_gated_fillers_require_evidence() {
        let text = "äh ich glaube ähm das passt";

        let unknown = remove_filler_words(text, &OutputLanguageEvidence::Unknown, &None, true);
        assert_eq!(normalize_transcription_output(&unknown), text);

        let result = filter_transcription_output(text, "de", &None);
        assert_eq!(result, "ich glaube das passt");
    }

    #[test]
    fn test_filter_preserves_millimetre_unit() {
        // "mm" was removed from the filler lists because it eats units.
        let text = "the screw is 5 mm long";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "the screw is 5 mm long");
    }

    #[test]
    fn test_filter_detected_evidence_unlocks_gated_fillers() {
        let model = remove_filler_words(
            "um I think this works",
            &OutputLanguageEvidence::ModelDetected("en".to_string()),
            &None,
            true,
        );
        assert_eq!(normalize_transcription_output(&model), "I think this works");

        let text = remove_filler_words(
            "euh je pense que ça marche",
            &OutputLanguageEvidence::TextDetected("fr".to_string()),
            &None,
            true,
        );
        assert_eq!(
            normalize_transcription_output(&text),
            "je pense que ça marche"
        );
    }

    #[test]
    fn test_filter_master_toggle_disables_custom_and_builtin_removal() {
        let text = "um customword I think";
        let language = OutputLanguageEvidence::UserSelected("en".to_string());
        let custom = Some(vec!["customword".to_string()]);

        let result = remove_filler_words(text, &language, &custom, false);

        assert_eq!(result, text);
    }

    #[test]
    fn test_filter_custom_words_apply_without_language_evidence() {
        let custom = Some(vec!["customword".to_string()]);
        let text = "customword should be removed but um should remain";

        let filtered = remove_filler_words(text, &OutputLanguageEvidence::Unknown, &custom, true);
        let result = normalize_transcription_output(&filtered);

        assert_eq!(result, "should be removed but um should remain");
    }

    #[test]
    fn test_apply_custom_words_ngram_two_words() {
        let text = "il cui nome è Charge B, che permette";
        let custom_words = vec!["ChargeBee".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.5);
        assert!(result.contains("ChargeBee,"), "unexpected result: {result}");
        assert!(!result.contains("Charge B"));
    }

    #[test]
    fn test_apply_custom_words_ngram_three_words() {
        let text = "use Chat G P T for this";
        let custom_words = vec!["ChatGPT".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.5);
        assert!(result.contains("ChatGPT"));
    }

    #[test]
    fn test_apply_custom_words_prefers_longer_ngram() {
        let text = "Open AI GPT model";
        let custom_words = vec!["OpenAI".to_string(), "GPT".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.5);
        assert_eq!(result, "OpenAI GPT model");
    }

    #[test]
    fn test_apply_custom_words_ngram_preserves_case() {
        let text = "CHARGE B is great";
        let custom_words = vec!["ChargeBee".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.5);
        assert!(result.contains("CHARGEBEE"));
    }

    #[test]
    fn test_apply_custom_words_ngram_with_spaces_in_custom() {
        // Custom word with space should also match against split words
        let text = "using Mac Book Pro";
        let custom_words = vec!["MacBook Pro".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.5);
        assert_eq!(result, "using MacBook Pro");
    }

    #[test]
    fn test_apply_custom_words_trailing_number_not_doubled() {
        // Verify that trailing non-alpha chars (like numbers) aren't double-counted
        // between build_ngram stripping them and extract_punctuation capturing them
        let text = "use GPT4 for this";
        let custom_words = vec!["GPT-4".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.5);
        // Should NOT produce "GPT-44" (double-counting the trailing 4)
        assert!(
            !result.contains("GPT-44"),
            "got double-counted result: {}",
            result
        );
    }

    #[test]
    fn test_apply_custom_words_matches_ampersand_word() {
        let text = "send it to RD for review";
        let custom_words = vec!["R&D".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.18);
        assert_eq!(result, "send it to R&D for review");
    }

    #[test]
    fn test_apply_custom_words_matches_spoken_ampersand_word() {
        let text = "send it to R and D for review";
        let custom_words = vec!["R&D".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.18);
        assert_eq!(result, "send it to R&D for review");
    }

    #[test]
    fn test_apply_custom_words_preserves_ampersand_word() {
        let text = "send it to R&D for review";
        let custom_words = vec!["R&D".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.18);
        assert_eq!(result, "send it to R&D for review");
    }

    #[test]
    fn test_apply_custom_words_handles_unicode_punctuation() {
        let text = "「Handee。」";
        let custom_words = vec!["Handy".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.5);
        assert_eq!(result, "「Handy。」");
    }

    #[test]
    fn test_apply_custom_words_skips_cjk_fuzzy_matching() {
        let text = "你好。";
        let custom_words = vec!["你号".to_string()];
        let result = apply_custom_words(text, &custom_words, 1.0);
        assert_eq!(result, text);
    }

    #[test]
    fn test_spoken_punctuation_single_word_tokens() {
        assert_eq!(
            normalize_spoken_punctuation("hello comma world", &dm()),
            "hello, world"
        );
        assert_eq!(
            normalize_spoken_punctuation("hello full stop world", &dm()),
            "hello. World"
        );
        assert_eq!(
            normalize_spoken_punctuation("hello period world", &dm()),
            "hello. World"
        );
        assert_eq!(
            normalize_spoken_punctuation("it is fine question mark", &dm()),
            "it is fine?"
        );
        assert_eq!(
            normalize_spoken_punctuation("amazing exclamation mark", &dm()),
            "amazing!"
        );
        assert_eq!(
            normalize_spoken_punctuation("amazing exclamation point", &dm()),
            "amazing!"
        );
        assert_eq!(
            normalize_spoken_punctuation("items colon one two", &dm()),
            "items: one two"
        );
        assert_eq!(
            normalize_spoken_punctuation("one semicolon two", &dm()),
            "one; two"
        );
    }

    #[test]
    fn test_spoken_punctuation_case_insensitive() {
        assert_eq!(
            normalize_spoken_punctuation("HELLO COMMA World", &dm()),
            "HELLO, World"
        );
        assert_eq!(
            normalize_spoken_punctuation("Stop there Full Stop then go", &dm()),
            "Stop there. Then go"
        );
    }

    #[test]
    fn test_spoken_punctuation_newline_and_paragraph() {
        assert_eq!(
            normalize_spoken_punctuation("line one new line line two", &dm()),
            "line one\nline two"
        );
        assert_eq!(
            normalize_spoken_punctuation("para one new paragraph para two", &dm()),
            "para one\n\npara two"
        );
    }

    #[test]
    fn test_spoken_punctuation_dash_is_plain_hyphen() {
        let result = normalize_spoken_punctuation("twenty dash five", &dm());
        assert_eq!(result, "twenty-five");
        // Unicode escapes keep the literal em/en dash characters out of the
        // source while still asserting they can never be emitted.
        assert!(
            !result.contains('\u{2014}') && !result.contains('\u{2013}'),
            "dash must never become an em or en dash: {result}"
        );
    }

    #[test]
    fn test_spoken_punctuation_token_is_consumed() {
        assert_eq!(normalize_spoken_punctuation("hello comma", &dm()), "hello,");
        assert_eq!(normalize_spoken_punctuation("wait period", &dm()), "wait.");
        // Punctuation the model attached to the spoken token is consumed too.
        assert_eq!(
            normalize_spoken_punctuation("hello comma, world", &dm()),
            "hello, world"
        );
    }

    #[test]
    fn test_spoken_punctuation_mark_plus_word_never_doubles() {
        // The model emitted BOTH the literal mark and the spoken command
        // word (whisper's frequent rendering of spoken punctuation): the
        // replacement must coalesce with the mark already in the text, not
        // append a second one.
        assert_eq!(
            normalize_spoken_punctuation("hello, comma world", &dm()),
            "hello, world"
        );
        assert_eq!(normalize_spoken_punctuation("done. period", &dm()), "done.");
        assert_eq!(
            normalize_spoken_punctuation("done? question mark", &dm()),
            "done?"
        );
        assert_eq!(
            normalize_spoken_punctuation("value: colon x", &dm()),
            "value: x"
        );
    }

    #[test]
    fn test_spoken_punctuation_consecutive_newline_phrases_still_stack() {
        // The dedup guard never applies to line breaks: spoken "new line" /
        // "new paragraph" repeats keep stacking layout.
        assert_eq!(
            normalize_spoken_punctuation("done new line new line next", &dm()),
            "done\n\nnext"
        );
        assert_eq!(
            normalize_spoken_punctuation("para new paragraph new paragraph", &dm()),
            "para\n\n\n\n"
        );
    }

    #[test]
    fn test_spoken_punctuation_capitalizes_after_sentence_end_only() {
        assert_eq!(
            normalize_spoken_punctuation("first full stop second", &dm()),
            "first. Second"
        );
        assert_eq!(
            normalize_spoken_punctuation("first comma second", &dm()),
            "first, second"
        );
        assert_eq!(
            normalize_spoken_punctuation("first colon second", &dm()),
            "first: second"
        );
        // The capital survives a following line break.
        assert_eq!(
            normalize_spoken_punctuation("done full stop new line next", &dm()),
            "done.\nNext"
        );
    }

    #[test]
    fn test_spoken_punctuation_word_boundaries_do_not_fire_inside_words() {
        assert_eq!(
            normalize_spoken_punctuation("the question is questionable", &dm()),
            "the question is questionable"
        );
        assert_eq!(
            normalize_spoken_punctuation("a question of time", &dm()),
            "a question of time"
        );
        assert_eq!(
            normalize_spoken_punctuation("periodic periods matter", &dm()),
            "periodic periods matter"
        );
        assert_eq!(
            normalize_spoken_punctuation("dashboards and dashes", &dm()),
            "dashboards and dashes"
        );
        // "question mark" must match the full phrase only.
        assert_eq!(
            normalize_spoken_punctuation("the question marks the end", &dm()),
            "the question marks the end"
        );
        assert_eq!(
            normalize_spoken_punctuation("check the question markdown", &dm()),
            "check the question markdown"
        );
        assert_eq!(
            normalize_spoken_punctuation("many commas here", &dm()),
            "many commas here"
        );
    }

    #[test]
    fn test_spoken_punctuation_preserves_existing_layout() {
        assert_eq!(
            normalize_spoken_punctuation("first line\nsecond line", &dm()),
            "first line\nsecond line"
        );
        assert_eq!(
            normalize_spoken_punctuation("already, punctuated. Text!", &dm()),
            "already, punctuated. Text!"
        );
        // CJK text is untouched: the vocabulary is English-only.
        assert_eq!(normalize_spoken_punctuation("你好世界", &dm()), "你好世界");
    }

    #[test]
    fn test_terminal_punctuation_appends_period_by_default() {
        assert_eq!(apply_terminal_punctuation("hello world"), "hello world.");
        // Trailing whitespace is dropped so the mark is not stranded.
        assert_eq!(apply_terminal_punctuation("hello world  "), "hello world.");
    }

    #[test]
    fn test_terminal_punctuation_appends_question_for_interrogatives() {
        for opener in [
            "what", "why", "how", "when", "who", "where", "which", "is", "are", "do", "does",
            "can", "could", "would", "should", "will",
        ] {
            let text = format!("{opener} is this");
            assert_eq!(
                apply_terminal_punctuation(&text),
                format!("{text}?"),
                "opener: {opener}"
            );
        }
        // Case-insensitive, with leading punctuation on the first word.
        assert_eq!(apply_terminal_punctuation("What is this"), "What is this?");
        assert_eq!(apply_terminal_punctuation("\"why\" ask"), "\"why\" ask?");
    }

    #[test]
    fn test_terminal_punctuation_never_doubles_or_touches_empty() {
        assert_eq!(apply_terminal_punctuation("hello world."), "hello world.");
        assert_eq!(apply_terminal_punctuation("hello world?"), "hello world?");
        assert_eq!(apply_terminal_punctuation("hello world!"), "hello world!");
        assert_eq!(apply_terminal_punctuation(""), "");
        assert_eq!(apply_terminal_punctuation("   "), "   ");
        // Existing terminal punctuation with trailing whitespace is left alone.
        assert_eq!(apply_terminal_punctuation("done! "), "done! ");
    }

    #[test]
    fn test_terminal_punctuation_inserts_mark_before_trailing_newline_run() {
        // A trailing newline run is spoken layout ("new line" / "new
        // paragraph"); the mark lands before it instead of trimming it away.
        assert_eq!(apply_terminal_punctuation("hello\n"), "hello.\n");
        assert_eq!(apply_terminal_punctuation("para one\n\n"), "para one.\n\n");
        // The interrogative choice runs on the body before the run.
        assert_eq!(
            apply_terminal_punctuation("what is this\n"),
            "what is this?\n"
        );
    }

    #[test]
    fn test_voice_deletion_word_commands() {
        for command in ["scratch that", "delete that", "remove that"] {
            let result = apply_voice_deletion(&format!("hello world {command}"), &dm());
            assert_eq!(result.text, "hello", "command: {command}");
            assert!(!result.cleared, "command: {command}");
        }
    }

    #[test]
    fn test_voice_deletion_case_insensitive_and_word_boundaries() {
        assert_eq!(
            apply_voice_deletion("Hello World SCRATCH THAT", &dm()).text,
            "Hello"
        );
        // Nothing fires inside larger words, on near-misses, or on digit
        // counts outside 1-10.
        for text in [
            "he deleted that file",
            "underscratch that",
            "scratch thatch",
            "removal that",
            "delete lasting words",
            "delete last 0 words",
            "delete last 99 words",
        ] {
            let result = apply_voice_deletion(text, &dm());
            assert_eq!(result.text, text, "text: {text}");
            assert!(!result.cleared, "text: {text}");
        }
    }

    #[test]
    fn test_voice_deletion_counted_forms() {
        assert_eq!(
            apply_voice_deletion("a b c delete last word", &dm()).text,
            "a b"
        );
        assert_eq!(
            apply_voice_deletion("a b c delete last one word", &dm()).text,
            "a b"
        );
        // Word forms and digit forms delete the same words.
        let base = "one two three four five six seven eight nine ten";
        let words: Vec<&str> = base.split(' ').collect();
        for (word_form, count) in [("two", 2), ("three", 3), ("ten", 10)] {
            let expected = words[..words.len() - count].join(" ");
            let spoken = format!("{base} delete last {word_form} words");
            assert_eq!(
                apply_voice_deletion(&spoken, &dm()).text,
                expected,
                "form: {word_form}"
            );
            let digits = format!("{base} delete last {count} words");
            assert_eq!(
                apply_voice_deletion(&digits, &dm()).text,
                expected,
                "digit count: {count}"
            );
        }
        // Requesting more words than remain deletes all of them.
        assert_eq!(
            apply_voice_deletion("a b delete last ten words", &dm()).text,
            ""
        );
    }

    #[test]
    fn test_voice_deletion_everything_commands_clear() {
        for command in ["delete everything", "scratch everything", "start over"] {
            let result =
                apply_voice_deletion(&format!("hello world {command} trailing words"), &dm());
            assert_eq!(result.text, "", "command: {command}");
            assert!(result.cleared, "command: {command}");
        }
    }

    #[test]
    fn test_voice_deletion_no_preceding_word_still_consumes() {
        let result = apply_voice_deletion("scratch that hello there", &dm());
        assert_eq!(result.text, "Hello there");
        assert!(!result.cleared);

        let empty = apply_voice_deletion("", &dm());
        assert_eq!(empty.text, "");
        assert!(!empty.cleared);
    }

    #[test]
    fn test_voice_deletion_capital_and_spacing_after_deletion() {
        // A word landing at a sentence start after a deletion is capitalized.
        assert_eq!(
            apply_voice_deletion("One. Two scratch that three", &dm()).text,
            "One. Three"
        );
        // Mid-sentence deletions do not capitalize.
        assert_eq!(
            apply_voice_deletion("hello world scratch that there", &dm()).text,
            "hello there"
        );
        // Deletions collapse doubled spaces around the join point.
        assert_eq!(
            apply_voice_deletion("a  b   scratch that   c", &dm()).text,
            "a c"
        );
    }

    #[test]
    fn test_voice_deletion_commands_chain_left_to_right() {
        assert_eq!(
            apply_voice_deletion("a b c scratch that delete that", &dm()).text,
            "a"
        );
        assert_eq!(
            apply_voice_deletion("a b c delete last two words scratch that", &dm()).text,
            ""
        );
        // Word-by-word deletion down to empty does not set the cleared flag.
        let emptied = apply_voice_deletion("a b scratch that scratch that", &dm());
        assert_eq!(emptied.text, "");
        assert!(!emptied.cleared);
    }

    #[test]
    fn test_voice_deletion_preserves_layout_without_commands() {
        let result = apply_voice_deletion("first line\nsecond line", &dm());
        assert_eq!(result.text, "first line\nsecond line");
        assert!(!result.cleared);

        assert_eq!(
            apply_voice_deletion("already, punctuated. Text!", &dm()).text,
            "already, punctuated. Text!"
        );
    }

    #[test]
    fn test_voice_deletion_consumes_punctuated_word_tokens() {
        // The pass runs after the spoken-punctuation normalizer, so deleted
        // words carry their attached punctuation with them.
        assert_eq!(
            apply_voice_deletion("hello, world scratch that", &dm()).text,
            "hello,"
        );
        assert_eq!(
            apply_voice_deletion("Done. World delete that next", &dm()).text,
            "Done. Next"
        );
    }

    #[test]
    fn test_voice_deletion_delete_line_clears_trailing_line() {
        // Everything after the last newline goes; the newline itself stays so
        // the next spoken word starts on the fresh line.
        assert_eq!(
            apply_voice_deletion("first line\nsecond part delete line", &dm()).text,
            "first line\n"
        );
        // Text spoken after the command remains, starting on the fresh line.
        assert_eq!(
            apply_voice_deletion("one\n_two\nthree delete line four", &dm()).text,
            "one\n_two\nfour"
        );
        assert_eq!(
            apply_voice_deletion("one\ntwo delete line three", &dm()).text,
            "one\nthree"
        );
    }

    #[test]
    fn test_voice_deletion_delete_line_without_newline_clears_everything() {
        // No newline means the whole buffer is the trailing line: clearing it
        // empties everything, reported with the cleared flag like "delete
        // everything" (including anything spoken after the command).
        let result = apply_voice_deletion("just one line delete line trailing words", &dm());
        assert_eq!(result.text, "");
        assert!(result.cleared);

        let bare = apply_voice_deletion("delete line", &dm());
        assert_eq!(bare.text, "");
        assert!(bare.cleared);
    }

    #[test]
    fn test_voice_deletion_delete_line_word_boundaries() {
        // Nothing fires inside larger words or on near-misses.
        for text in [
            "delete lined",
            "deleted line",
            "delete lines",
            "the delete lineage here",
        ] {
            let result = apply_voice_deletion(text, &dm());
            assert_eq!(result.text, text, "text: {text}");
            assert!(!result.cleared, "text: {text}");
        }
    }

    #[test]
    fn test_voice_deletion_delete_line_then_more_commands_chain() {
        // A delete-line followed by a word deletion: each command sees the
        // result of the previous one. Here "scratch that" removes "four"
        // (the word spoken after the line was cleared).
        assert_eq!(
            apply_voice_deletion("one\ntwo three delete line four scratch that five", &dm()).text,
            "one\nfive"
        );
    }

    #[test]
    fn test_remove_trailing_word_from_buffer_matches_voice_deletion_word_semantics() {
        // A word is a trailing non-whitespace run; attached punctuation goes
        // with it; the separating whitespace is kept for the join.
        assert_eq!(remove_trailing_word_from_buffer("hello world"), "hello ");
        assert_eq!(remove_trailing_word_from_buffer("done."), "");
        // CJK without spaces is one contiguous non-whitespace run, so the
        // whole run counts as the trailing word.
        assert_eq!(remove_trailing_word_from_buffer("你好 世界"), "你好 ");
        assert_eq!(remove_trailing_word_from_buffer(""), "");
    }

    #[test]
    fn test_remove_trailing_line_from_buffer_matches_voice_deletion_line_semantics() {
        // Everything after the last newline goes; the newline stays so text
        // arriving after the edit starts on the fresh line.
        assert_eq!(
            remove_trailing_line_from_buffer("first\nsecond third"),
            "first\n"
        );
        assert_eq!(remove_trailing_line_from_buffer("first\n"), "first\n");
        // No newline: the whole buffer is the trailing line and empties.
        assert_eq!(remove_trailing_line_from_buffer("only line"), "");
        assert_eq!(remove_trailing_line_from_buffer(""), "");
    }

    #[test]
    fn test_deletion_reporting_helpers_report_the_removed_span() {
        // Word reporting: the reported text is exactly the removed trailing
        // non-whitespace run, attached punctuation included; the buffer
        // outcome matches the non-reporting variant.
        assert_eq!(
            remove_trailing_word_from_buffer_reporting("one two three"),
            ("one two ".to_string(), Some("three".to_string()))
        );
        assert_eq!(
            remove_trailing_word_from_buffer_reporting("done."),
            (String::new(), Some("done.".to_string()))
        );
        assert_eq!(
            remove_trailing_word_from_buffer_reporting("one "),
            (String::new(), Some("one".to_string()))
        );
        // Nothing visible to remove.
        assert_eq!(
            remove_trailing_word_from_buffer_reporting("   "),
            (String::new(), None)
        );
        assert_eq!(
            remove_trailing_word_from_buffer_reporting(""),
            (String::new(), None)
        );

        // Line reporting: the span after the last newline, or the whole
        // buffer with no newline; None when that span holds nothing
        // visible.
        assert_eq!(
            remove_trailing_line_from_buffer_reporting("first\nsecond third"),
            ("first\n".to_string(), Some("second third".to_string()))
        );
        assert_eq!(
            remove_trailing_line_from_buffer_reporting("first\n"),
            ("first\n".to_string(), None)
        );
        assert_eq!(
            remove_trailing_line_from_buffer_reporting("first\n  "),
            ("first\n".to_string(), None)
        );
        assert_eq!(
            remove_trailing_line_from_buffer_reporting("only line"),
            (String::new(), Some("only line".to_string()))
        );
        assert_eq!(
            remove_trailing_line_from_buffer_reporting(""),
            (String::new(), None)
        );
    }

    #[test]
    fn test_interim_display_transform_runs_both_enabled_passes() {
        // Spoken punctuation converts and voice deletion removes, in finalize
        // order.
        assert_eq!(
            interim_display_transform("hello comma world", true, true, &dm()),
            "hello, world"
        );
        assert_eq!(
            interim_display_transform("hello world scratch that there", true, true, &dm()),
            "hello there"
        );
    }

    #[test]
    fn test_interim_display_transform_respects_toggles() {
        assert_eq!(
            interim_display_transform("hello comma world", false, true, &dm()),
            "hello comma world"
        );
        assert_eq!(
            interim_display_transform("hello world scratch that there", true, false, &dm()),
            "hello world scratch that there"
        );
    }

    #[test]
    fn test_interim_display_transform_grows_no_terminal_punctuation() {
        // The deliberate stop: a mid-sentence buffer must not gain a period
        // (or question mark) on any tick.
        assert_eq!(
            interim_display_transform("hello world", true, true, &dm()),
            "hello world"
        );
        assert_eq!(
            interim_display_transform("what is this", true, true, &dm()),
            "what is this"
        );
    }

    #[test]
    fn test_interim_display_transform_clear_outcome_empties_display() {
        assert_eq!(
            interim_display_transform("hello delete everything spoken after", true, true, &dm()),
            ""
        );
    }

    #[test]
    fn test_interim_display_transform_is_idempotent() {
        // The transform is recomputed from the raw buffer every tick, never
        // applied to its own output; still, double application must be a
        // fixed point so a recompute can never compound.
        for raw in [
            "hello comma world full stop next period",
            "one scratch that two delete last two words three",
            "first line new line second line delete line tail",
            "plain text with no commands at all",
            "twenty dash five",
        ] {
            let once = interim_display_transform(raw, true, true, &dm());
            let twice = interim_display_transform(&once, true, true, &dm());
            assert_eq!(once, twice, "raw: {raw}");
        }
    }
}
