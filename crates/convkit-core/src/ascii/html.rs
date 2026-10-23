//! The art as a web page.

use std::fmt::Write as _;

use super::{Art, Bg};

fn hex(c: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A self-contained page showing `art` on `bg`'s page colour. Text styles
/// are a `<pre>` of the characters, so they can be copied. Half blocks are
/// drawn as fixed-size cells with a two-colour background instead of
/// glyphs, because a font's block glyph rarely fills its line exactly and
/// leaves thin gaps between rows.
pub fn html(art: &Art, bg: Bg, title: &str) -> String {
    let page = hex(bg.page());
    let mut s = format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\"><title>{}</title><style>\nbody{{margin:0;padding:24px;background:{page};color:{}}}\npre{{margin:0;font:12px/1.2 ui-monospace,\"SF Mono\",Menlo,Consolas,\"DejaVu Sans Mono\",monospace}}\n.b div{{display:flex;height:12px}}\n.b i{{flex:none;display:block;width:6px;height:12px}}\n</style></head><body>\n",
        escape(title),
        hex(bg.text())
    );
    let width = art.cols.max(1) as usize;
    // Blocks are drawn as cells only when nothing else is in the art; a
    // ramp mixing them with letters stays text.
    let blocks = art.is_blocks()
        && art
            .cells
            .iter()
            .all(|c| matches!(c.ch, ' ' | '▀' | '▄' | '█'));
    if blocks {
        s.push_str("<div class=\"b\">\n");
        for row in art.cells.chunks(width) {
            s.push_str("<div>");
            for cell in row {
                // Without its own colour a block is in the text colour.
                let ink = Some(cell.fg.unwrap_or(bg.text()));
                let (top, bottom) = match cell.ch {
                    '▀' => (ink, cell.bg),
                    '▄' => (cell.bg, ink),
                    '█' => (ink, ink),
                    _ => (None, None),
                };
                let (top, bottom) = (
                    top.map_or(page.clone(), hex),
                    bottom.map_or(page.clone(), hex),
                );
                if top == bottom {
                    let _ = write!(s, "<i style=\"background:{top}\"></i>");
                } else {
                    let _ = write!(
                        s,
                        "<i style=\"background:linear-gradient({top} 50%,{bottom} 50%)\"></i>"
                    );
                }
            }
            s.push_str("</div>\n");
        }
        s.push_str("</div>\n");
    } else {
        s.push_str("<pre>");
        for row in art.cells.chunks(width) {
            let mut run: Option<[u8; 3]> = None;
            let mut text = String::new();
            let flush = |s: &mut String, run: Option<[u8; 3]>, text: &mut String| {
                if text.is_empty() {
                    return;
                }
                match run {
                    Some(c) => {
                        let _ = write!(
                            s,
                            "<span style=\"color:{}\">{}</span>",
                            hex(c),
                            escape(text)
                        );
                    }
                    None => s.push_str(&escape(text)),
                }
                text.clear();
            };
            let end = row.iter().rposition(|c| c.ch != ' ').map_or(0, |i| i + 1);
            for cell in &row[..end] {
                if cell.fg != run {
                    flush(&mut s, run, &mut text);
                    run = cell.fg;
                }
                text.push(cell.ch);
            }
            flush(&mut s, run, &mut text);
            s.push('\n');
        }
        s.push_str("</pre>\n");
    }
    s.push_str("</body></html>\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ascii::Cell;

    fn art(cells: Vec<Cell>) -> Art {
        Art {
            cols: cells.len() as u32,
            rows: 1,
            cells,
        }
    }

    fn red(ch: char) -> Cell {
        Cell {
            ch,
            fg: Some([255, 0, 0]),
            bg: None,
        }
    }

    fn plain(ch: char) -> Cell {
        Cell {
            ch,
            fg: None,
            bg: None,
        }
    }

    #[test]
    fn plain_half_blocks_show_in_the_text_colour() {
        // A ramp of block characters has no colours of its own: drawn in
        // the page's text colour, as the PNG does, not the page's.
        let page = html(&art(vec![plain('▀'), plain('█')]), Bg::Dark, "t");
        assert!(
            page.contains("linear-gradient(#e4e4e7 50%,#0c0c0e 50%)"),
            "{page}"
        );
        assert!(page.contains("background:#e4e4e7\""), "{page}");
    }

    #[test]
    fn half_blocks_mixed_with_letters_stay_text() {
        let page = html(&art(vec![plain('a'), plain('▀')]), Bg::Dark, "t");
        assert!(page.contains("<pre>a▀"), "{page}");
    }

    #[test]
    fn html_escapes_and_colours() {
        let page = html(&art(vec![red('<')]), Bg::Dark, "a&b");
        assert!(page.contains("&lt;"), "{page}");
        assert!(page.contains("color:#ff0000"), "{page}");
        assert!(page.contains("<title>a&amp;b</title>"), "{page}");
        assert!(page.starts_with("<!doctype html>"));
        assert!(page.contains("<meta charset=\"utf-8\">"));
    }

    #[test]
    fn html_merges_runs() {
        let page = html(&art(vec![red('a'), red('b'), red('c')]), Bg::Dark, "t");
        assert_eq!(page.matches("color:#ff0000").count(), 1, "{page}");
        assert!(page.contains(">abc</span>"), "{page}");
    }

    #[test]
    fn html_blocks_have_no_glyphs() {
        let cells = vec![
            Cell {
                ch: '▀',
                fg: Some([255, 0, 0]),
                bg: Some([0, 0, 255]),
            },
            Cell {
                ch: '▄',
                fg: Some([0, 0, 255]),
                bg: None,
            },
        ];
        let page = html(&art(cells), Bg::Dark, "t");
        assert!(!page.contains('▀') && !page.contains('▄'), "{page}");
        assert!(
            page.contains("linear-gradient(#ff0000 50%,#0000ff 50%)"),
            "{page}"
        );
        assert!(
            page.contains("linear-gradient(#0c0c0e 50%,#0000ff 50%)"),
            "{page}"
        );
    }

    #[test]
    fn html_is_self_contained() {
        let page = html(&art(vec![red('x')]), Bg::Light, "t");
        assert!(!page.contains("http"), "{page}");
        assert!(page.contains("#ffffff"), "the light page: {page}");
    }
    /// A row wider than the window must scroll, not squeeze its cells
    /// narrower than they are tall (the picture would be squished).
    #[test]
    fn html_block_cells_never_shrink() {
        let cell = Cell {
            ch: '▀',
            fg: Some([255, 0, 0]),
            bg: None,
        };
        let page = html(&art(vec![cell; 3]), Bg::Dark, "t");
        assert!(page.contains(".b i{flex:none;"), "{page}");
    }
}
