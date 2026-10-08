//! Page ranges for `conv split`: what the user typed, `z` resolved against
//! a real page count, and which pages a set of ranges leaves out or
//! repeats.

use crate::error::{ConvError, ErrorCode, Result};

/// One end of a range: a page number (1-based), or `z`, the last page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageRef {
    Num(u32),
    Last,
}

/// A range as the user typed it: `5`, `1-3`, `11-z`, `z`, `5-1`. `text` is
/// kept for messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRange {
    pub start: PageRef,
    pub end: PageRef,
    pub text: String,
}

/// A range with `z` resolved against a real page count. `start > end` is a
/// reversed range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolved {
    pub start: u32,
    pub end: u32,
}

fn malformed(text: &str) -> ConvError {
    ConvError::new(
        ErrorCode::InvalidInvocation,
        format!(
            "`{text}` is not a page range; use a page (5), a span (1-3), \
             or z for the last page (11-z)"
        ),
    )
}

fn parse_ref(token: &str) -> Option<PageRef> {
    if token.eq_ignore_ascii_case("z") {
        return Some(PageRef::Last);
    }
    if token.is_empty() || !token.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    token
        .parse::<u32>()
        .ok()
        .filter(|&n| n >= 1)
        .map(PageRef::Num)
}

/// Parses `5`, `1-3`, `11-z`, `z` or `5-1`. Comma lists and qpdf's own
/// `r`-from-the-end syntax are deliberately not accepted.
pub fn parse_range(text: &str) -> Result<PageRange> {
    let (a, b) = text.split_once('-').unwrap_or((text, text));
    let start = parse_ref(a).ok_or_else(|| malformed(text))?;
    let end = parse_ref(b).ok_or_else(|| malformed(text))?;
    Ok(PageRange {
        start,
        end,
        text: text.to_string(),
    })
}

impl PageRange {
    /// Resolves `z` to `pages` and checks both ends exist. `input` names the
    /// file in the error.
    pub fn resolve(&self, pages: u32, input: &str) -> Result<Resolved> {
        let at = |r: PageRef| match r {
            PageRef::Num(n) => n,
            PageRef::Last => pages,
        };
        let (start, end) = (at(self.start), at(self.end));
        if start > pages || end > pages {
            let noun = if pages == 1 { "page" } else { "pages" };
            return Err(ConvError::new(
                ErrorCode::InvalidInvocation,
                format!(
                    "{input} has {pages} {noun}; {} goes past the end",
                    self.text
                ),
            ));
        }
        Ok(Resolved { start, end })
    }
}

impl Resolved {
    /// The source pages in output order: ascending, or descending for a
    /// reversed range.
    pub fn pages(&self) -> Vec<u32> {
        if self.start <= self.end {
            (self.start..=self.end).collect()
        } else {
            (self.end..=self.start).rev().collect()
        }
    }

    /// How the range appears in a file name and in qpdf's page list.
    pub fn label(&self) -> String {
        if self.start == self.end {
            self.start.to_string()
        } else {
            format!("{}-{}", self.start, self.end)
        }
    }
}

/// Pages no range covers, and pages more than one range covers, each in
/// ascending order. A page counts once per range that contains it.
pub fn coverage(pages: u32, ranges: &[Resolved]) -> (Vec<u32>, Vec<u32>) {
    let mut count = vec![0u32; pages as usize + 1];
    for r in ranges {
        for p in r.start.min(r.end)..=r.start.max(r.end) {
            count[p as usize] += 1;
        }
    }
    let left_out = (1..=pages).filter(|&p| count[p as usize] == 0).collect();
    let repeated = (1..=pages).filter(|&p| count[p as usize] > 1).collect();
    (left_out, repeated)
}

/// Ascending, unique pages as text: `[1, 4, 9, 10, 11, 12]` is
/// `1, 4 and 9-12`.
pub fn spans(pages: &[u32]) -> String {
    let mut parts = Vec::new();
    let mut i = 0;
    while i < pages.len() {
        let start = pages[i];
        let mut end = start;
        while i + 1 < pages.len() && pages[i + 1] == end + 1 {
            i += 1;
            end = pages[i];
        }
        parts.push(if start == end {
            start.to_string()
        } else {
            format!("{start}-{end}")
        });
        i += 1;
    }
    join_and(&parts)
}

/// `a`, `a and b`, `a, b and c`.
pub(crate) fn join_and(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(text: &str) -> PageRange {
        parse_range(text).unwrap_or_else(|e| panic!("{text}: {e}"))
    }

    #[test]
    fn parses_every_documented_form() {
        assert_eq!(r("5").start, PageRef::Num(5));
        assert_eq!(r("5").end, PageRef::Num(5));
        assert_eq!(
            (r("1-3").start, r("1-3").end),
            (PageRef::Num(1), PageRef::Num(3))
        );
        assert_eq!(
            (r("11-z").start, r("11-z").end),
            (PageRef::Num(11), PageRef::Last)
        );
        assert_eq!((r("z").start, r("z").end), (PageRef::Last, PageRef::Last));
        assert_eq!(
            (r("5-1").start, r("5-1").end),
            (PageRef::Num(5), PageRef::Num(1))
        );
        assert_eq!(
            (r("z-1").start, r("z-1").end),
            (PageRef::Last, PageRef::Num(1))
        );
        assert_eq!(r("Z").start, PageRef::Last);
        assert_eq!(r("007").start, PageRef::Num(7));
        assert_eq!(r("1-3").text, "1-3");
    }

    #[test]
    fn refuses_anything_else_and_says_what_works() {
        for bad in [
            "", "0", "0-3", "3-", "-3", "1-3,7", "a", "r1", "1-2-3", "+2", "1 - 3",
        ] {
            let e = parse_range(bad).unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidInvocation, "{bad}");
            assert_eq!(
                e.message,
                format!(
                    "`{bad}` is not a page range; use a page (5), a span (1-3), \
                     or z for the last page (11-z)"
                ),
            );
        }
    }

    #[test]
    fn resolves_z_and_refuses_pages_past_the_end() {
        assert_eq!(
            r("11-z").resolve(12, "report.pdf").unwrap(),
            Resolved { start: 11, end: 12 }
        );
        assert_eq!(
            r("z").resolve(12, "report.pdf").unwrap(),
            Resolved { start: 12, end: 12 }
        );
        let e = r("15-20").resolve(12, "report.pdf").unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInvocation);
        assert_eq!(
            e.message,
            "report.pdf has 12 pages; 15-20 goes past the end"
        );
        let e = r("2").resolve(1, "one.pdf").unwrap_err();
        assert_eq!(e.message, "one.pdf has 1 page; 2 goes past the end");
        let e = r("15-z").resolve(12, "report.pdf").unwrap_err();
        assert_eq!(e.message, "report.pdf has 12 pages; 15-z goes past the end");
    }

    #[test]
    fn a_resolved_range_lists_its_pages_in_output_order() {
        assert_eq!(Resolved { start: 1, end: 3 }.pages(), vec![1, 2, 3]);
        assert_eq!(Resolved { start: 5, end: 1 }.pages(), vec![5, 4, 3, 2, 1]);
        assert_eq!(Resolved { start: 4, end: 4 }.pages(), vec![4]);
        assert_eq!(Resolved { start: 1, end: 3 }.label(), "1-3");
        assert_eq!(Resolved { start: 5, end: 1 }.label(), "5-1");
        assert_eq!(Resolved { start: 4, end: 4 }.label(), "4");
    }

    #[test]
    fn coverage_finds_pages_left_out_and_pages_repeated() {
        let ranges = [
            Resolved { start: 1, end: 5 },
            Resolved { start: 3, end: 8 },
            Resolved { start: 8, end: 9 },
        ];
        assert_eq!(coverage(12, &ranges), (vec![10, 11, 12], vec![3, 4, 5, 8]));
        let reversed = [Resolved { start: 3, end: 1 }];
        assert_eq!(coverage(3, &reversed), (vec![], vec![]));
    }

    #[test]
    fn spans_compress_runs_and_join_with_and() {
        assert_eq!(spans(&[]), "");
        assert_eq!(spans(&[1]), "1");
        assert_eq!(spans(&[11, 12]), "11-12");
        assert_eq!(spans(&[1, 5, 6, 7, 8, 9, 10, 11, 12]), "1 and 5-12");
        assert_eq!(spans(&[1, 4, 9, 10, 11, 12]), "1, 4 and 9-12");
    }

    #[test]
    fn join_and_reads_like_a_sentence() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(join_and(&s(&[])), "");
        assert_eq!(join_and(&s(&["a.pdf"])), "a.pdf");
        assert_eq!(join_and(&s(&["a.pdf", "b.pdf"])), "a.pdf and b.pdf");
        assert_eq!(
            join_and(&s(&["a.pdf", "b.pdf", "c.pdf"])),
            "a.pdf, b.pdf and c.pdf"
        );
    }
}
