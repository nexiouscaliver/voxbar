//! Deterministic spoken-number formatting (inverse text normalization for
//! number words only).
//!
//! Some engines (Parakeet-class, per the upstream Handy model docs) write
//! every number as words, so operators dictating identifiers hear "pull
//! request one one zero five" back instead of "1105". This module is the
//! post-model fix: a pure, local, language-aware pass that rewrites spoken
//! number words into digits under the `number_format` setting
//! (`as_transcribed` keeps the transcript byte-identical to 1.1.0).
//!
//! Grammar classes are staged after the classic ITN literature (cardinals,
//! compound ordinals, point decimals and digit sequences as separate
//! rules; dates, times and currency deliberately excluded, and no
//! reordering ever). Isolated-word semantics borrow from text2num: a
//! small-number threshold ("eleven" converts in Smart, "ten" and below
//! never convert alone), positional digit-sequence collapse preserving
//! leading zeros, and ordinals as a flag on the cardinal grammar.
//! Hand-rolled table, no new dependency (the repo convention stated at
//! hindi_script.rs).
//!
//! Safety rails, all pinned by tests:
//! * only ALPHABETIC number-word tokens are ever rewritten; existing
//!   digits and digit+letter identifiers ("room 4B", "0x1F", "base64",
//!   "IPv4", "GPT 4") are byte-identical in every mode, and untouched text
//!   keeps its whitespace and newline layout byte-for-byte;
//! * lone unit words (one..ten, zero, "oh") never convert alone, so the
//!   pronoun "one", "no one", "someone", "give me five" and "one in a
//!   million" stay words;
//! * scale words never convert without a digit coefficient ("a million",
//!   bare "hundred");
//! * list speech ("one, two, three") never collapses: a token carrying
//!   edge punctuation can only END a phrase, never extend one;
//! * the Devanagari grammar matches only Devanagari words, so romanized
//!   Hinglish number words ("ek", "do", "char") are never touched, and a
//!   lone "ek" (the Hindi indefinite article) never converts;
//! * output digits are ASCII for the English grammar and Devanagari
//!   (U+0966-096F) for the Hindi grammar; the existing hindi_script
//!   transliterator maps those to ASCII for Hinglish output for free;
//! * values emit PLAIN digits with no comma grouping (locale-dependent
//!   placement would corrupt code-paste contexts; Indian 15,00,000
//!   grouping may become a Smart-only option later).

use crate::audio_toolkit::text::OutputLanguageEvidence;
use crate::settings::{AppSettings, NumberFormat};

/// Which grammar (word tables) a conversion run uses. The English grammar
/// matches only Latin-script English number words; the Devanagari grammar
/// matches only Devanagari Hindi number words, so the two never collide
/// and mixed-script text converts only the words of its own script.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumberScript {
    English,
    Devanagari,
}

/// Which number passes the pipeline runs for a transcription, resolved
/// from the setting plus the output-language evidence. The Devanagari pass
/// runs on the raw (still-Devanagari) text for Hindi and Hinglish output,
/// before the Hinglish transliteration; the English pass runs on the final
/// wording (Latin fragments of Hinglish included). Both fail closed on
/// unknown output language (the filler-pass convention), except the
/// explicit "hi-Latn" script intent, which guarantees Devanagari model
/// output the same way it guarantees the transliteration step.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NumberPassScripts {
    pub devanagari: bool,
    pub english: bool,
}

/// The per-session snapshot the interim path captures: the mode plus the
/// script gating the finalize pipeline resolved for the run. Bundled so a
/// session begin stays a single argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NumberPass {
    pub mode: NumberFormat,
    pub scripts: NumberPassScripts,
}

impl NumberPass {
    /// The inert snapshot: no pass runs anywhere.
    pub fn disabled() -> Self {
        Self {
            mode: NumberFormat::AsTranscribed,
            scripts: NumberPassScripts::default(),
        }
    }
}

/// The passes a transcription's pipeline should run, resolved from the
/// setting plus its output-language evidence.
pub fn number_pass(settings: &AppSettings, output_language: &OutputLanguageEvidence) -> NumberPass {
    NumberPass {
        mode: settings.number_format,
        scripts: number_pass_scripts(settings, output_language),
    }
}

pub fn number_pass_scripts(
    settings: &AppSettings,
    output_language: &OutputLanguageEvidence,
) -> NumberPassScripts {
    let hinglish_intent = settings.selected_language == "hi-Latn";
    let base = output_language
        .language()
        .map(|lang| lang.split(&['-', '_'][..]).next().unwrap_or(lang));
    NumberPassScripts {
        devanagari: hinglish_intent || base == Some("hi"),
        english: hinglish_intent || base == Some("en"),
    }
}

/// A classified number word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// A spoken single zero: "zero" / "oh" / "shunya". Never a compound
    /// coefficient; lives inside digit runs and point fractions.
    Digit(u8),
    /// one..nine; a compound coefficient.
    Unit(u8),
    /// ten..nineteen; a complete group.
    Teen(u16),
    /// twenty..ninety; combines with a following unit.
    Tens(u16),
    /// A hyphenated tens-unit token ("forty-two") carries its full group
    /// value and converts standalone even in Digits mode (it is two spoken
    /// number words joined by the model).
    HyphenGroup(u16),
    /// A scale word. `big` scales terminate the accumulated group
    /// (thousand/million/billion, hazaar/lakh/karod); hundred-class scales
    /// (English "hundred", Hindi "sau") multiply the current group.
    Scale { value: u64, big: bool },
    /// British "and" joining a scale to the rest ("two thousand and
    /// five"). English grammar only.
    And,
    /// The decimal connector ("three point one four"). English grammar
    /// only.
    Point,
    /// An ordinal word (first..ninetieth); Smart mode, multi-word
    /// compounds only. English grammar only.
    Ordinal(u16),
}

const fn scale(value: u64) -> Kind {
    Kind::Scale { value, big: true }
}

const fn hundred_class() -> Kind {
    Kind::Scale {
        value: 100,
        big: false,
    }
}

fn english_kind(core: &str) -> Option<Kind> {
    Some(match core {
        "zero" | "oh" => Kind::Digit(0),
        "one" => Kind::Unit(1),
        "two" => Kind::Unit(2),
        "three" => Kind::Unit(3),
        "four" => Kind::Unit(4),
        "five" => Kind::Unit(5),
        "six" => Kind::Unit(6),
        "seven" => Kind::Unit(7),
        "eight" => Kind::Unit(8),
        "nine" => Kind::Unit(9),
        "ten" => Kind::Teen(10),
        "eleven" => Kind::Teen(11),
        "twelve" => Kind::Teen(12),
        "thirteen" => Kind::Teen(13),
        "fourteen" => Kind::Teen(14),
        "fifteen" => Kind::Teen(15),
        "sixteen" => Kind::Teen(16),
        "seventeen" => Kind::Teen(17),
        "eighteen" => Kind::Teen(18),
        "nineteen" => Kind::Teen(19),
        "twenty" => Kind::Tens(20),
        "thirty" => Kind::Tens(30),
        "forty" => Kind::Tens(40),
        "fifty" => Kind::Tens(50),
        "sixty" => Kind::Tens(60),
        "seventy" => Kind::Tens(70),
        "eighty" => Kind::Tens(80),
        "ninety" => Kind::Tens(90),
        "hundred" => hundred_class(),
        "thousand" => scale(1_000),
        "million" => scale(1_000_000),
        "billion" => scale(1_000_000_000),
        "and" => Kind::And,
        "point" => Kind::Point,
        "first" => Kind::Ordinal(1),
        "second" => Kind::Ordinal(2),
        "third" => Kind::Ordinal(3),
        "fourth" => Kind::Ordinal(4),
        "fifth" => Kind::Ordinal(5),
        "sixth" => Kind::Ordinal(6),
        "seventh" => Kind::Ordinal(7),
        "eighth" => Kind::Ordinal(8),
        "ninth" => Kind::Ordinal(9),
        "tenth" => Kind::Ordinal(10),
        "eleventh" => Kind::Ordinal(11),
        "twelfth" => Kind::Ordinal(12),
        "thirteenth" => Kind::Ordinal(13),
        "fourteenth" => Kind::Ordinal(14),
        "fifteenth" => Kind::Ordinal(15),
        "sixteenth" => Kind::Ordinal(16),
        "seventeenth" => Kind::Ordinal(17),
        "eighteenth" => Kind::Ordinal(18),
        "nineteenth" => Kind::Ordinal(19),
        "twentieth" => Kind::Ordinal(20),
        "thirtieth" => Kind::Ordinal(30),
        "fortieth" => Kind::Ordinal(40),
        "fiftieth" => Kind::Ordinal(50),
        "sixtieth" => Kind::Ordinal(60),
        "seventieth" => Kind::Ordinal(70),
        "eightieth" => Kind::Ordinal(80),
        "ninetieth" => Kind::Ordinal(90),
        _ => return None,
    })
}

fn devanagari_kind(core: &str) -> Option<Kind> {
    Some(match core {
        "शून्य" => Kind::Digit(0),
        "एक" => Kind::Unit(1),
        "दो" => Kind::Unit(2),
        "तीन" => Kind::Unit(3),
        "चार" => Kind::Unit(4),
        "पाँच" | "पांच" => Kind::Unit(5),
        "छह" | "छः" => Kind::Unit(6),
        "सात" => Kind::Unit(7),
        "आठ" => Kind::Unit(8),
        "नौ" => Kind::Unit(9),
        "दस" => Kind::Teen(10),
        "ग्यारह" => Kind::Teen(11),
        "बारह" => Kind::Teen(12),
        "तेरह" => Kind::Teen(13),
        "चौदह" => Kind::Teen(14),
        "पंद्रह" => Kind::Teen(15),
        "सोलह" => Kind::Teen(16),
        "सत्रह" => Kind::Teen(17),
        "अठारह" => Kind::Teen(18),
        "उन्नीस" => Kind::Teen(19),
        "बीस" => Kind::Tens(20),
        "तीस" => Kind::Tens(30),
        "चालीस" => Kind::Tens(40),
        "पचास" => Kind::Tens(50),
        "साठ" => Kind::Tens(60),
        "सत्तर" => Kind::Tens(70),
        "अस्सी" => Kind::Tens(80),
        "नब्बे" => Kind::Tens(90),
        "सौ" => hundred_class(),
        "हज़ार" | "हजार" => scale(1_000),
        "लाख" => scale(100_000),
        "करोड़" | "करोड" => scale(10_000_000),
        _ => return None,
    })
}

fn script_kind(core: &str, script: NumberScript) -> Option<Kind> {
    match script {
        NumberScript::English => english_kind(core),
        NumberScript::Devanagari => devanagari_kind(core),
    }
}

/// One whitespace-delimited token. Matching uses the lowercased
/// alphanumeric core; edge punctuation survives around a replacement.
struct Token<'a> {
    text: &'a str,
    kind: Option<Kind>,
    leading_punct: &'a str,
    trailing_punct: &'a str,
}

impl<'a> Token<'a> {
    fn new(text: &'a str, script: NumberScript) -> Self {
        let lower = text.to_lowercase();
        let mut token = Token {
            text,
            kind: None,
            leading_punct: "",
            trailing_punct: "",
        };
        let Some((start, _)) = lower.char_indices().find(|(_, c)| c.is_alphanumeric()) else {
            return token;
        };
        let Some((end, last)) = lower
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_alphanumeric())
        else {
            return token;
        };
        let end = end + last.len_utf8();
        token.leading_punct = &text[..start];
        token.trailing_punct = &text[end..];
        let core = &lower[start..end];

        // Anything not purely word-like inside (existing digits,
        // identifiers like "4B"/"base64"/"0x1F", contractions like "won't")
        // never matches. Word-like means letters plus combining marks:
        // Devanagari vowel signs, virama and nukta are marks, not letters,
        // so an alphanumeric-only test would reject words like "shunya"
        // and "hazaar". The single exception to the no-punctuation rule is
        // the canonical hyphenated tens-unit compound ("forty-two"): two
        // spoken number words in one token. A hyphen joining anything else
        // keeps the whole token inert ("one-two punch").
        if let Some(hyphen) = core.find('-') {
            let (left, right) = (&core[..hyphen], &core[hyphen + 1..]);
            if !left.is_empty()
                && !right.is_empty()
                && left.chars().all(is_word_char)
                && right.chars().all(is_word_char)
            {
                if let (Some(Kind::Tens(tens)), Some(Kind::Unit(unit))) =
                    (script_kind(left, script), script_kind(right, script))
                {
                    token.kind = Some(Kind::HyphenGroup(tens + u16::from(unit)));
                }
            }
            return token;
        }
        if !core.chars().all(is_word_char) {
            return token;
        }
        token.kind = script_kind(core, script);
        token
    }
}

/// Letters and the Devanagari block's combining marks (matras, virama,
/// nukta) form words; digits, punctuation and symbols do not.
fn is_word_char(c: char) -> bool {
    c.is_alphabetic() || ('\u{0900}'..='\u{097F}').contains(&c)
}

/// A successful cardinal-grammar parse of a token span.
struct Cardinal {
    value: u64,
    words: usize,
}

/// Parse the cardinal grammar over `kinds`, longest valid prefix:
/// groups (units / teens / tens, hundred-class multiplied) joined by big
/// scales, British "and" allowed between a scale and the following group.
/// Stops (without failing) at the first word that cannot extend the
/// phrase; callers decide whether the prefix is convertible. A bare scale
/// word never starts a phrase and a big scale never lands on an empty
/// group, so "a hundred..." style coefficient-less scales never parse.
fn parse_cardinal(kinds: &[Option<Kind>]) -> Option<Cardinal> {
    let mut total: u64 = 0;
    let mut current: u64 = 0;
    let mut words = 0usize;
    let mut last_was_scale = false;

    for (index, kind) in kinds.iter().enumerate() {
        match kind {
            Some(Kind::Unit(unit)) => {
                // A unit only fills a group's open ones-slot: nothing yet,
                // a bare tens (twenty + five), or whole hundreds.
                if current.is_multiple_of(10) {
                    current += *unit as u64;
                } else {
                    break;
                }
            }
            Some(Kind::Teen(value)) => {
                if current.is_multiple_of(100) {
                    current += *value as u64;
                } else {
                    break;
                }
            }
            Some(Kind::Tens(value)) | Some(Kind::HyphenGroup(value)) => {
                if current.is_multiple_of(100) {
                    current += *value as u64;
                } else {
                    break;
                }
            }
            Some(Kind::Scale { value, big }) => {
                if *big {
                    // "thousand" needs a group to terminate ("one
                    // thousand"); a second big scale with nothing
                    // accumulated is not English ("thousand thousand").
                    if current == 0 {
                        break;
                    }
                    total = total.saturating_add(current.saturating_mul(*value));
                    current = 0;
                } else {
                    // Hundred-class: multiplies the current group
                    // ("eleven hundred", "one hundred"), implied one only
                    // mid-phrase; a phrase never STARTS on a scale.
                    if words == 0 {
                        break;
                    }
                    current = (current.max(1)).saturating_mul(*value);
                }
            }
            Some(Kind::And) => {
                // Consumed only between a scale and a following group, so
                // a trailing "and" (or "one and done") never gets eaten.
                let next_is_group = matches!(
                    kinds.get(index + 1),
                    Some(Some(
                        Kind::Unit(_) | Kind::Teen(_) | Kind::Tens(_) | Kind::HyphenGroup(_)
                    ))
                );
                if !last_was_scale || !next_is_group {
                    break;
                }
            }
            _ => break,
        }
        last_was_scale = matches!(kind, Some(Kind::Scale { .. }));
        words += 1;
    }

    if words == 0 {
        None
    } else {
        Some(Cardinal {
            value: total.saturating_add(current),
            words,
        })
    }
}

fn is_digit_word(kind: &Option<Kind>) -> bool {
    matches!(kind, Some(Kind::Digit(_)) | Some(Kind::Unit(_)))
}

/// Any word of the number grammar; used for the Smart-mode neighbor guard
/// so isolated conversions never fire next to a larger phrase still being
/// dictated ("four twenty five" stays words rather than "four 20 five").
fn is_number_word(kind: &Option<Kind>) -> bool {
    kind.is_some()
}

/// Render a value's decimal digits in the target script.
fn render_digits(ascii: &str, script: NumberScript) -> String {
    match script {
        NumberScript::English => ascii.to_string(),
        NumberScript::Devanagari => ascii
            .chars()
            .map(|c| {
                if c.is_ascii_digit() {
                    char::from_u32('\u{0966}' as u32 + (c as u32 - '0' as u32)).unwrap_or(c)
                } else {
                    c
                }
            })
            .collect(),
    }
}

fn ordinal_suffix(value: u64) -> &'static str {
    match value % 100 {
        11..=13 => "th",
        _ => match value % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        },
    }
}

/// The scan state: tokens with byte spans into the source, so untouched
/// text (including runs of spaces and newline layout) survives
/// byte-for-byte and only matched spans are rewritten.
struct Scan<'a> {
    text: &'a str,
    tokens: Vec<Token<'a>>,
    /// Byte offset where each token starts.
    starts: Vec<usize>,
    kinds: Vec<Option<Kind>>,
}

impl<'a> Scan<'a> {
    fn new(text: &'a str, script: NumberScript) -> Self {
        let mut tokens = Vec::new();
        let mut starts = Vec::new();
        let mut cursor = 0usize;
        for word in text.split_whitespace() {
            // split_whitespace yields non-overlapping slices in order; find
            // each word's byte offset by advancing the cursor.
            let Some(offset) = text[cursor..].find(word) else {
                break;
            };
            let start = cursor + offset;
            cursor = start + word.len();
            starts.push(start);
            tokens.push(Token::new(word, script));
        }
        let kinds = tokens.iter().map(|token| token.kind).collect();
        Scan {
            text,
            tokens,
            starts,
            kinds,
        }
    }

    fn end_of(&self, index: usize) -> usize {
        self.starts[index] + self.tokens[index].text.len()
    }

    /// Whether tokens [index, index + words) form one contiguous phrase:
    /// every token classified, no punctuation INSIDE the span (leading
    /// punctuation is fine on the first word, trailing punctuation on the
    /// last), and no newline in the glue between words (a phrase never
    /// spans lines). This is what keeps list speech ("one, two, three")
    /// and parenthetical breaks ("twenty (five") from collapsing.
    fn admissible(&self, index: usize, words: usize) -> bool {
        if index + words > self.tokens.len() {
            return false;
        }
        for offset in 0..words {
            let token = &self.tokens[index + offset];
            if token.kind.is_none() {
                return false;
            }
            if offset > 0 && !token.leading_punct.is_empty() {
                return false;
            }
            if offset + 1 < words && !token.trailing_punct.is_empty() {
                return false;
            }
            if offset + 1 < words {
                let glue = &self.text[self.end_of(index + offset)..self.starts[index + offset + 1]];
                if glue.contains('\n') {
                    return false;
                }
            }
        }
        true
    }

    /// Length of the positional digit run starting at `index` (0 when the
    /// token is not a digit word). Stops at an unclassified token, at
    /// punctuation between words (a token's trailing punctuation closes
    /// the run AFTER that token, so "one, two" never pairs), at a newline
    /// in the glue, and at a leading-punctuated token.
    fn digit_run(&self, index: usize) -> usize {
        if !is_digit_word(&self.kinds[index]) {
            return 0;
        }
        let mut end = index + 1;
        while end < self.tokens.len() {
            if !self.tokens[end - 1].trailing_punct.is_empty() {
                break;
            }
            let token = &self.tokens[end];
            if !is_digit_word(&self.kinds[end]) {
                break;
            }
            if !token.leading_punct.is_empty() {
                break;
            }
            let glue = &self.text[self.end_of(end - 1)..self.starts[end]];
            if glue.contains('\n') {
                break;
            }
            end += 1;
            if !token.trailing_punct.is_empty() {
                break;
            }
        }
        end - index
    }

    /// Whether the token at `index` is a bare single letter ("f", "B"),
    /// punctuation stripped: the dictated tail of a hex or version
    /// identifier ("zero zero one f" reads as "001f", so its digit run
    /// must stay words rather than paste "001 f" into code).
    fn single_letter_follows(&self, index: usize) -> bool {
        match self.tokens.get(index) {
            Some(token) => {
                let core = token.text.trim_matches(|c: char| !c.is_alphanumeric());
                core.chars().count() == 1 && core.chars().all(char::is_alphabetic)
            }
            None => false,
        }
    }

    /// Positional digits for a run of Digit/Unit words ("zero zero one"
    /// -> "001": leading zeros preserved by rendering digit by digit).
    fn run_digits(&self, index: usize, words: usize) -> String {
        self.kinds[index..index + words]
            .iter()
            .map(|kind| match kind {
                Some(Kind::Digit(value)) | Some(Kind::Unit(value)) => {
                    char::from_digit(*value as u32, 10).unwrap_or('0')
                }
                _ => '0',
            })
            .collect()
    }
}

/// Fail-open wrapper for the interim (overlay) invocation, which runs
/// outside the finalize pipeline's own fail-open transform: a bug in the
/// converter must fall back to the raw text rather than eat the display.
/// The converter is total by construction; this is the second net.
pub fn convert_number_words_fail_open(
    text: String,
    mode: NumberFormat,
    script: NumberScript,
) -> String {
    let fallback = text.clone();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        convert_number_words(&text, mode, script)
    })) {
        Ok(converted) => converted,
        Err(payload) => {
            log::error!(
                "Spoken-number formatting panicked: {}; using the raw text",
                crate::utils::panic_payload_message(payload.as_ref())
            );
            fallback
        }
    }
}

/// Convert spoken number words to digits. Pure and total: any input
/// (empty, adversarial, mixed-script) returns without panicking, and
/// `AsTranscribed` returns the input unchanged.
pub fn convert_number_words(text: &str, mode: NumberFormat, script: NumberScript) -> String {
    if mode == NumberFormat::AsTranscribed || text.is_empty() {
        return text.to_string();
    }
    let smart = mode == NumberFormat::Smart;

    let scan = Scan::new(text, script);
    if scan.tokens.is_empty() {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len());
    let mut resume = 0usize; // byte offset in `text` already emitted
    let mut i = 0usize;

    while i < scan.tokens.len() {
        if scan.tokens[i].kind.is_none() {
            i += 1;
            continue;
        }

        // A conversion never STARTS right after another number word: the
        // words before it belong to a larger phrase this grammar could not
        // parse ("four twenty five" is not English cardinal order), and
        // converting the tail half would paste mixed garbage ("four 25").
        let blocked_by_preceding_number = i > 0 && is_number_word(&scan.kinds[i - 1]);

        // ---- Rule 1 (Smart): compound ordinals, "twenty fifth" -> 25th.
        // Simple ordinals never convert ("first place", "a second");
        // only a cardinal phrase plus a final ordinal word does, and the
        // date around it is never reordered.
        if smart && !blocked_by_preceding_number && !matches!(scan.kinds[i], Some(Kind::Ordinal(_)))
        {
            if let Some(card) = parse_cardinal(&scan.kinds[i..]) {
                let next_kind = scan.kinds.get(i + card.words).copied().flatten();
                if let Some(Kind::Ordinal(ordinal_value)) = next_kind {
                    // Fold the ordinal back in as its group value so the
                    // grammar itself rejects nonsense ("five fifth").
                    let folded = match u64::from(ordinal_value) {
                        value if value < 10 => Some(Kind::Unit(ordinal_value as u8)),
                        value if value < 20 => Some(Kind::Teen(ordinal_value)),
                        value if value.is_multiple_of(10) => Some(Kind::Tens(ordinal_value)),
                        _ => None,
                    };
                    if let Some(folded) = folded {
                        let words = card.words + 1;
                        if scan.admissible(i, words) {
                            let mut extended: Vec<Option<Kind>> =
                                scan.kinds[i..i + card.words].to_vec();
                            extended.push(Some(folded));
                            if let Some(full) = parse_cardinal(&extended) {
                                if full.words == words {
                                    let value = full.value;
                                    let replacement = format!("{}{}", value, ordinal_suffix(value));
                                    emit(&mut out, &scan, resume, i, words, &replacement, script);
                                    resume = scan.end_of(i + words - 1);
                                    i += words;
                                    continue;
                                }
                            }
                        }
                    }
                }
            }
        }

        // ---- Rule 2: point decimals. "point" converts only flanked by
        // parseable number words, so "good point", "point taken" and "one
        // point to note" never fire.
        if let Some((replacement, words)) = try_point_decimal(&scan, i) {
            if scan.admissible(i, words) {
                emit(&mut out, &scan, resume, i, words, &replacement, script);
                resume = scan.end_of(i + words - 1);
                i += words;
                continue;
            }
        }

        // ---- Rule 3: compound cardinals ("twenty five" -> 25, "one
        // hundred forty two thousand fifteen" -> 142015, "ek sau" -> 100).
        // Walk the longest admissible prefix down to the shortest
        // convertible one: punctuation inside the phrase shortens the
        // match instead of blocking it ("two thousand, and five"
        // converts "two thousand"). A single word converts only when it
        // is a hyphenated tens-unit compound, which is two spoken number
        // words in one token.
        let mut converted_cardinal = false;
        if !blocked_by_preceding_number {
            if let Some(card) = parse_cardinal(&scan.kinds[i..]) {
                let mut words = card.words;
                while words >= 1 {
                    if !scan.admissible(i, words) {
                        words -= 1;
                        continue;
                    }
                    let Some(prefix) = parse_cardinal(&scan.kinds[i..i + words]) else {
                        break;
                    };
                    if prefix.words < words {
                        // The shorter slice stops earlier (an "and" waiting
                        // for its group): continue from there.
                        words = prefix.words;
                        continue;
                    }
                    let convertible =
                        words >= 2 || matches!(scan.kinds[i], Some(Kind::HyphenGroup(_)));
                    if convertible {
                        let replacement = prefix.value.to_string();
                        emit(&mut out, &scan, resume, i, words, &replacement, script);
                        resume = scan.end_of(i + words - 1);
                        i += words;
                        converted_cardinal = true;
                    }
                    break;
                }
            }
        }
        if converted_cardinal {
            continue;
        }

        // ---- Rule 4: digit-sequence collapse ("one eight zero one" ->
        // 1801), preserving leading zeros. A lone digit word never
        // converts; "oh" lives only inside a run. A run followed by a bare
        // single letter stays words: that is a dictated identifier tail
        // ("zero zero one f" is "001f", and pasting "001 f" into code
        // would corrupt it).
        let run = scan.digit_run(i);
        if run >= 2 && !blocked_by_preceding_number && !scan.single_letter_follows(i + run) {
            let digits = scan.run_digits(i, run);
            emit(&mut out, &scan, resume, i, run, &digits, script);
            resume = scan.end_of(i + run - 1);
            i += run;
            continue;
        }

        // ---- Rule 5 (Smart): isolated single-word cardinals of eleven
        // and above ("eleven" -> 11). "ten" stays a word with the other
        // lone units (it heads the one..ten family the guards protect).
        // A word flanked by other number words is left alone: it may open
        // a larger phrase ("four twenty five" stays words).
        if smart {
            let isolated = (i == 0 || !is_number_word(&scan.kinds[i - 1]))
                && (i + 1 >= scan.tokens.len() || !is_number_word(&scan.kinds[i + 1]));
            if isolated {
                if let Some(value) = match scan.kinds[i] {
                    Some(Kind::Teen(value)) if value >= 11 => Some(u64::from(value)),
                    Some(Kind::Tens(value)) => Some(u64::from(value)),
                    _ => None,
                } {
                    let replacement = value.to_string();
                    emit(&mut out, &scan, resume, i, 1, &replacement, script);
                    resume = scan.end_of(i);
                    i += 1;
                    continue;
                }
            }
        }

        i += 1;
    }

    out.push_str(&text[resume.min(text.len())..]);
    out
}

/// Point-decimal parse starting at `index`: a cardinal or positional digit
/// run, the word "point", then one or more simple number words
/// concatenated positionally ("one four one five" -> 1415; "twenty three"
/// -> 23). None when "point" is not flanked by number words.
fn try_point_decimal(scan: &Scan, index: usize) -> Option<(String, usize)> {
    // Left side: a cardinal value ("one", "twenty five") when a "point"
    // follows its last word, else a positional digit run ("one eight
    // zero", "zero") with the same property.
    let cardinal_lhs = parse_cardinal(&scan.kinds[index..]).and_then(|card| {
        matches!(scan.kinds.get(index + card.words), Some(Some(Kind::Point)))
            .then(|| (card.value.to_string(), card.words))
    });
    let (lhs_digits, lhs_words) = match cardinal_lhs {
        Some(lhs) => lhs,
        None => {
            let run = scan.digit_run(index);
            if run >= 1 && matches!(scan.kinds.get(index + run), Some(Some(Kind::Point))) {
                (scan.run_digits(index, run), run)
            } else {
                return None;
            }
        }
    };

    let point_at = index + lhs_words;
    if !matches!(scan.kinds.get(point_at), Some(Some(Kind::Point))) {
        return None;
    }

    let mut rhs = String::new();
    let mut j = point_at + 1;
    while j < scan.tokens.len() && scan.tokens[j].kind.is_some() {
        match scan.kinds[j] {
            Some(Kind::Digit(value)) | Some(Kind::Unit(value)) => {
                rhs.push(char::from_digit(value as u32, 10).unwrap_or('0'));
            }
            Some(Kind::Teen(value)) | Some(Kind::HyphenGroup(value)) => {
                rhs.push_str(&value.to_string());
            }
            Some(Kind::Tens(value)) => {
                if let Some(Some(Kind::Unit(unit))) = scan.kinds.get(j + 1) {
                    rhs.push_str(&(value + u16::from(*unit)).to_string());
                    j += 1;
                } else {
                    rhs.push_str(&value.to_string());
                }
            }
            _ => break,
        }
        j += 1;
    }

    if rhs.is_empty() {
        return None;
    }
    Some((format!("{lhs_digits}.{rhs}"), j - index))
}

/// Append the matched phrase [index, index + words): the glue before it,
/// the edge punctuation of the first/last tokens, and the replacement
/// rendered in the target script's digits.
fn emit(
    out: &mut String,
    scan: &Scan,
    resume: usize,
    index: usize,
    words: usize,
    replacement: &str,
    script: NumberScript,
) {
    let start = scan.starts[index];
    out.push_str(&scan.text[resume..start]);
    out.push_str(scan.tokens[index].leading_punct);
    out.push_str(&render_digits(replacement, script));
    out.push_str(scan.tokens[index + words - 1].trailing_punct);
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGITS: NumberFormat = NumberFormat::Digits;
    const SMART: NumberFormat = NumberFormat::Smart;
    const OFF: NumberFormat = NumberFormat::AsTranscribed;

    #[test]
    fn digit_sequence_collapse() {
        assert_eq!(
            convert_number_words("one eight zero one", DIGITS, NumberScript::English),
            "1801"
        );
        assert_eq!(
            convert_number_words(
                "pull request one one zero five",
                DIGITS,
                NumberScript::English
            ),
            "pull request 1105"
        );
        assert_eq!(
            convert_number_words("five zero zero", DIGITS, NumberScript::English),
            "500"
        );
        assert_eq!(
            convert_number_words("zero zero one", DIGITS, NumberScript::English),
            "001"
        );
        assert_eq!(
            convert_number_words("one oh one", DIGITS, NumberScript::English),
            "101"
        );
        // A lone "oh" never converts.
        assert_eq!(
            convert_number_words("oh well", DIGITS, NumberScript::English),
            "oh well"
        );
        assert_eq!(
            convert_number_words("oh", DIGITS, NumberScript::English),
            "oh"
        );
    }

    #[test]
    fn lone_unit_words_untouched_in_every_mode() {
        for mode in [DIGITS, SMART] {
            for text in [
                "I have one question",
                "no one knows",
                "someone anyone everyone",
                "give me five",
                "nine out of ten",
                "one should always",
                "give me a second",
                "ten",
            ] {
                assert_eq!(
                    convert_number_words(text, mode, NumberScript::English),
                    text,
                    "mode: {mode:?}, text: {text}"
                );
            }
        }
    }

    #[test]
    fn compound_cardinals() {
        assert_eq!(
            convert_number_words("twenty five", DIGITS, NumberScript::English),
            "25"
        );
        assert_eq!(
            convert_number_words(
                "one hundred forty two thousand fifteen",
                DIGITS,
                NumberScript::English
            ),
            "142015"
        );
        assert_eq!(
            convert_number_words("two thousand and five", DIGITS, NumberScript::English),
            "2005"
        );
        assert_eq!(
            convert_number_words("forty-two", DIGITS, NumberScript::English),
            "42"
        );
        assert_eq!(
            convert_number_words("a million", DIGITS, NumberScript::English),
            "a million"
        );
        assert_eq!(
            convert_number_words("millions of stars", DIGITS, NumberScript::English),
            "millions of stars"
        );
        assert_eq!(
            convert_number_words("eleven hundred", DIGITS, NumberScript::English),
            "1100"
        );
        assert_eq!(
            convert_number_words("twenty five items", DIGITS, NumberScript::English),
            "25 items"
        );
        assert_eq!(
            convert_number_words("Twenty five items", DIGITS, NumberScript::English),
            "25 items"
        );
        // Isolated teens/tens stay words in Digits mode.
        assert_eq!(
            convert_number_words("eleven", DIGITS, NumberScript::English),
            "eleven"
        );
        assert_eq!(
            convert_number_words("chapter twenty", DIGITS, NumberScript::English),
            "chapter twenty"
        );
    }

    #[test]
    fn point_decimals_and_guards() {
        assert_eq!(
            convert_number_words("version one point two", DIGITS, NumberScript::English),
            "version 1.2"
        );
        assert_eq!(
            convert_number_words(
                "three point one four one five",
                DIGITS,
                NumberScript::English
            ),
            "3.1415"
        );
        assert_eq!(
            convert_number_words("twenty five point six", DIGITS, NumberScript::English),
            "25.6"
        );
        assert_eq!(
            convert_number_words("one eight zero point five", DIGITS, NumberScript::English),
            "180.5"
        );
        for text in [
            "good point",
            "one point to note",
            "point taken",
            "what is your point",
        ] {
            assert_eq!(
                convert_number_words(text, DIGITS, NumberScript::English),
                text,
                "text: {text}"
            );
        }
    }

    #[test]
    fn smart_compound_ordinals() {
        assert_eq!(
            convert_number_words("the twenty fifth of March", SMART, NumberScript::English),
            "the 25th of March"
        );
        assert_eq!(
            convert_number_words("twenty first", SMART, NumberScript::English),
            "21st"
        );
        assert_eq!(
            convert_number_words("the thirty second row", SMART, NumberScript::English),
            "the 32nd row"
        );
        assert_eq!(
            convert_number_words("one hundred fifth", SMART, NumberScript::English),
            "105th"
        );
        // Simple ordinals never convert, in any mode.
        for mode in [DIGITS, SMART] {
            for text in [
                "first place",
                "second nature",
                "third party",
                "a second",
                "the tenth attempt",
                "fifth",
            ] {
                assert_eq!(
                    convert_number_words(text, mode, NumberScript::English),
                    text,
                    "mode: {mode:?}, text: {text}"
                );
            }
        }
    }

    #[test]
    fn smart_isolated_cardinals_over_threshold() {
        assert_eq!(
            convert_number_words("eleven", SMART, NumberScript::English),
            "11"
        );
        assert_eq!(
            convert_number_words("nineteen", SMART, NumberScript::English),
            "19"
        );
        assert_eq!(
            convert_number_words("chapter thirty", SMART, NumberScript::English),
            "chapter 30"
        );
        // Below the threshold, and neighbors that are number words, stay
        // words.
        assert_eq!(
            convert_number_words("four twenty five", SMART, NumberScript::English),
            "four twenty five"
        );
        assert_eq!(
            convert_number_words("ten", SMART, NumberScript::English),
            "ten"
        );
    }

    #[test]
    fn existing_digits_and_identifiers_byte_identical() {
        for mode in [DIGITS, SMART] {
            for text in [
                "room 4B",
                "iPhone 15",
                "0x1F",
                "base64",
                "IPv4",
                "GPT 4",
                "PR 1105 already digits",
                "deadbeef cafe face fad",
                "zero zero one f",
                "won't do it",
                "a one-two punch",
                "404 not found",
            ] {
                assert_eq!(
                    convert_number_words(text, mode, NumberScript::English),
                    text,
                    "mode: {mode:?}, text: {text}"
                );
            }
        }
    }

    #[test]
    fn list_speech_guard() {
        for mode in [DIGITS, SMART] {
            assert_eq!(
                convert_number_words("one, two, three", mode, NumberScript::English),
                "one, two, three",
                "mode: {mode:?}"
            );
            assert_eq!(
                convert_number_words("pick one, or two", mode, NumberScript::English),
                "pick one, or two",
                "mode: {mode:?}"
            );
            // "and" also interrupts a run.
            assert_eq!(
                convert_number_words("one and zero", mode, NumberScript::English),
                "one and zero",
                "mode: {mode:?}"
            );
        }
    }

    #[test]
    fn off_mode_is_byte_identical() {
        for text in [
            "one eight zero one",
            "pull request one one zero five",
            "twenty five",
            "version one point two",
            "the twenty fifth of March",
            "room 4B",
            "one, two, three",
            "  hello   world  ",
            "line one\nline two",
        ] {
            assert_eq!(
                convert_number_words(text, OFF, NumberScript::English),
                text,
                "text: {text}"
            );
            assert_eq!(
                convert_number_words(text, OFF, NumberScript::Devanagari),
                text,
                "text: {text}"
            );
        }
    }

    #[test]
    fn layout_and_untouched_text_survive_byte_for_byte() {
        // Multi-space runs and newline layout outside matches survive.
        assert_eq!(
            convert_number_words("hello  one eight  world", DIGITS, NumberScript::English),
            "hello  18  world"
        );
        assert_eq!(
            convert_number_words("one\ntwo", DIGITS, NumberScript::English),
            "one\ntwo",
            "a run never crosses a newline"
        );
        assert_eq!(
            convert_number_words("twenty\nfive items", DIGITS, NumberScript::English),
            "twenty\nfive items"
        );
        // Trailing punctuation on the final phrase word converts with it.
        assert_eq!(
            convert_number_words("It costs twenty five.", DIGITS, NumberScript::English),
            "It costs 25."
        );
        assert_eq!(
            convert_number_words("(twenty five)", DIGITS, NumberScript::English),
            "(25)"
        );
        // A comma mid-phrase shortens the match instead of blocking it.
        assert_eq!(
            convert_number_words("two thousand, and five", DIGITS, NumberScript::English),
            "2000, and five"
        );
        // Empty and whitespace-only text.
        assert_eq!(convert_number_words("", DIGITS, NumberScript::English), "");
        assert_eq!(
            convert_number_words("   ", DIGITS, NumberScript::English),
            "   "
        );
    }

    #[test]
    fn hindi_devanagari_conversion() {
        assert_eq!(
            convert_number_words("एक आठ शून्य एक", DIGITS, NumberScript::Devanagari),
            "१८०१"
        );
        assert_eq!(
            convert_number_words("एक सौ", DIGITS, NumberScript::Devanagari),
            "१००"
        );
        assert_eq!(
            convert_number_words("पंद्रह लाख", DIGITS, NumberScript::Devanagari),
            "१५०००००"
        );
        assert_eq!(
            convert_number_words("एक लाख पचास हज़ार", DIGITS, NumberScript::Devanagari),
            "१५००००"
        );
        // Lone "ek" is the indefinite article; never converts.
        assert_eq!(
            convert_number_words("एक फ़िल्म देखी", DIGITS, NumberScript::Devanagari),
            "एक फ़िल्म देखी"
        );
        assert_eq!(
            convert_number_words("दो काम", DIGITS, NumberScript::Devanagari),
            "दो काम"
        );
        // Latin fragments in code-mixed lines pass through verbatim; only
        // the Devanagari number words convert. An isolated tens word
        // follows the same mode rules as English ("twenty" stays a word
        // in Digits, converts in Smart).
        assert_eq!(
            convert_number_words("meeting तीस बजे", DIGITS, NumberScript::Devanagari),
            "meeting तीस बजे"
        );
        assert_eq!(
            convert_number_words("meeting तीस बजे", SMART, NumberScript::Devanagari),
            "meeting ३० बजे"
        );
        // Romanized Hindi number words never match the English grammar.
        assert_eq!(
            convert_number_words("ek do teen char", DIGITS, NumberScript::English),
            "ek do teen char"
        );
        // Smart isolated teens/tens convert in Devanagari too.
        assert_eq!(
            convert_number_words("ग्यारह", SMART, NumberScript::Devanagari),
            "११"
        );
    }

    #[test]
    fn english_grammar_never_matches_devanagari_and_vice_versa() {
        assert_eq!(
            convert_number_words("एक सौ", DIGITS, NumberScript::English),
            "एक सौ"
        );
        assert_eq!(
            convert_number_words("twenty five", DIGITS, NumberScript::Devanagari),
            "twenty five"
        );
    }

    #[test]
    fn adversarial_inputs_never_panic_or_empty_out() {
        // 50+ digit-word run, mixed scripts, punctuation noise: total
        // function, non-empty output for non-empty input.
        let long_run = "one ".repeat(60) + "zero";
        let converted = convert_number_words(&long_run, DIGITS, NumberScript::English);
        assert!(!converted.is_empty());
        assert!(converted.chars().all(|c| c.is_ascii_digit()));

        let mixed = "एक twenty सौ five शून्य point आठ";
        for script in [NumberScript::English, NumberScript::Devanagari] {
            let converted = convert_number_words(mixed, SMART, script);
            assert!(!converted.is_empty());
        }

        let noise = "!!! ??? ... (( )) one)) ((two";
        let converted = convert_number_words(noise, SMART, NumberScript::English);
        assert!(!converted.is_empty());
    }

    #[test]
    fn saturated_values_stay_safe() {
        let huge = "nine hundred ninety nine billion nine hundred ninety nine million nine hundred ninety nine thousand nine hundred ninety nine";
        let converted = convert_number_words(huge, DIGITS, NumberScript::English);
        assert!(converted.chars().all(|c| c.is_ascii_digit() || c == ' '));
        // A run past u64 range saturates rather than panicking.
        let over = "two million billion trillion".to_string();
        assert!(!convert_number_words(&over, DIGITS, NumberScript::English).is_empty());
    }

    #[test]
    fn gating_resolves_script_passes_from_evidence() {
        let mut settings = AppSettings::default();
        let hi = OutputLanguageEvidence::UserSelected("hi".to_string());
        let en = OutputLanguageEvidence::UserSelected("en".to_string());
        let unknown = OutputLanguageEvidence::Unknown;

        assert_eq!(
            number_pass_scripts(&settings, &en),
            NumberPassScripts {
                devanagari: false,
                english: true
            }
        );
        assert_eq!(
            number_pass_scripts(&settings, &hi),
            NumberPassScripts {
                devanagari: true,
                english: false
            }
        );
        // Unknown evidence fails closed.
        assert_eq!(
            number_pass_scripts(&settings, &unknown),
            NumberPassScripts::default()
        );

        // The hi-Latn script intent guarantees Devanagari model output:
        // both passes run (the English pass sees the romanized text).
        settings.selected_language = "hi-Latn".to_string();
        assert_eq!(
            number_pass_scripts(&settings, &unknown),
            NumberPassScripts {
                devanagari: true,
                english: true
            }
        );

        // Region subtags and translation evidence resolve by base language.
        settings.selected_language = "auto".to_string();
        assert_eq!(
            number_pass_scripts(
                &settings,
                &OutputLanguageEvidence::ModelDetected("en-US".to_string())
            )
            .english,
            true
        );
        assert_eq!(
            number_pass_scripts(&settings, &OutputLanguageEvidence::TranslatedToEnglish).english,
            true
        );
    }
}
