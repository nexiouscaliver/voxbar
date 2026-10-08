//! Deterministic Devanagari-to-Roman transliteration for Hinglish output.
//!
//! Selected language "hi-Latn" (Hinglish, Roman script) expresses a SCRIPT
//! intent: the model still transcribes Hindi speech as Devanagari (no
//! catalog model advertises Roman-script Hindi output), so the pipeline
//! transliterates the Devanagari text to Roman after the script slot and
//! before every text pass, and the interim overlay does the same so the
//! live display matches the paste (WYSIWYG). This is script conversion,
//! the same class as `chinese_script`, never translation.
//!
//! Scope, stated honestly: pragmatic romanization with word-final
//! inherent-vowel drop and common matra and conjunct handling. Legible
//! Hinglish, not ISO-15919-exact; schwa deletion beyond the word-final
//! position and cycle-final drop are out of scope. Only the Devanagari
//! block (U+0900 to U+097F) is mapped; every other code point passes
//! through verbatim, so code-mixed speech keeps its Latin fragments
//! Latin. Hand-rolled table, no new dependency.

/// Whether a code point is in the Devanagari block this module maps.
fn is_devanagari(c: char) -> bool {
    ('\u{0900}'..='\u{097F}').contains(&c)
}

/// Consonant base forms (U+0915 to U+0939 plus the extras). The inherent
/// vowel is handled by the caller, not the table.
fn consonant_form(c: char) -> Option<&'static str> {
    Some(match c {
        '\u{0915}' => "k",   // क
        '\u{0916}' => "kh",  // ख
        '\u{0917}' => "g",   // ग
        '\u{0918}' => "gh",  // घ
        '\u{0919}' => "n",   // ङ
        '\u{091A}' => "ch",  // च
        '\u{091B}' => "chh", // छ
        '\u{091C}' => "j",   // ज
        '\u{091D}' => "jh",  // झ
        '\u{091E}' => "n",   // ञ
        '\u{091F}' => "t",   // ट
        '\u{0920}' => "th",  // ठ
        '\u{0921}' => "d",   // ड
        '\u{0922}' => "dh",  // ढ
        '\u{0923}' => "n",   // ण
        '\u{0924}' => "t",   // त
        '\u{0925}' => "th",  // थ
        '\u{0926}' => "d",   // द
        '\u{0927}' => "dh",  // ध
        '\u{0928}' => "n",   // न
        '\u{0929}' => "n",   // ऩ
        '\u{092A}' => "p",   // प
        '\u{092B}' => "ph",  // फ
        '\u{092C}' => "b",   // ब
        '\u{092D}' => "bh",  // भ
        '\u{092E}' => "m",   // म
        '\u{092F}' => "y",   // य
        '\u{0930}' => "r",   // र
        '\u{0931}' => "r",   // ऱ
        '\u{0932}' => "l",   // ल
        '\u{0933}' => "l",   // ळ
        '\u{0934}' => "l",   // ऴ
        '\u{0935}' => "v",   // व
        '\u{0936}' => "sh",  // श
        '\u{0937}' => "sh",  // ष
        '\u{0938}' => "s",   // स
        '\u{0939}' => "h",   // ह
        _ => return None,
    })
}

/// The nukta (and precomposed) forms of the common consonants.
fn nukta_form(base: &str) -> Option<&'static str> {
    Some(match base {
        "k" => "q",   // क़
        "kh" => "kh", // ख़
        "g" => "g",   // ग़
        "j" => "z",   // ज़
        "d" => "r",   // ड़
        "dh" => "rh", // ढ़
        "ph" => "f",  // फ़
        _ => None?,
    })
}

/// Independent vowel letters.
fn independent_vowel_form(c: char) -> Option<&'static str> {
    Some(match c {
        '\u{0905}' => "a",  // अ
        '\u{0906}' => "aa", // आ
        '\u{0907}' => "i",  // इ
        '\u{0908}' => "ee", // ई
        '\u{0909}' => "u",  // उ
        '\u{090A}' => "oo", // ऊ
        '\u{090B}' => "ri", // ऋ
        '\u{090C}' => "l",  // ऌ
        '\u{090F}' => "e",  // ए
        '\u{0910}' => "ai", // ऐ
        '\u{0911}' => "o",  // ऑ
        '\u{0912}' => "au", // औ
        _ => return None,
    })
}

/// Vowel matras. A matra REPLACES the consonant's inherent vowel.
fn matra_form(c: char) -> Option<&'static str> {
    Some(match c {
        '\u{093E}' => "aa", // ा
        '\u{093F}' => "i",  // ि
        '\u{0940}' => "ee", // ी
        '\u{0941}' => "u",  // ु
        '\u{0942}' => "oo", // ू
        '\u{0943}' => "ri", // ृ
        '\u{0944}' => "ri", // ॄ
        '\u{0945}' => "e",  // ॅ
        '\u{0947}' => "e",  // े
        '\u{0948}' => "ai", // ै
        '\u{0949}' => "o",  // ॉ
        '\u{094B}' => "o",  // ो
        '\u{094C}' => "au", // ौ
        _ => return None,
    })
}

/// The precomposed nukta consonants (U+0958 to U+095F).
fn precomposed_nukta_form(c: char) -> Option<&'static str> {
    Some(match c {
        '\u{0958}' => "q",  // क़
        '\u{0959}' => "kh", // ख़
        '\u{095A}' => "g",  // ग़
        '\u{095B}' => "z",  // ज़
        '\u{095C}' => "r",  // ड़
        '\u{095D}' => "rh", // ढ़
        '\u{095E}' => "f",  // फ़
        '\u{095F}' => "y",  // य़
        _ => return None,
    })
}

/// Word-scoped transliteration state: the romanized word so far and
/// whether its last consonant still owes its inherent "a" (cancelled by a
/// matra or a virama, emitted before the next consonant, DROPPED at the
/// word's end).
struct WordTransliterator {
    out: String,
    pending_inherent_a: bool,
    /// Byte offset in `out` where the last consonant base starts, for
    /// nukta replacement.
    last_consonant_at: Option<usize>,
}

impl WordTransliterator {
    fn new() -> Self {
        Self {
            out: String::new(),
            pending_inherent_a: false,
            last_consonant_at: None,
        }
    }

    fn flush_inherent_a(&mut self) {
        if self.pending_inherent_a {
            self.out.push('a');
            self.pending_inherent_a = false;
        }
    }

    /// Transliterate one Devanagari code point into the word buffer.
    fn push(&mut self, c: char) {
        // The nukta branch needs the PREVIOUS code point's consonant, so
        // take (rather than clear) the tracked offset up front.
        let previous_consonant_at = self.last_consonant_at.take();
        if let Some(form) = consonant_form(c) {
            self.flush_inherent_a();
            self.last_consonant_at = Some(self.out.len());
            self.out.push_str(form);
            self.pending_inherent_a = true;
        } else if let Some(form) = precomposed_nukta_form(c) {
            self.flush_inherent_a();
            self.out.push_str(form);
            self.pending_inherent_a = true;
        } else if let Some(form) = matra_form(c) {
            // Replaces the inherent vowel entirely.
            self.pending_inherent_a = false;
            self.out.push_str(form);
        } else if c == '\u{094D}' {
            // Virama: silences the preceding consonant's inherent vowel
            // and forms the conjunct with whatever follows.
            self.pending_inherent_a = false;
        } else if let Some(form) = independent_vowel_form(c) {
            self.flush_inherent_a();
            self.out.push_str(form);
        } else if c == '\u{0902}' || c == '\u{0901}' {
            // Anusvara / chandrabindu follow the vowel they nasalize.
            self.flush_inherent_a();
            self.out.push('n');
        } else if c == '\u{0903}' {
            self.flush_inherent_a();
            self.out.push('h');
        } else if c == '\u{093C}' {
            // Nukta: rewrite the last consonant to its nukta form when it
            // has one. The pending inherent vowel survives.
            if let Some(at) = previous_consonant_at {
                if let Some(base) = self.out.get(at..) {
                    if let Some(form) = nukta_form(base) {
                        self.out.truncate(at);
                        self.out.push_str(form);
                    }
                }
            }
        } else if let digit @ '\u{0966}'..='\u{096F}' = c {
            // Devanagari digits map to ASCII digits.
            let ascii = ((digit as u32 - '\u{0966}' as u32) + '0' as u32) as u8 as char;
            self.out.push(ascii);
        } else if c == '\u{0964}' || c == '\u{0965}' {
            // Danda / double danda read as sentence-ending periods.
            self.out.push('.');
        } else {
            // Unhandled block code points pass through verbatim.
            self.out.push(c);
        }
    }

    /// Finish the word: a trailing INHERENT vowel drops (schwa deletion,
    /// simply never emitted); a real matra vowel never does.
    fn finish(self) -> String {
        self.out
    }
}

/// Transliterate Devanagari script (U+0900 to U+097F) to a pragmatic
/// Roman form; every other code point passes through verbatim. Words are
/// flushed (and their final inherent vowel dropped) at the first
/// non-Devanagari code point and at the end of the text.
pub fn transliterate_devanagari_to_roman(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut word: Option<WordTransliterator> = None;
    for c in text.chars() {
        if is_devanagari(c) {
            let word = word.get_or_insert_with(WordTransliterator::new);
            word.push(c);
        } else {
            if let Some(word) = word.take() {
                out.push_str(&word.finish());
            }
            out.push(c);
        }
    }
    if let Some(word) = word.take() {
        out.push_str(&word.finish());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hinglish_transliteration_word_list() {
        // Word-final inherent-vowel drop + matras + conjuncts.
        assert_eq!(transliterate_devanagari_to_roman("नमस्ते"), "namaste");
        assert_eq!(transliterate_devanagari_to_roman("कम"), "kam");
        assert_eq!(transliterate_devanagari_to_roman("है"), "hai");
        assert_eq!(transliterate_devanagari_to_roman("क्या"), "kyaa");
        assert_eq!(transliterate_devanagari_to_roman("आप"), "aap");
        assert_eq!(transliterate_devanagari_to_roman("कैसे"), "kaise");
        assert_eq!(transliterate_devanagari_to_roman("नहीं"), "naheen");
        assert_eq!(transliterate_devanagari_to_roman("दिल्ली"), "dillee");
        // Anusvara nasalizes the vowel it follows.
        assert_eq!(transliterate_devanagari_to_roman("अंदर"), "andar");
        // Initial conjunct cluster.
        assert_eq!(transliterate_devanagari_to_roman("स्कूल"), "skool");
        // Nukta forms: ज़ -> z, फ़ -> f.
        assert_eq!(transliterate_devanagari_to_roman("ज़रा"), "zaraa");
        assert_eq!(transliterate_devanagari_to_roman("फ़िल्म"), "film");
        // Multi-word input drops the final vowel per word.
        assert_eq!(transliterate_devanagari_to_roman("कैसे हो"), "kaise ho");
        // A real matra at the word end never drops.
        assert_eq!(transliterate_devanagari_to_roman("चलो"), "chalo");
        // Danda reads as a period.
        assert_eq!(transliterate_devanagari_to_roman("चलो।"), "chalo.");
    }

    #[test]
    fn hinglish_code_mixed_input_passes_latin_through_untouched() {
        assert_eq!(
            transliterate_devanagari_to_roman("यह office है"),
            "yah office hai",
        );
        assert_eq!(
            transliterate_devanagari_to_roman("meeting ३० बजे"),
            "meeting 30 baje",
        );
        // Pure Latin and punctuation stay byte-for-byte.
        assert_eq!(
            transliterate_devanagari_to_roman("hello, world!"),
            "hello, world!"
        );
        assert_eq!(transliterate_devanagari_to_roman(""), "");
        // CJK passes through verbatim too (outside the block).
        assert_eq!(transliterate_devanagari_to_roman("你好"), "你好");
    }
}
