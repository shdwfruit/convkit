//! The developer and text tools: `conv base64`, `hash`, `uuid`, `jwt`,
//! `time`, `link`, `count`, `case`, `chars`, `lorem` and `diff`.
//!
//! These are the chores people otherwise do on a website, often by pasting
//! a token or a secret into it. Everything here runs in this process and,
//! like the rest of the core, never prints. Only `link::fetch_location`
//! touches the network, and only `conv link --follow` calls it.

pub mod base64;
pub mod case;
pub mod chars;
pub mod count;
pub mod diff;
pub mod hash;
pub mod jwt;
pub mod link;
pub mod lorem;
pub mod time;
pub mod uuid;

/// `n` with a comma between each group of three digits: 12,345.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// "1 word", "2 words": `n` with the singular or the plural.
pub fn plural(n: u64, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{} {many}", thousands(n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_groups_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1234567), "1,234,567");
    }

    #[test]
    fn plural_picks_the_word() {
        assert_eq!(plural(1, "file", "files"), "1 file");
        assert_eq!(plural(2000, "file", "files"), "2,000 files");
    }
}
