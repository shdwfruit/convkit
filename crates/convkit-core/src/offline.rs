//! Keeping pandoc off the network.
//!
//! pandoc downloads things a document points at, and says so only in a
//! warning. Checked against pandoc 3.10 with a local server logging every
//! request:
//!
//! - Its HTML reader fetches an `<iframe>`'s page to read it in, for every
//!   output format. It does not when `raw_html` is on, so every HTML source
//!   is read as `html+raw_html`. That keeps every tag pandoc has no element
//!   for (`<iframe>`, `<nav>`, `<form>`) as raw HTML, which the Markdown
//!   writer would print, so the filter drops it again for any output but
//!   HTML, as the plain reader would have. What the tags held is kept.
//! - Its docx, odt and rtf writers fetch every picture on the web to put it
//!   in the file, and read any file a picture names, outside the
//!   document's folder too (`../.ssh/id_rsa`). For those (pandoc's
//!   `FORMAT`), the Lua filter here replaces each such picture with a link
//!   to it before the writer sees it, and tells conv how many it replaced.
//!   The plain, gfm and html writers fetch nothing, and keep the address.
//!
//! `<link>`, `<video>`, `<object>`, `<embed>` and `srcset` fetch nothing in
//! any of them.

/// The filter's whole text, written beside the output for pandoc's
/// `--lua-filter`. A picture is on the web when its address has a scheme
/// and `//` (`https://`), or starts with `//`; `file://`, `data:` and a
/// Windows path (`C:\...`) are not. A picture is outside the document's
/// folder when its path, `%XX` decoded as pandoc reads it, is absolute
/// (`/`, `\`, `~`, a drive letter, `file:`) or has a `..` part. pandoc runs
/// the `Pandoc` function after every element, so the counts are complete
/// when they are written.
pub const FILTER: &str = r#"-- Written by conv: pictures on the web become links to them, so pandoc
-- downloads nothing, and so are pictures outside the document's folder.
-- conv never goes online unless asked.
local left_out, outside = 0, 0

local function on_the_web(src)
  if src:sub(1, 2) == '//' then return true end
  local scheme = src:match('^(%a[%w+.-]*)://')
  return scheme ~= nil and scheme:lower() ~= 'file'
end

local function out_of_folder(src)
  -- pandoc reads `%2e%2e` as `..`.
  local path = src:gsub('%%(%x%x)', function(h) return string.char(tonumber(h, 16)) end)
  if path:match('^[/\\~]') or path:match('^%a:') or path:lower():match('^file:') then
    return true
  end
  for part in path:gmatch('[^/\\]+') do
    if part == '..' then return true end
  end
  return false
end

-- Only the writers that put pictures in the file fetch them; Markdown and
-- HTML keep the address, which is what the page had.
local embeds = FORMAT == 'docx' or FORMAT == 'odt' or FORMAT == 'rtf'
  or FORMAT == 'pptx' or FORMAT:match('^epub') ~= nil

function Image(img)
  if not embeds then return nil end
  if on_the_web(img.src) then
    left_out = left_out + 1
  elseif out_of_folder(img.src) then
    outside = outside + 1
  else
    return nil
  end
  local text = img.caption
  if #text == 0 then text = { pandoc.Str(img.src) } end
  return pandoc.Link(text, img.src)
end

-- Raw HTML is kept by the reader only so it fetches no iframe.
local function raw(el)
  if el.format == 'html' and not FORMAT:match('html') then return {} end
end
RawBlock = raw
RawInline = raw

function Pandoc(doc)
  if left_out + outside > 0 then
    io.stderr:write('conv-offline: ' .. left_out .. ' ' .. outside .. '\n')
  end
  return doc
end
"#;

/// The line the filter writes, before its two counts: pictures on the web,
/// then pictures outside the document's folder.
const MARKER: &str = "conv-offline: ";

/// The note for what the filter reported on stderr, if it replaced any
/// picture.
pub fn note(stderr: &str) -> Option<String> {
    let (mut web, mut outside) = (0, 0);
    for counts in stderr.lines().filter_map(|l| l.trim().strip_prefix(MARKER)) {
        let mut n = counts
            .split_whitespace()
            .map(|n| n.parse::<usize>().unwrap_or(0));
        web += n.next().unwrap_or(0);
        outside += n.next().unwrap_or(0);
    }
    let outside = match outside {
        0 => None,
        1 => Some(
            "A picture outside the document's folder was left out and linked to instead: \
             conv puts in only pictures beside the document or below it. Copy it there to \
             keep it."
                .to_string(),
        ),
        n => Some(format!(
            "{n} pictures outside the document's folder were left out and linked to \
             instead: conv puts in only pictures beside the document or below it. Copy \
             them there to keep them."
        )),
    };
    let web = match web {
        0 => None,
        1 => Some(
            "A picture on the web was left out and linked to instead: conv downloads \
             nothing. Save it beside the document and point to it there to keep it."
                .to_string(),
        ),
        n => Some(format!(
            "{n} pictures on the web were left out and linked to instead: conv downloads \
             nothing. Save them beside the document and point to them there to keep them."
        )),
    };
    match (web, outside) {
        (Some(web), Some(outside)) => Some(format!("{web} {outside}")),
        (web, outside) => web.or(outside),
    }
}

/// Whether a stderr line is the filter's own report, which is no warning.
pub fn is_report(line: &str) -> bool {
    line.trim().starts_with(MARKER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_becomes_one_note() {
        assert_eq!(note(""), None);
        assert_eq!(note("[WARNING] something\n"), None);
        assert!(note("conv-offline: 1 0\n")
            .unwrap()
            .starts_with("A picture on the web"));
        assert!(note("x\nconv-offline: 3 0\n")
            .unwrap()
            .starts_with("3 pictures"));
        let both = note("conv-offline: 0 2\nconv-offline: 1 0\n").unwrap();
        assert!(both.starts_with("A picture on the web"), "{both}");
        assert!(both.contains("2 pictures outside"), "{both}");
        assert!(is_report("conv-offline: 2"));
        assert!(!is_report("[WARNING] Could not fetch resource"));
    }
}
