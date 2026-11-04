//! `conv chars`: every character in a text, by code point, name and
//! category, with the ones that hide or pretend flagged.
//!
//! Flagged: invisible characters (zero-width spaces, joiners outside the
//! emoji and scripts that use them, tag characters, stray variation
//! selectors), bidi controls (the Trojan Source trick), lookalikes (a
//! Cyrillic `а` in a Latin word, styled and fullwidth letters, the Greek
//! question mark), unusual and no-break spaces, and control characters.

use unicode_properties::{GeneralCategory, UnicodeEmoji, UnicodeGeneralCategory};
use unicode_script::{Script, UnicodeScript};
use unicode_segmentation::UnicodeSegmentation;

/// The kinds of trouble, for the summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Bidi,
    Invisible,
    Lookalike,
    Space,
    Control,
}

impl Kind {
    pub fn key(self) -> &'static str {
        match self {
            Kind::Bidi => "bidi",
            Kind::Invisible => "invisible",
            Kind::Lookalike => "lookalike",
            Kind::Space => "space",
            Kind::Control => "control",
        }
    }

    /// "2 invisible characters".
    pub fn count(self, n: usize) -> String {
        let (one, many) = match self {
            Kind::Bidi => ("bidi control", "bidi controls"),
            Kind::Invisible => ("invisible character", "invisible characters"),
            Kind::Lookalike => ("lookalike", "lookalikes"),
            Kind::Space => ("unusual space", "unusual spaces"),
            Kind::Control => ("control character", "control characters"),
        };
        if n == 1 {
            format!("1 {one}")
        } else {
            format!("{n} {many}")
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flag {
    pub kind: Kind,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Char {
    pub ch: char,
    /// "U+0041".
    pub code_point: String,
    pub name: String,
    /// The two-letter general category: "Lu".
    pub category: &'static str,
    /// What it means: "uppercase letter".
    pub category_name: &'static str,
    pub script: &'static str,
    /// 1-based; the column counts characters (code points).
    pub line: usize,
    pub column: usize,
    pub flag: Option<Flag>,
}

impl Char {
    /// How to show the character in a listing: itself, or a label for
    /// one that wouldn't show (or would rearrange the line, as a bidi
    /// control does).
    pub fn shown(&self) -> String {
        if let Some(l) = label(self.ch) {
            return format!("[{l}]");
        }
        match self.ch.general_category() {
            GeneralCategory::Control => "[ctrl]".to_string(),
            GeneralCategory::Format => "[fmt]".to_string(),
            GeneralCategory::SpaceSeparator => "[space]".to_string(),
            GeneralCategory::NonspacingMark | GeneralCategory::EnclosingMark => {
                format!("\u{25cc}{}", self.ch)
            }
            _ if self
                .flag
                .as_ref()
                .is_some_and(|f| f.kind != Kind::Lookalike) =>
            {
                format!("[{}]", self.code_point)
            }
            _ => self.ch.to_string(),
        }
    }
}

/// What a reader sees as one character, and the code points in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cluster {
    pub text: String,
    pub chars: Vec<Char>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub clusters: Vec<Cluster>,
}

impl Report {
    pub fn chars(&self) -> impl Iterator<Item = &Char> {
        self.clusters.iter().flat_map(|c| c.chars.iter())
    }

    pub fn flagged(&self) -> impl Iterator<Item = &Char> {
        self.chars().filter(|c| c.flag.is_some())
    }

    /// Each kind of trouble found, with how many and where the first is.
    pub fn summary(&self) -> Vec<(Kind, usize, (usize, usize))> {
        let mut out: Vec<(Kind, usize, (usize, usize))> = Vec::new();
        for c in self.flagged() {
            let kind = c.flag.as_ref().expect("flagged").kind;
            match out.iter_mut().find(|(k, _, _)| *k == kind) {
                Some(entry) => entry.1 += 1,
                None => out.push((kind, 1, (c.line, c.column))),
            }
        }
        out.sort_by_key(|(k, _, _)| *k);
        out
    }
}

pub fn inspect(text: &str) -> Report {
    let mixed = mixed_script_letters(text);
    let cjk_lines = lines_with_cjk(text);
    let mut clusters = Vec::new();
    let (mut line, mut column) = (1usize, 0usize);
    for (start, g) in text.grapheme_indices(true) {
        let cps: Vec<(usize, char)> = g.char_indices().map(|(i, c)| (start + i, c)).collect();
        let emoji = cps
            .iter()
            .any(|&(_, c)| !c.is_ascii() && c.is_emoji_char() && c != '\u{200d}');
        let mut chars = Vec::with_capacity(cps.len());
        for (i, &(at, c)) in cps.iter().enumerate() {
            column += 1;
            let ctx = Context {
                prev: (i > 0)
                    .then(|| cps[i - 1].1)
                    .or_else(|| text[..at].chars().last()),
                next: cps
                    .get(i + 1)
                    .map(|p| p.1)
                    .or_else(|| text[at + c.len_utf8()..].chars().next()),
                base: cps[0].1,
                emoji,
                first_in_text: at == 0,
                cjk_line: cjk_lines.contains(&line),
            };
            let flag = mixed
                .iter()
                .find(|(pos, _)| *pos == at)
                .map(|(_, note)| Flag {
                    kind: Kind::Lookalike,
                    note: note.clone(),
                })
                .or_else(|| flag_for(c, &ctx));
            let (category, category_name) = category(c);
            chars.push(Char {
                ch: c,
                code_point: format!("U+{:04X}", c as u32),
                name: name(c),
                category,
                category_name,
                script: c.script().full_name(),
                line,
                column,
                flag,
            });
            if c == '\n' {
                line += 1;
                column = 0;
            }
        }
        clusters.push(Cluster {
            text: g.to_string(),
            chars,
        });
    }
    Report { clusters }
}

struct Context {
    prev: Option<char>,
    next: Option<char>,
    /// The first code point of the cluster this one is in.
    base: char,
    /// Whether the cluster is an emoji.
    emoji: bool,
    first_in_text: bool,
    cjk_line: bool,
}

fn flag_for(c: char, ctx: &Context) -> Option<Flag> {
    let flag = |kind: Kind, note: &str| {
        Some(Flag {
            kind,
            note: note.to_string(),
        })
    };
    let cp = c as u32;
    match c {
        '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}' | '\u{061c}' => {
            flag(
                Kind::Bidi,
                "bidi control: changes the order the text around it is shown in, so it can read differently than it is",
            )
        }
        '\u{feff}' if ctx.first_in_text => flag(
            Kind::Invisible,
            "byte order mark at the start; some programs show it as junk",
        ),
        '\u{feff}' => flag(
            Kind::Invisible,
            "zero-width no-break space: a byte order mark in the middle of the text",
        ),
        '\u{e0000}'..='\u{e007f}' if ctx.base == '\u{1f3f4}' => None,
        '\u{e0000}'..='\u{e007f}' => flag(
            Kind::Invisible,
            "tag character: invisible, and can carry hidden text",
        ),
        '\u{fe00}'..='\u{fe0f}' if ctx.emoji || !ctx.base.is_alphabetic() => None,
        '\u{fe00}'..='\u{fe0f}' => flag(
            Kind::Invisible,
            "variation selector after a letter: invisible, and does nothing here",
        ),
        '\u{e0100}'..='\u{e01ef}' if ctx.base.script() == Script::Han => None,
        '\u{e0100}'..='\u{e01ef}' => flag(
            Kind::Invisible,
            "variation selector: invisible here, and can carry hidden data",
        ),
        '\u{200d}' if ctx.emoji || joins_letters(ctx) => None,
        '\u{200d}' => flag(Kind::Invisible, "zero-width joiner, outside an emoji"),
        '\u{200c}' if joins_letters(ctx) => None,
        '\u{200c}' => flag(Kind::Invisible, "zero-width non-joiner"),
        '\u{200b}' => flag(Kind::Invisible, "zero-width space"),
        '\u{2060}' => flag(Kind::Invisible, "word joiner: invisible"),
        '\u{2061}'..='\u{2064}' => flag(Kind::Invisible, "invisible math operator"),
        '\u{180e}' => flag(Kind::Invisible, "Mongolian vowel separator: invisible"),
        '\u{034f}' => flag(Kind::Invisible, "combining grapheme joiner: invisible"),
        '\u{00ad}' => flag(
            Kind::Invisible,
            "soft hyphen: invisible unless a line breaks there",
        ),
        '\u{115f}' | '\u{1160}' | '\u{3164}' | '\u{ffa0}' => flag(
            Kind::Invisible,
            "Hangul filler: shows as nothing, a favourite for names that look empty",
        ),
        '\u{2800}' => flag(Kind::Space, "braille blank: looks like a space but isn't one"),
        '\u{00a0}' => flag(Kind::Space, "no-break space"),
        '\u{202f}' => flag(Kind::Space, "narrow no-break space"),
        '\u{2007}' => flag(Kind::Space, "figure space: a no-break space as wide as a digit"),
        '\u{2000}'..='\u{200a}' | '\u{205f}' | '\u{1680}' => {
            flag(Kind::Space, "unusual space, wider or narrower than a plain one")
        }
        '\u{2028}' | '\u{2029}' => flag(
            Kind::Space,
            "line separator: a line break in some programs and nothing in others",
        ),
        '\u{2011}' => flag(Kind::Lookalike, "non-breaking hyphen: looks like -"),
        '\u{037e}' => flag(Kind::Lookalike, "Greek question mark: looks exactly like ;"),
        '\u{01c3}' => flag(Kind::Lookalike, "Latin letter retroflex click: looks like !"),
        '\u{2215}' | '\u{2044}' => flag(Kind::Lookalike, "a slash that isn't /"),
        '\u{2024}' => flag(Kind::Lookalike, "one dot leader: looks like ."),
        '\u{a789}' | '\u{0589}' => flag(Kind::Lookalike, "looks like : but isn't"),
        '\u{ff01}'..='\u{ff5e}' if !ctx.cjk_line => {
            let plain = char::from_u32(cp - 0xfee0).unwrap_or('?');
            Some(Flag {
                kind: Kind::Lookalike,
                note: format!("fullwidth {plain}: looks like {plain} but is another character"),
            })
        }
        '\u{1d400}'..='\u{1d7ff}' => {
            let looks = styled_letter(c)
                .map(|p| format!(" like {p}"))
                .unwrap_or_default();
            Some(Flag {
                kind: Kind::Lookalike,
                note: format!(
                    "styled letter: looks{looks}, but search, screen readers and filters see another character"
                ),
            })
        }
        '\t' | '\n' | '\r' => None,
        _ => match c.general_category() {
            GeneralCategory::Control => flag(Kind::Control, "control character"),
            GeneralCategory::Format => flag(Kind::Invisible, "invisible formatting character"),
            _ => None,
        },
    }
}

/// Whether a joiner sits between letters of a script that uses it: ZWNJ
/// in Persian, ZWJ and ZWNJ in the scripts of India.
fn joins_letters(ctx: &Context) -> bool {
    let uses = |c: Option<char>| {
        c.is_some_and(|c| {
            (c.is_alphabetic()
                || c.general_category() == GeneralCategory::SpacingMark
                || c.general_category() == GeneralCategory::NonspacingMark)
                && !matches!(
                    c.script(),
                    Script::Latin | Script::Common | Script::Inherited
                )
        })
    };
    uses(ctx.prev) && uses(ctx.next)
}

/// The plain letter a mathematical styled one copies, from its name:
/// MATHEMATICAL BOLD SMALL A is `a`.
fn styled_letter(c: char) -> Option<char> {
    let name = name(c);
    let (case, letter) = name.rsplit_once(' ').map(|(head, l)| {
        let case = if head.ends_with("SMALL") {
            Some(false)
        } else if head.ends_with("CAPITAL") {
            Some(true)
        } else {
            None
        };
        (case, l)
    })?;
    let mut chars = letter.chars();
    let first = chars.next()?;
    if chars.next().is_some() || !first.is_ascii_alphabetic() {
        return None;
    }
    match case? {
        true => Some(first),
        false => Some(first.to_ascii_lowercase()),
    }
}

/// Scripts whose letters pass for each other.
const LOOKALIKE_SCRIPTS: [Script; 5] = [
    Script::Latin,
    Script::Cyrillic,
    Script::Greek,
    Script::Armenian,
    Script::Cherokee,
];

/// Letters that pass for Latin ones, and the Latin letter each looks like.
const LOOKS_LIKE: &[(char, char)] = &[
    // Cyrillic.
    ('а', 'a'),
    ('е', 'e'),
    ('ё', 'ë'),
    ('о', 'o'),
    ('р', 'p'),
    ('с', 'c'),
    ('у', 'y'),
    ('х', 'x'),
    ('ѕ', 's'),
    ('і', 'i'),
    ('ї', 'ï'),
    ('ј', 'j'),
    ('ԁ', 'd'),
    ('ԛ', 'q'),
    ('ԝ', 'w'),
    ('һ', 'h'),
    ('ӏ', 'l'),
    ('А', 'A'),
    ('В', 'B'),
    ('Е', 'E'),
    ('К', 'K'),
    ('М', 'M'),
    ('Н', 'H'),
    ('О', 'O'),
    ('Р', 'P'),
    ('С', 'C'),
    ('Т', 'T'),
    ('Х', 'X'),
    ('У', 'Y'),
    ('Ѕ', 'S'),
    ('І', 'I'),
    ('Ј', 'J'),
    ('Ԛ', 'Q'),
    ('Ԝ', 'W'),
    ('Ү', 'Y'),
    // Greek.
    ('α', 'a'),
    ('ο', 'o'),
    ('ν', 'v'),
    ('ρ', 'p'),
    ('ι', 'i'),
    ('κ', 'k'),
    ('υ', 'u'),
    ('χ', 'x'),
    ('ϲ', 'c'),
    ('Α', 'A'),
    ('Β', 'B'),
    ('Ε', 'E'),
    ('Ζ', 'Z'),
    ('Η', 'H'),
    ('Ι', 'I'),
    ('Κ', 'K'),
    ('Μ', 'M'),
    ('Ν', 'N'),
    ('Ο', 'O'),
    ('Ρ', 'P'),
    ('Τ', 'T'),
    ('Υ', 'Y'),
    ('Χ', 'X'),
    // Armenian.
    ('օ', 'o'),
    ('ս', 'u'),
    ('հ', 'h'),
    ('ո', 'n'),
];

/// Letters in a word whose other letters are mostly another script, by
/// byte offset, with what to say about each: "Cyrillic а in a Latin word,
/// looks like a".
fn mixed_script_letters(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (start, word) in text.split_word_bound_indices() {
        let letters: Vec<(usize, char, Script)> = word
            .char_indices()
            .filter(|(_, c)| c.is_alphabetic())
            .map(|(i, c)| (start + i, c, c.script()))
            .filter(|(_, _, s)| LOOKALIKE_SCRIPTS.contains(s))
            .collect();
        let mut counts: Vec<(Script, usize)> = Vec::new();
        for (_, _, s) in &letters {
            match counts.iter_mut().find(|(k, _)| k == s) {
                Some(e) => e.1 += 1,
                None => counts.push((*s, 1)),
            }
        }
        if counts.len() < 2 {
            continue;
        }
        // The word's own script: the most letters, Latin on a tie.
        let main = counts
            .iter()
            .max_by_key(|(s, n)| (*n, *s == Script::Latin))
            .map(|(s, _)| *s)
            .expect("two scripts");
        for (at, c, s) in letters {
            if s == main {
                continue;
            }
            let looks = if main == Script::Latin {
                LOOKS_LIKE
                    .iter()
                    .find(|(k, _)| *k == c)
                    .map(|(_, l)| format!(", looks like Latin {l}"))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            out.push((
                at,
                format!(
                    "{} {c} in a {} word{looks}",
                    s.full_name(),
                    main.full_name()
                ),
            ));
        }
    }
    out
}

/// Lines (1-based) with Chinese, Japanese or Korean in them, where
/// fullwidth letters and digits are ordinary.
fn lines_with_cjk(text: &str) -> Vec<usize> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| {
            l.chars().any(|c| {
                matches!(
                    c.script(),
                    Script::Han | Script::Hiragana | Script::Katakana | Script::Hangul
                )
            })
        })
        .map(|(i, _)| i + 1)
        .collect()
}

/// The character's name, or for one Unicode names only by alias (the
/// control characters), that alias.
pub fn name(c: char) -> String {
    if let Some(n) = unicode_names2::name(c) {
        return n.to_string();
    }
    let alias = match c {
        '\0' => "NULL",
        '\u{7}' => "BELL",
        '\u{8}' => "BACKSPACE",
        '\t' => "CHARACTER TABULATION",
        '\n' => "LINE FEED",
        '\u{b}' => "LINE TABULATION",
        '\u{c}' => "FORM FEED",
        '\r' => "CARRIAGE RETURN",
        '\u{1b}' => "ESCAPE",
        '\u{7f}' => "DELETE",
        '\u{85}' => "NEXT LINE",
        _ => {
            return match c.general_category() {
                GeneralCategory::Control => "<control>".to_string(),
                GeneralCategory::PrivateUse => "<private use>".to_string(),
                GeneralCategory::Unassigned => "<unassigned>".to_string(),
                _ => "<no name>".to_string(),
            }
        }
    };
    alias.to_string()
}

/// A short label for a character that wouldn't show.
fn label(c: char) -> Option<&'static str> {
    Some(match c {
        ' ' => "SP",
        '\t' => "TAB",
        '\n' => "LF",
        '\r' => "CR",
        '\0' => "NUL",
        '\u{1b}' => "ESC",
        '\u{7f}' => "DEL",
        '\u{a0}' => "NBSP",
        '\u{202f}' => "NNBSP",
        '\u{2007}' => "FIGSP",
        '\u{ad}' => "SHY",
        '\u{200b}' => "ZWSP",
        '\u{200c}' => "ZWNJ",
        '\u{200d}' => "ZWJ",
        '\u{2060}' => "WJ",
        '\u{feff}' => "BOM",
        '\u{200e}' => "LRM",
        '\u{200f}' => "RLM",
        '\u{61c}' => "ALM",
        '\u{202a}' => "LRE",
        '\u{202b}' => "RLE",
        '\u{202c}' => "PDF",
        '\u{202d}' => "LRO",
        '\u{202e}' => "RLO",
        '\u{2066}' => "LRI",
        '\u{2067}' => "RLI",
        '\u{2068}' => "FSI",
        '\u{2069}' => "PDI",
        '\u{2028}' => "LS",
        '\u{2029}' => "PS",
        '\u{fe0e}' => "VS15",
        '\u{fe0f}' => "VS16",
        '\u{fe00}'..='\u{fe0d}' => "VS",
        '\u{e0100}'..='\u{e01ef}' => "VS",
        '\u{e0000}'..='\u{e007f}' => "TAG",
        '\u{115f}' | '\u{1160}' | '\u{3164}' | '\u{ffa0}' => "FILLER",
        '\u{2800}' => "BRAILLE BLANK",
        _ => return None,
    })
}

fn category(c: char) -> (&'static str, &'static str) {
    use GeneralCategory as G;
    match c.general_category() {
        G::UppercaseLetter => ("Lu", "uppercase letter"),
        G::LowercaseLetter => ("Ll", "lowercase letter"),
        G::TitlecaseLetter => ("Lt", "titlecase letter"),
        G::ModifierLetter => ("Lm", "modifier letter"),
        G::OtherLetter => ("Lo", "letter"),
        G::NonspacingMark => ("Mn", "combining mark"),
        G::SpacingMark => ("Mc", "spacing mark"),
        G::EnclosingMark => ("Me", "enclosing mark"),
        G::DecimalNumber => ("Nd", "digit"),
        G::LetterNumber => ("Nl", "letter number"),
        G::OtherNumber => ("No", "number"),
        G::ConnectorPunctuation => ("Pc", "connector punctuation"),
        G::DashPunctuation => ("Pd", "dash"),
        G::OpenPunctuation => ("Ps", "opening punctuation"),
        G::ClosePunctuation => ("Pe", "closing punctuation"),
        G::InitialPunctuation => ("Pi", "opening quote"),
        G::FinalPunctuation => ("Pf", "closing quote"),
        G::OtherPunctuation => ("Po", "punctuation"),
        G::MathSymbol => ("Sm", "math symbol"),
        G::CurrencySymbol => ("Sc", "currency symbol"),
        G::ModifierSymbol => ("Sk", "modifier symbol"),
        G::OtherSymbol => ("So", "symbol"),
        G::SpaceSeparator => ("Zs", "space"),
        G::LineSeparator => ("Zl", "line separator"),
        G::ParagraphSeparator => ("Zp", "paragraph separator"),
        G::Control => ("Cc", "control"),
        G::Format => ("Cf", "format"),
        G::Surrogate => ("Cs", "surrogate"),
        G::PrivateUse => ("Co", "private use"),
        _ => ("Cn", "unassigned"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(text: &str) -> Vec<(char, Kind)> {
        inspect(text)
            .flagged()
            .map(|c| (c.ch, c.flag.as_ref().unwrap().kind))
            .collect()
    }

    #[test]
    fn plain_text_is_clean() {
        assert!(flags("Hello, world!\n\tIndented line.\r\n").is_empty());
        assert!(flags("Привет, мир. Γειά σου κόσμε. 你好，世界！").is_empty());
    }

    #[test]
    fn names_categories_and_positions() {
        let r = inspect("A\né");
        let chars: Vec<&Char> = r.chars().collect();
        assert_eq!(chars[0].name, "LATIN CAPITAL LETTER A");
        assert_eq!(chars[0].category, "Lu");
        assert_eq!(chars[0].script, "Latin");
        assert_eq!(chars[1].name, "LINE FEED");
        assert_eq!(chars[1].shown(), "[LF]");
        assert_eq!((chars[2].line, chars[2].column), (2, 1));
        assert_eq!(chars[2].code_point, "U+00E9");
    }

    #[test]
    fn a_cyrillic_letter_in_a_latin_word_is_a_lookalike() {
        let r = inspect("pаypal login");
        let flagged: Vec<&Char> = r.flagged().collect();
        assert_eq!(flagged.len(), 1);
        let c = flagged[0];
        assert_eq!(c.ch, 'а');
        assert_eq!(c.column, 2);
        let note = &c.flag.as_ref().unwrap().note;
        assert_eq!(note, "Cyrillic а in a Latin word, looks like Latin a");
    }

    #[test]
    fn a_latin_letter_in_a_cyrillic_word_is_flagged_too() {
        // "Москва" with a Latin o.
        let f = flags("Мoсква");
        assert_eq!(f, [('o', Kind::Lookalike)]);
    }

    #[test]
    fn zero_width_characters() {
        let f = flags("pass\u{200b}word");
        assert_eq!(f, [('\u{200b}', Kind::Invisible)]);
        assert_eq!(flags("a\u{200d}b"), [('\u{200d}', Kind::Invisible)]);
        assert_eq!(flags("\u{feff}hi"), [('\u{feff}', Kind::Invisible)]);
        assert_eq!(flags("soft\u{ad}ware"), [('\u{ad}', Kind::Invisible)]);
    }

    #[test]
    fn emoji_sequences_are_not_suspicious() {
        // ZWJ family, skin tone, flag, England's tag flag, keycap, heart
        // with VS16.
        let text = "👨‍👩‍👧 👍🏽 🇯🇵 🏴\u{e0067}\u{e0062}\u{e0065}\u{e006e}\u{e0067}\u{e007f} 1\u{fe0f}\u{20e3} ❤\u{fe0f}";
        assert!(flags(text).is_empty(), "{:?}", flags(text));
        let r = inspect("👨‍👩‍👧");
        assert_eq!(r.clusters.len(), 1);
        assert_eq!(r.clusters[0].chars.len(), 5);
    }

    #[test]
    fn joiners_in_scripts_that_use_them_are_fine() {
        // Persian "mi-xaham" with a ZWNJ, and Devanagari with a ZWJ.
        assert!(flags("می\u{200c}خواهم").is_empty());
        assert!(flags("क्\u{200d}ष").is_empty());
        assert_eq!(flags("a\u{200c}b"), [('\u{200c}', Kind::Invisible)]);
    }

    #[test]
    fn bidi_controls_are_flagged_and_never_shown_raw() {
        let r = inspect("access\u{202e}lvl\u{2066}");
        let f: Vec<&Char> = r.flagged().collect();
        assert_eq!(f.len(), 2);
        assert!(f
            .iter()
            .all(|c| c.flag.as_ref().unwrap().kind == Kind::Bidi));
        assert_eq!(f[0].shown(), "[RLO]");
        assert_eq!(f[1].shown(), "[LRI]");
    }

    #[test]
    fn hidden_tag_text_is_flagged() {
        // "hi" in tag characters after a plain letter: ASCII smuggling.
        let f = flags("ok\u{e0068}\u{e0069}");
        assert_eq!(f.len(), 2);
        assert!(f.iter().all(|(_, k)| *k == Kind::Invisible));
    }

    #[test]
    fn spaces_and_lookalike_punctuation() {
        assert_eq!(flags("10\u{a0}km"), [('\u{a0}', Kind::Space)]);
        assert_eq!(flags("a\u{2009}b"), [('\u{2009}', Kind::Space)]);
        assert_eq!(flags("x = 1\u{37e}"), [('\u{37e}', Kind::Lookalike)]);
        assert_eq!(flags("bell\u{7}"), [('\u{7}', Kind::Control)]);
    }

    #[test]
    fn styled_and_fullwidth_letters() {
        let r = inspect("𝐟𝐫𝐞𝐞 ＡＢ");
        let notes: Vec<String> = r
            .flagged()
            .map(|c| c.flag.as_ref().unwrap().note.clone())
            .collect();
        assert_eq!(notes.len(), 6);
        assert!(notes[0].contains("looks like f"), "{}", notes[0]);
        assert!(notes[4].contains("fullwidth A"), "{}", notes[4]);
        // Fullwidth forms in Japanese text are ordinary.
        assert!(flags("価格は１００円").is_empty());
    }

    #[test]
    fn combining_marks_show_on_a_circle() {
        let r = inspect("e\u{301}");
        assert_eq!(r.clusters.len(), 1);
        assert_eq!(r.clusters[0].chars[1].shown(), "\u{25cc}\u{301}");
        assert_eq!(r.clusters[0].chars[1].category, "Mn");
    }

    #[test]
    fn the_summary_counts_each_kind() {
        let r = inspect("a\u{200b}b\u{200b} pаy\u{202e}");
        let s = r.summary();
        assert_eq!(
            s,
            [
                (Kind::Bidi, 1, (1, 9)),
                (Kind::Invisible, 2, (1, 2)),
                (Kind::Lookalike, 1, (1, 7))
            ]
        );
        assert_eq!(Kind::Invisible.count(2), "2 invisible characters");
    }
}
