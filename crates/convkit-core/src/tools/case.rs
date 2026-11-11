//! `conv case`: UPPER, lower, Title, Sentence, and the cases code uses.
//!
//! Every letter changes by Unicode's own case rules, so `ß` becomes `SS`
//! and a final Greek sigma comes out as `ς`. The code cases (camel,
//! pascal, snake, kebab, constant) work a line at a time, so a list of
//! names stays a list.

use heck::{ToKebabCase, ToLowerCamelCase, ToShoutySnakeCase, ToSnakeCase, ToUpperCamelCase};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Upper,
    Lower,
    Title,
    Sentence,
    Camel,
    Pascal,
    Snake,
    Kebab,
    Constant,
}

impl Style {
    pub const NAMES: [&'static str; 9] = [
        "upper", "lower", "title", "sentence", "camel", "pascal", "snake", "kebab", "constant",
    ];

    pub fn parse(s: &str) -> Option<Style> {
        Some(
            match s.to_ascii_lowercase().replace(['-', '_'], "").as_str() {
                "upper" | "uppercase" => Style::Upper,
                "lower" | "lowercase" => Style::Lower,
                "title" | "titlecase" => Style::Title,
                "sentence" | "sentencecase" => Style::Sentence,
                "camel" | "camelcase" | "lowercamel" => Style::Camel,
                "pascal" | "pascalcase" | "uppercamel" => Style::Pascal,
                "snake" | "snakecase" => Style::Snake,
                "kebab" | "kebabcase" | "dash" => Style::Kebab,
                "constant" | "constantcase" | "screamingsnake" | "shoutysnake" => Style::Constant,
                _ => return None,
            },
        )
    }

    pub fn name(self) -> &'static str {
        Style::NAMES[self as usize]
    }
}

pub fn convert(text: &str, style: Style) -> String {
    match style {
        Style::Upper => text.to_uppercase(),
        Style::Lower => text.to_lowercase(),
        Style::Title => title(text),
        Style::Sentence => sentence(text),
        code => text
            .split_inclusive('\n')
            .map(|line| {
                let (body, end) = split_line_end(line);
                let body = drop_apostrophes(body);
                let converted = match code {
                    Style::Camel => body.to_lower_camel_case(),
                    Style::Pascal => body.to_upper_camel_case(),
                    Style::Snake => body.to_snake_case(),
                    Style::Kebab => body.to_kebab_case(),
                    _ => body.to_shouty_snake_case(),
                };
                format!("{converted}{end}")
            })
            .collect(),
    }
}

fn split_line_end(line: &str) -> (&str, &str) {
    let body = line.trim_end_matches(['\n', '\r']);
    (body, &line[body.len()..])
}

/// "don't" is one word in code, `dont`, not `don_t`.
fn drop_apostrophes(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for (i, &c) in chars.iter().enumerate() {
        let between_letters = i > 0
            && chars[i - 1].is_alphanumeric()
            && chars.get(i + 1).is_some_and(|n| n.is_alphanumeric());
        if matches!(c, '\'' | '\u{2019}') && between_letters {
            continue;
        }
        out.push(c);
    }
    out
}

/// English words a title leaves lowercase unless they start or end it.
const SMALL_WORDS: &[&str] = &[
    "a", "an", "and", "as", "at", "but", "by", "for", "if", "in", "nor", "of", "on", "or", "per",
    "the", "to", "vs", "via",
];

/// Whether a word's capitals are its own: an acronym (NASA, the C of
/// USB-C) or a name like iPhone, which a change of case would spoil.
fn keeps_case(word: &str) -> bool {
    let inner = word.chars().skip(1).any(char::is_uppercase);
    let all_caps = word.chars().any(char::is_uppercase) && !word.chars().any(char::is_lowercase);
    inner || all_caps
}

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// A line or sentence in capitals throughout carries no case worth
/// keeping: it is lowercased first.
fn shouting(text: &str) -> bool {
    !text.chars().any(char::is_lowercase)
}

/// Title Case: each word capitalized, except small words inside a line
/// ("of", "the"); words like iPhone and NASA keep their capitals.
fn title(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let line = if shouting(line) {
            line.to_lowercase()
        } else {
            line.to_string()
        };
        let tokens: Vec<&str> = line.split_word_bounds().collect();
        let word_at: Vec<usize> = tokens
            .iter()
            .enumerate()
            .filter(|(_, t)| t.chars().any(char::is_alphabetic))
            .map(|(i, _)| i)
            .collect();
        let first = word_at.first().copied();
        let last = word_at.last().copied();
        for (i, token) in tokens.iter().enumerate() {
            if word_at.binary_search(&i).is_err() || keeps_case(token) {
                out.push_str(token);
                continue;
            }
            let lower = token.to_lowercase();
            let small = SMALL_WORDS.contains(&lower.as_str());
            if small && Some(i) != first && Some(i) != last {
                out.push_str(&lower);
            } else {
                out.push_str(&capitalize(&lower));
            }
        }
    }
    out
}

/// Sentence case: the first letter of each sentence a capital, the rest
/// lowercase, keeping acronyms, names like iPhone, and "I".
fn sentence(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for s in sentences(text) {
        let s = if shouting(s) {
            s.to_lowercase()
        } else {
            s.to_string()
        };
        let mut started = false;
        for token in s.split_word_bounds() {
            let is_word = token.chars().any(char::is_alphabetic);
            if !is_word {
                out.push_str(token);
                continue;
            }
            if keeps_case(token) {
                out.push_str(token);
            } else {
                let lower = token.to_lowercase();
                if !started || is_english_i(&lower) {
                    out.push_str(&capitalize(&lower));
                } else {
                    out.push_str(&lower);
                }
            }
            started = true;
        }
    }
    out
}

/// Sentences for sentence case: up to `.`, `!` or `?` (or their CJK
/// forms) and the spaces after them, or to a line break. Unicode's own
/// sentence rules don't end a sentence before a lowercase word, so as not
/// to split "e.g. this", and lowercase text is what sentence case is for.
fn sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut ended = false;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '\n' {
            out.push(&text[start..=i]);
            start = i + 1;
            ended = false;
        } else if matches!(
            c,
            '.' | '!' | '?' | '\u{3002}' | '\u{ff01}' | '\u{ff1f}' | '\u{2026}'
        ) {
            ended = true;
        } else if ended
            && matches!(
                c,
                '"' | '\'' | ')' | ']' | '\u{201d}' | '\u{2019}' | '\u{bb}'
            )
        {
            // A closing quote or bracket belongs to the sentence it ends.
        } else if ended && c.is_whitespace() {
            let mut end = i + c.len_utf8();
            while let Some(&(j, d)) = chars.peek() {
                if d.is_whitespace() && d != '\n' {
                    end = j + d.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
            out.push(&text[start..end]);
            start = end;
            ended = false;
        } else {
            ended = false;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// The English pronoun, alone or in a contraction (I'm, I've, I'd, I'll).
fn is_english_i(word: &str) -> bool {
    word == "i" || ["i'", "i\u{2019}"].iter().any(|p| word.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_case_of_one_huge_line_is_linear() {
        let text = "word ".repeat(200_000);
        assert!(convert(&text, Style::Title).starts_with("Word Word"));
    }

    #[test]
    fn upper_and_lower_follow_unicode() {
        assert_eq!(convert("straße", Style::Upper), "STRASSE");
        // A final sigma.
        assert_eq!(convert("ΟΔΟΣ", Style::Lower), "οδο\u{3c2}");
        assert_eq!(convert("\u{130}stanbul", Style::Lower), "i\u{307}stanbul");
        assert_eq!(convert("café 👍", Style::Upper), "CAFÉ 👍");
    }

    #[test]
    fn title_case_keeps_small_words_and_acronyms() {
        assert_eq!(
            convert("the lord of the rings", Style::Title),
            "The Lord of the Rings"
        );
        assert_eq!(
            convert("a guide to NASA and the iPhone", Style::Title),
            "A Guide to NASA and the iPhone"
        );
        assert_eq!(convert("WHAT IS IT FOR", Style::Title), "What Is It For");
        assert_eq!(convert("élan vital", Style::Title), "Élan Vital");
        assert_eq!(convert("one\ntwo of", Style::Title), "One\nTwo Of");
    }

    #[test]
    fn sentence_case() {
        assert_eq!(
            convert("HELLO THERE. HOW ARE YOU? i am fine.", Style::Sentence),
            "Hello there. How are you? I am fine."
        );
        assert_eq!(
            convert("my iPhone uses USB-C. it's good", Style::Sentence),
            "My iPhone uses USB-C. It's good"
        );
        assert_eq!(convert("i'm here", Style::Sentence), "I'm here");
        assert_eq!(
            convert("pi is 3.14. \"really?\" yes!\nnext line", Style::Sentence),
            "Pi is 3.14. \"Really?\" Yes!\nNext line"
        );
    }

    #[test]
    fn code_cases() {
        let s = "Hello world, it's XMLHttpRequest 2";
        assert_eq!(convert(s, Style::Camel), "helloWorldItsXmlHttpRequest2");
        assert_eq!(convert(s, Style::Pascal), "HelloWorldItsXmlHttpRequest2");
        assert_eq!(
            convert(s, Style::Snake),
            "hello_world_its_xml_http_request_2"
        );
        assert_eq!(
            convert(s, Style::Kebab),
            "hello-world-its-xml-http-request-2"
        );
        assert_eq!(
            convert(s, Style::Constant),
            "HELLO_WORLD_ITS_XML_HTTP_REQUEST_2"
        );
    }

    #[test]
    fn code_cases_are_unicode_aware_and_per_line() {
        assert_eq!(convert("Ünïcödé Straße", Style::Snake), "ünïcödé_straße");
        assert_eq!(
            convert("Ünïcödé Straße", Style::Constant),
            "ÜNÏCÖDÉ_STRASSE"
        );
        assert_eq!(convert("你好 世界", Style::Kebab), "你好-世界");
        assert_eq!(
            convert("first name\r\nlast name\n", Style::Snake),
            "first_name\r\nlast_name\n"
        );
    }

    #[test]
    fn style_names_read_loosely() {
        assert_eq!(Style::parse("SNAKE_CASE"), Some(Style::Snake));
        assert_eq!(Style::parse("kebab-case"), Some(Style::Kebab));
        assert_eq!(Style::parse("camelCase"), Some(Style::Camel));
        assert_eq!(Style::parse("wavy"), None);
        assert_eq!(Style::Constant.name(), "constant");
    }
}
