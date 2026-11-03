//! `conv count`: words, characters, sentences, paragraphs and lines, and
//! how long the text takes to read and to say.
//!
//! Words and sentences follow Unicode's own rules (UAX #29), so each CJK
//! character counts as a word, as word processors count them, and a
//! character is what a reader sees as one: an emoji made of several code
//! points, or a letter with its accents, is one character.

use unicode_segmentation::UnicodeSegmentation;

/// Average silent reading speed for adults reading English non-fiction:
/// Brysbaert (2019), a review of 190 studies.
pub const READING_WPM: f64 = 238.0;

/// A steady presenting pace; conversation runs faster.
pub const SPEAKING_WPM: f64 = 140.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Counts {
    pub words: u64,
    /// Characters as a reader sees them (grapheme clusters), without line
    /// breaks.
    pub characters: u64,
    pub characters_no_spaces: u64,
    pub sentences: u64,
    pub paragraphs: u64,
    pub lines: u64,
    pub bytes: u64,
    pub reading_seconds: u64,
    pub speaking_seconds: u64,
}

pub fn count(text: &str) -> Counts {
    let mut characters = 0u64;
    let mut no_spaces = 0u64;
    for g in text.graphemes(true) {
        if g == "\n" || g == "\r\n" || g == "\r" {
            continue;
        }
        characters += 1;
        if !g.chars().all(char::is_whitespace) {
            no_spaces += 1;
        }
    }
    let words = words(text);
    // A plain loop: unicode-segmentation 1.13's sentence iterator
    // underflows in `size_hint` on empty text, which `count` asks for.
    let mut sentences = 0u64;
    for s in text.unicode_sentences() {
        if s.chars().any(char::is_alphanumeric) {
            sentences += 1;
        }
    }
    let mut paragraphs = 0u64;
    let mut in_paragraph = false;
    for line in text.lines() {
        let blank = line.trim().is_empty();
        if !blank && !in_paragraph {
            paragraphs += 1;
        }
        in_paragraph = !blank;
    }
    let lines = text.lines().count() as u64;
    Counts {
        words,
        characters,
        characters_no_spaces: no_spaces,
        sentences,
        paragraphs,
        lines,
        bytes: text.len() as u64,
        reading_seconds: seconds(words, READING_WPM),
        speaking_seconds: seconds(words, SPEAKING_WPM),
    }
}

fn seconds(words: u64, wpm: f64) -> u64 {
    (words as f64 * 60.0 / wpm).round() as u64
}

/// Words by UAX #29, with a hyphen between two words joining them, as
/// people count "e-mail" or "well-known": one word each.
fn words(text: &str) -> u64 {
    let mut count = 0u64;
    // Whether the last token was a word, and whether a hyphen followed it
    // directly, so a word right after it continues it.
    let mut after_word = false;
    let mut hyphen = false;
    for token in text.split_word_bounds() {
        let is_word = token.chars().any(char::is_alphanumeric);
        if is_word {
            if !(after_word && hyphen) {
                count += 1;
            }
            after_word = true;
            hyphen = false;
        } else if after_word && !hyphen && matches!(token, "-" | "\u{2010}" | "\u{2011}") {
            hyphen = true;
        } else {
            after_word = false;
            hyphen = false;
        }
    }
    count
}

/// "4 min 12 s", "38 s", "0 s".
pub fn duration(seconds: u64) -> String {
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    match (h, m) {
        (0, 0) => format!("{s} s"),
        (0, _) => format!("{m} min {s} s"),
        _ => format!("{h} h {m} min"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_english() {
        let c = count("The quick brown fox. It jumped!\n\nA second paragraph here.\n");
        assert_eq!(c.words, 10);
        assert_eq!(c.sentences, 3);
        assert_eq!(c.paragraphs, 2);
        assert_eq!(c.lines, 3);
        assert_eq!(c.characters, 55);
        assert_eq!(c.characters_no_spaces, 47);
    }

    #[test]
    fn hyphens_and_apostrophes_stay_inside_a_word() {
        assert_eq!(count("e-mail isn't well-known").words, 3);
        assert_eq!(count("a - b").words, 2);
        assert_eq!(count("3.14 and 1,000").words, 3);
    }

    #[test]
    fn cjk_counts_a_word_a_character() {
        let c = count("我喜欢猫。你呢？");
        assert_eq!(c.words, 6);
        assert_eq!(c.sentences, 2);
        assert_eq!(c.characters, 8);
        // Japanese: kanji and kana one by one, a katakana run together.
        assert_eq!(count("猫がコーヒーを飲む").words, 6);
    }

    #[test]
    fn an_emoji_sequence_is_one_character() {
        // Family (man, woman, girl, boy) joined by ZWJ, a flag, a skin
        // tone, and e with a combining accent.
        let c = count("👨‍👩‍👧‍👦 🇯🇵 👍🏽 e\u{301}");
        assert_eq!(c.characters, 7);
        assert_eq!(c.characters_no_spaces, 4);
        assert_eq!(c.words, 1);
    }

    #[test]
    fn crlf_lines_and_empty_text() {
        let c = count("one\r\ntwo\r\n\r\nthree");
        assert_eq!(c.lines, 4);
        assert_eq!(c.paragraphs, 2);
        assert_eq!(c.characters, 11);
        let e = count("");
        assert_eq!((e.words, e.lines, e.paragraphs, e.sentences), (0, 0, 0, 0));
    }

    #[test]
    fn reading_and_speaking_time() {
        let text = "word ".repeat(476);
        let c = count(&text);
        assert_eq!(c.reading_seconds, 120);
        assert_eq!(c.speaking_seconds, 204);
        assert_eq!(duration(204), "3 min 24 s");
        assert_eq!(duration(38), "38 s");
        assert_eq!(duration(3725), "1 h 2 min");
    }
}
