//! `conv lorem`: placeholder text, by words, sentences or paragraphs.

/// The words of the classic passage, from Cicero's *De finibus*.
const WORDS: &[&str] = &[
    "lorem",
    "ipsum",
    "dolor",
    "sit",
    "amet",
    "consectetur",
    "adipiscing",
    "elit",
    "sed",
    "do",
    "eiusmod",
    "tempor",
    "incididunt",
    "ut",
    "labore",
    "et",
    "dolore",
    "magna",
    "aliqua",
    "enim",
    "ad",
    "minim",
    "veniam",
    "quis",
    "nostrud",
    "exercitation",
    "ullamco",
    "laboris",
    "nisi",
    "aliquip",
    "ex",
    "ea",
    "commodo",
    "consequat",
    "duis",
    "aute",
    "irure",
    "in",
    "reprehenderit",
    "voluptate",
    "velit",
    "esse",
    "cillum",
    "fugiat",
    "nulla",
    "pariatur",
    "excepteur",
    "sint",
    "occaecat",
    "cupidatat",
    "non",
    "proident",
    "sunt",
    "culpa",
    "qui",
    "officia",
    "deserunt",
    "mollit",
    "anim",
    "id",
    "est",
    "laborum",
    "at",
    "vero",
    "eos",
    "accusamus",
    "iusto",
    "odio",
    "dignissimos",
    "ducimus",
    "blanditiis",
    "praesentium",
    "voluptatum",
    "deleniti",
    "atque",
    "corrupti",
    "quos",
    "dolores",
    "quas",
    "molestias",
    "excepturi",
    "occaecati",
    "cupiditate",
    "provident",
    "similique",
    "mollitia",
    "animi",
    "dolorum",
    "fuga",
    "harum",
    "quidem",
    "rerum",
    "facilis",
    "expedita",
    "distinctio",
    "nam",
    "libero",
    "tempore",
    "cum",
    "soluta",
    "nobis",
    "eligendi",
    "optio",
    "cumque",
    "nihil",
    "impedit",
    "quo",
    "minus",
    "quod",
    "maxime",
    "placeat",
    "facere",
    "possimus",
    "omnis",
    "voluptas",
    "assumenda",
    "repellendus",
    "temporibus",
    "autem",
    "quibusdam",
    "officiis",
    "debitis",
    "aut",
    "necessitatibus",
    "saepe",
    "eveniet",
    "voluptates",
    "repudiandae",
    "recusandae",
    "itaque",
    "earum",
    "hic",
    "tenetur",
    "sapiente",
    "delectus",
    "reiciendis",
    "voluptatibus",
    "maiores",
    "alias",
    "perferendis",
    "doloribus",
    "asperiores",
    "repellat",
    "natus",
    "error",
    "accusantium",
    "doloremque",
    "laudantium",
    "totam",
    "rem",
    "aperiam",
    "eaque",
    "ipsa",
    "quae",
    "ab",
    "illo",
    "inventore",
    "veritatis",
    "quasi",
    "architecto",
    "beatae",
    "vitae",
    "dicta",
    "explicabo",
    "nemo",
    "ipsam",
    "quia",
    "aspernatur",
    "odit",
    "fugit",
    "consequuntur",
    "magni",
    "ratione",
    "sequi",
    "nesciunt",
    "neque",
    "porro",
    "quisquam",
    "dolorem",
    "adipisci",
    "numquam",
    "eius",
    "modi",
    "tempora",
    "incidunt",
    "magnam",
    "aliquam",
    "quaerat",
];

/// How every generator's text starts.
const OPENING: &str = "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Amount {
    Words(usize),
    Sentences(usize),
    Paragraphs(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Markup {
    Text,
    Html,
    Markdown,
}

/// A small, fast generator (splitmix64): placeholder text needs variety,
/// not secrecy.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + (self.next() % (hi - lo + 1) as u64) as usize
    }

    fn word(&mut self) -> &'static str {
        WORDS[self.range(0, WORDS.len() - 1)]
    }
}

/// A seed that differs from run to run, from the standard library's own
/// per-process randomness.
pub fn random_seed() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    h.finish()
}

fn sentence(rng: &mut Rng) -> String {
    let n = rng.range(6, 16);
    let mut words: Vec<String> = (0..n).map(|_| rng.word().to_string()).collect();
    // A comma after a word in the first two thirds, in about half of the
    // longer sentences.
    if n >= 9 && rng.next().is_multiple_of(2) {
        let at = rng.range(2, n * 2 / 3);
        words[at].push(',');
    }
    let mut s = words.join(" ");
    capitalize(&mut s);
    s.push('.');
    s
}

fn capitalize(s: &mut String) {
    if let Some(first) = s.chars().next() {
        let upper: String = first.to_uppercase().collect();
        s.replace_range(..first.len_utf8(), &upper);
    }
}

/// The paragraphs of placeholder text; words and sentences come as one
/// paragraph. With `classic`, the text starts "Lorem ipsum dolor sit
/// amet".
pub fn generate(amount: Amount, classic: bool, seed: u64) -> Vec<String> {
    let mut rng = Rng(seed);
    match amount {
        Amount::Words(n) => {
            let mut words: Vec<String> = Vec::with_capacity(n);
            if classic {
                words.extend(
                    OPENING
                        .trim_end_matches('.')
                        .split(' ')
                        .take(n)
                        .map(str::to_string),
                );
            }
            while words.len() < n {
                words.push(rng.word().to_string());
            }
            if n == 0 {
                return vec![];
            }
            let mut text = words.join(" ");
            let text_trimmed = text.trim_end_matches(',').len();
            text.truncate(text_trimmed);
            capitalize(&mut text);
            text.push('.');
            vec![text]
        }
        Amount::Sentences(n) => {
            if n == 0 {
                return vec![];
            }
            let mut sentences = Vec::with_capacity(n);
            if classic {
                sentences.push(OPENING.to_string());
            }
            while sentences.len() < n {
                sentences.push(sentence(&mut rng));
            }
            vec![sentences.join(" ")]
        }
        Amount::Paragraphs(n) => (0..n)
            .map(|i| {
                let count = rng.range(4, 7);
                let mut sentences = Vec::with_capacity(count);
                if classic && i == 0 {
                    sentences.push(OPENING.to_string());
                }
                while sentences.len() < count {
                    sentences.push(sentence(&mut rng));
                }
                sentences.join(" ")
            })
            .collect(),
    }
}

/// The paragraphs as text (a blank line between them), HTML (a `<p>`
/// each) or Markdown (wrapped at 80 columns, as Markdown files are).
pub fn render(paragraphs: &[String], markup: Markup) -> String {
    match markup {
        Markup::Text => paragraphs.join("\n\n"),
        Markup::Html => paragraphs
            .iter()
            .map(|p| format!("<p>{p}</p>"))
            .collect::<Vec<_>>()
            .join("\n"),
        Markup::Markdown => paragraphs
            .iter()
            .map(|p| wrap(p, 80))
            .collect::<Vec<_>>()
            .join("\n\n"),
    }
}

fn wrap(text: &str, width: usize) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / width);
    let mut line_len = 0;
    for word in text.split(' ') {
        if line_len > 0 && line_len + 1 + word.len() > width {
            out.push('\n');
            line_len = 0;
        } else if line_len > 0 {
            out.push(' ');
            line_len += 1;
        }
        out.push_str(word);
        line_len += word.len();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_come_to_the_count_asked_for() {
        let p = generate(Amount::Words(5), true, 1);
        assert_eq!(p, ["Lorem ipsum dolor sit amet."]);
        let p = generate(Amount::Words(40), true, 1);
        assert_eq!(p[0].split(' ').count(), 40);
        assert!(p[0].starts_with("Lorem ipsum dolor sit amet, consectetur"));
        assert!(p[0].ends_with('.'));
        let p = generate(Amount::Words(7), false, 3);
        assert_eq!(p[0].split(' ').count(), 7);
        assert!(!p[0].starts_with("Lorem ipsum"));
        // Seven words of the opening end on a comma, which a full stop
        // replaces.
        assert_eq!(
            generate(Amount::Words(8), true, 1)[0],
            "Lorem ipsum dolor sit amet, consectetur adipiscing elit."
        );
    }

    #[test]
    fn sentences_and_paragraphs() {
        let p = generate(Amount::Sentences(3), true, 9);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].matches(". ").count() + 1, 3);
        let p = generate(Amount::Paragraphs(4), true, 9);
        assert_eq!(p.len(), 4);
        assert!(p[0].starts_with("Lorem ipsum"));
        assert!(!p[1].starts_with("Lorem ipsum"));
        assert!(generate(Amount::Paragraphs(0), true, 9).is_empty());
    }

    #[test]
    fn the_same_seed_gives_the_same_text() {
        assert_eq!(
            generate(Amount::Paragraphs(2), false, 42),
            generate(Amount::Paragraphs(2), false, 42)
        );
        assert_ne!(
            generate(Amount::Paragraphs(2), false, 42),
            generate(Amount::Paragraphs(2), false, 43)
        );
    }

    #[test]
    fn markup() {
        let p = vec!["One two.".to_string(), "Three.".to_string()];
        assert_eq!(render(&p, Markup::Text), "One two.\n\nThree.");
        assert_eq!(render(&p, Markup::Html), "<p>One two.</p>\n<p>Three.</p>");
        let long = generate(Amount::Paragraphs(1), true, 5);
        let md = render(&long, Markup::Markdown);
        assert!(md.lines().all(|l| l.len() <= 80), "{md}");
        assert_eq!(md.replace('\n', " "), long[0]);
    }
}
