//! CJK-aware token/unit budgets and the memory forecast for the local LLM
//! (spec 1.2, section 4).
//!
//! Why script-aware math at all (reviewer finding R3): Chinese, Japanese,
//! and Korean text has no whitespace, so word-count caps and guards
//! silently mis-scope it, and per-character token density differs from
//! English by roughly 2x. Every cap and guard below is therefore defined
//! over a token ESTIMATE and a unit count that both count CJK per
//! character. They are budgeting heuristics: deterministic, monotonic, and
//! testable, never correctness claims.

/// Input cap (L10): transcripts whose estimated token count exceeds this
/// are skipped entirely (reason `too_long`, raw text out). This is the
/// tiny-model quality cliff, not just a context limit: 1200 est-tokens plus
/// the 256-token prompt/template reserve plus the 192-token output floor
/// stays well inside the worker's 4096-token context.
pub const INPUT_TOKEN_CAP: u64 = 1200;

/// Output floor for the token cap (L11).
pub const MIN_GEN_TOKENS: u32 = 192;

/// Output ceiling for the token cap (L11): large CJK inputs clamp here so a
/// faithful full-length cleanup at roughly one token per character still
/// fits the context instead of the grammar being forced to complete
/// truncated JSON that gets pasted.
pub const MAX_GEN_TOKENS: u32 = 2048;

/// File-size forecast multiplier (L5): forecast base = file size * 3/2.
/// File size alone undercounts llama.cpp's resident footprint (KV cache,
/// compute scratch, Metal buffers); 3/2 is the conservative named constant
/// and the measured-RSS refinement below corrects it after first use. A
/// wrong guess fails safe: the gate refuses and the raw transcript is used.
pub const FILE_SIZE_FORECAST_MULTIPLIER_NUM: u64 = 3;
pub const FILE_SIZE_FORECAST_MULTIPLIER_DEN: u64 = 2;

/// Count the CJK characters in `text`: CJK Unified Ideographs and Extension
/// A, Hiragana, Katakana, Hangul Syllables, and Hangul Jamo (spec 1.2
/// codepoint ranges).
pub fn cjk_char_count(text: &str) -> u64 {
    text.chars()
        .filter(|c| {
            let cp = *c as u32;
            // CJK Unified Ideographs, CJK Extension A, Hiragana, Katakana,
            // Hangul Syllables, Hangul Jamo (the spec 1.2 ranges).
            (0x4E00..=0x9FFF).contains(&cp)
                || (0x3400..=0x4DBF).contains(&cp)
                || (0x3040..=0x309F).contains(&cp)
                || (0x30A0..=0x30FF).contains(&cp)
                || (0xAC00..=0xD7AF).contains(&cp)
                || (0x1100..=0x11FF).contains(&cp)
        })
        .count() as u64
}

/// Deterministic token estimate: CJK counts roughly one token per
/// character, Latin scripts roughly four characters per token (the Qwen
/// tokenizer families this model belongs to). Monotonic in the input.
pub fn estimate_tokens(text: &str) -> u64 {
    let total = text.chars().count() as u64;
    let cjk = cjk_char_count(text);
    let non_cjk = total.saturating_sub(cjk);
    cjk + non_cjk / 4
}

/// Count of "units": CJK characters plus whitespace-separated words. This
/// is the fidelity-guard measure, so a script with no whitespace is counted
/// per character and never silently under-measured.
pub fn text_units(text: &str) -> u64 {
    cjk_char_count(text) + text.split_whitespace().count() as u64
}

/// The input cap decision (L10): true when local post-processing must be
/// skipped and the raw transcript returned.
pub fn input_exceeds_cap(text: &str) -> bool {
    estimate_tokens(text) > INPUT_TOKEN_CAP
}

/// The output token budget (L11): `clamp(2 * est_tokens(input) + 128,
/// 192, 2048)`.
pub fn max_gen_tokens(input: &str) -> u32 {
    let est = estimate_tokens(input);
    let raw = 2 * est as u128 + 128;
    let clamped = raw.clamp(MIN_GEN_TOKENS as u128, MAX_GEN_TOKENS as u128);
    clamped as u32
}

/// The length-collapse fidelity guard (L9): true when the cleaned output
/// collapsed below a strict 40% of the input's unit count AND the input was
/// long enough for that ratio to be meaningful (>= 20 units). Inputs below
/// the floor are exempt exactly as before. When true, the raw transcript is
/// used (reason `length_guard`).
///
/// `output` must already be the extracted, stripped text (think block
/// removed, JSON `transcription` field extracted): the caller applies those
/// belts first, so a think-only output reaches this function as empty and
/// trips the guard on any meaningful input.
pub fn fails_fidelity_guard(input: &str, output: &str) -> bool {
    let input_units = text_units(input);
    if input_units < 20 {
        return false;
    }
    let output_units = text_units(output);
    output_units.saturating_mul(10) < input_units.saturating_mul(4)
}

/// Runtime-inclusive memory forecast (L5): `max(file_size * 3/2,
/// measured_rss)`. `measured_rss` is the worker's RSS captured after the
/// first successful generation this app launch (in-memory only, never
/// persisted); `None` before that, when the 3/2 multiplier stands alone.
pub fn llm_forecast_bytes(file_size_bytes: u64, measured_rss: Option<u64>) -> u64 {
    let base = file_size_bytes.saturating_mul(FILE_SIZE_FORECAST_MULTIPLIER_NUM)
        / FILE_SIZE_FORECAST_MULTIPLIER_DEN;
    base.max(measured_rss.unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T12: cjk_char_count over the pure tables: ASCII, CJK ideographs
    /// (basic + Extension A), kana (hiragana + katakana), hangul (syllables
    /// + jamo), and mixed strings. Ranges that must NOT count: CJK
    /// punctuation, the CJK Compatibility block, fullwidth Latin.
    #[test]
    fn cjk_char_count_covers_the_spec_ranges_only() {
        assert_eq!(cjk_char_count(""), 0);
        assert_eq!(cjk_char_count("hello world"), 0);
        assert_eq!(cjk_char_count("1234 !?"), 0);
        // Ideographs and Extension A.
        assert_eq!(cjk_char_count("你好"), 2);
        assert_eq!(cjk_char_count("\u{3400}\u{4DBF}\u{4E00}\u{9FFF}"), 4);
        // Kana.
        assert_eq!(cjk_char_count("こんにちは"), 5);
        assert_eq!(cjk_char_count("\u{3040}\u{309F}\u{30A0}\u{30FF}"), 4);
        // Hangul syllables and jamo.
        assert_eq!(cjk_char_count("안녕하세요"), 5);
        assert_eq!(cjk_char_count("\u{1100}\u{11FF}"), 2);
        // Mixed scripts counted together.
        assert_eq!(cjk_char_count("你好 world こんにちは 안녕"), 9);
        // Outside the ranges: CJK punctuation and compatibility ideographs
        // do not count (they are punctuation/rare archaic forms, and the
        // spec's ranges are the contract).
        assert_eq!(cjk_char_count("、。「」"), 0);
        assert_eq!(cjk_char_count("\u{F900}"), 0);
    }

    /// T12: estimate_tokens tables. CJK at one token per char, Latin at
    /// four chars per token, mixed scripts summed.
    #[test]
    fn estimate_tokens_tables() {
        assert_eq!(estimate_tokens(""), 0);
        // Pure ASCII: 16 chars / 4 = 4.
        assert_eq!(estimate_tokens("aaaaaaaaaaaaaaaa"), 4);
        // Pure CJK: one per char, remainder-free.
        assert_eq!(estimate_tokens("你好世界測試"), 6);
        // Mixed: 4 CJK + 8 ASCII (8/4 = 2) = 6.
        assert_eq!(estimate_tokens("你好世界abcdefgh"), 6);
        // Non-CJK remainder truncates: 5 ASCII -> 1.
        assert_eq!(estimate_tokens("abcde"), 1);
        // Punctuation is a plain non-CJK char for budgeting.
        assert_eq!(estimate_tokens("a b c d!"), 2);
    }

    /// T12: text_units tables. CJK per character, Latin per whitespace
    /// word, mixed summed; script without whitespace never counts zero on
    /// real content.
    #[test]
    fn text_units_tables() {
        assert_eq!(text_units(""), 0);
        assert_eq!(text_units("hello world"), 2);
        assert_eq!(text_units("  hello   world  "), 2);
        // A whitespace-free string is ONE field for split_whitespace, so
        // pure CJK counts cjk_chars + 1: the per-character count dominates
        // and the constant offset cancels in the guard's input/output ratio.
        assert_eq!(text_units("你好世界"), 5);
        assert_eq!(text_units("你好 world こんにちは"), 10);
        assert_eq!(text_units("one"), 1);
    }

    /// T9: the fidelity guard, CJK-aware. A truncated zh cleanup trips it;
    /// a faithful one passes; short inputs are exempt; empty (or
    /// think-stripped-to-empty) output trips it on meaningful input; mixed
    /// zh+en is counted as cjk_chars + whitespace words.
    #[test]
    fn fidelity_guard_is_cjk_aware() {
        let zh_input: String = "字".repeat(500);
        // 150-char output: 150 * 10 = 1500 < 500 * 4 = 2000 -> fallback raw.
        let collapsed: String = "字".repeat(150);
        assert!(fails_fidelity_guard(&zh_input, &collapsed));
        // 480-char output: 4800 >= 2000 -> passes.
        let faithful: String = "字".repeat(480);
        assert!(!fails_fidelity_guard(&zh_input, &faithful));

        // English under 20 words is exempt even with a collapsed output.
        let short_en = "hello um world this is a test";
        assert!(text_units(short_en) < 20);
        assert!(!fails_fidelity_guard(short_en, "x"));
        // English at 20+ words with a collapsed output trips.
        let long_en = "one two three four five six seven eight nine ten eleven twelve \
                       thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty";
        assert!(text_units(long_en) >= 20);
        assert!(fails_fidelity_guard(long_en, "gone"));

        // Empty / think-only output folds to raw on meaningful input.
        assert!(fails_fidelity_guard(&zh_input, ""));
        assert!(fails_fidelity_guard(long_en, ""));

        // Mixed zh+en: 500 CJK chars + 30 words input vs a 190-CJK-char
        // output that dropped every Latin word still trips the guard
        // (1900 < (500 + 30) * 4 = 2120).
        let mixed_in = format!("{} {}", "字".repeat(500), "word ".repeat(30).trim_end());
        let mixed_out = "字".repeat(190);
        assert!(fails_fidelity_guard(&mixed_in, &mixed_out));
    }

    /// T10: the input cap, CJK-aware. A 10000-char zh input (est 10000
    /// tokens) trips; an English 1200-word input (~4800 chars, est 1200)
    /// sits exactly at the boundary and passes; one more word skips.
    #[test]
    fn input_cap_is_token_estimated_and_cjk_aware() {
        let zh_10k: String = "字".repeat(10_000);
        assert!(input_exceeds_cap(&zh_10k));

        // Exactly at the cap: 1200 four-char words = 4800 chars -> est
        // 1200, which is <= the cap, so it passes.
        let at_boundary = "word".repeat(1200);
        assert_eq!(estimate_tokens(&at_boundary), 1200);
        assert!(!input_exceeds_cap(&at_boundary));

        // One word over: est 1201 > 1200 -> skip.
        let over_boundary = format!("{} more", at_boundary);
        assert!(input_exceeds_cap(&over_boundary));

        // Short inputs never trip.
        assert!(!input_exceeds_cap("hello world"));
        assert!(!input_exceeds_cap("字".repeat(1200).as_str()));
    }

    /// T11: the output token cap at boundary values: empty input, 1 char,
    /// 480 chars en, 1000 chars zh (the 2048 clamp, the R3 truncation
    /// fix), and a 1200-est-token input.
    #[test]
    fn max_gen_tokens_clamps_at_both_ends() {
        assert_eq!(max_gen_tokens(""), MIN_GEN_TOKENS);
        assert_eq!(max_gen_tokens("a"), MIN_GEN_TOKENS);
        // 480 ASCII chars -> est 120 -> 2*120+128 = 368.
        assert_eq!(max_gen_tokens(&"a".repeat(480)), 368);
        // 1000 CJK chars -> est 1000 -> 2128 -> clamps to 2048.
        assert_eq!(max_gen_tokens(&"字".repeat(1000)), MAX_GEN_TOKENS);
        // 1200-est-token input -> 2528 -> 2048.
        assert_eq!(max_gen_tokens(&"a".repeat(4800)), MAX_GEN_TOKENS);
        // The formula between the clamps, on a mixed input: 100 CJK + 200
        // ASCII -> est 150 -> 428.
        let mixed = format!("{}{}", "字".repeat(100), "a".repeat(200));
        assert_eq!(max_gen_tokens(&mixed), 428);
    }

    /// T13: the forecast multiplier floor. Without a measurement the
    /// forecast is file size * 3/2; a larger measured RSS wins; a smaller
    /// measurement does not shrink below the floor.
    #[test]
    fn forecast_is_max_of_multiplier_floor_and_measured_rss() {
        let file = 610 * 1024 * 1024;
        assert_eq!(
            llm_forecast_bytes(file, None),
            file * FILE_SIZE_FORECAST_MULTIPLIER_NUM / FILE_SIZE_FORECAST_MULTIPLIER_DEN
        );
        assert_eq!(
            llm_forecast_bytes(file, Some(file * 3 / 2 + 128 * 1024 * 1024)),
            file * 3 / 2 + 128 * 1024 * 1024,
            "a larger measured RSS wins"
        );
        assert_eq!(
            llm_forecast_bytes(file, Some(1000)),
            file * 3 / 2,
            "a smaller measurement never shrinks below the floor"
        );
        assert_eq!(llm_forecast_bytes(0, None), 0);
    }
}
