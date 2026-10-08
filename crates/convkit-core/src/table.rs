//! What a CSV or a workbook holds that LibreOffice would get wrong or not
//! mention, read before planning.
//!
//! A CSV says nothing about itself: not its encoding, its delimiter, or
//! which of its columns are numbers. LibreOffice guesses, and its guesses
//! lose data: `02134` becomes 2134, a 16-digit card number loses its last
//! digit, `1/2` becomes a date, and `=1+1` is run as a formula. So conv
//! reads the CSV itself and tells LibreOffice exactly how to import it
//! (`import_filter`). The other way, a workbook's extra sheets and its
//! formulas do not survive into a CSV, and the notes say so only when the
//! workbook has them.

use std::io::Read;
use std::path::Path;

use crate::Format;

/// What `read` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableShape {
    Csv(CsvShape),
    Workbook(WorkbookShape),
}

/// How a CSV is written, and which of its columns must stay text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvShape {
    pub delimiter: u8,
    pub encoding: Encoding,
    /// Numbers are written `1,5`: a semicolon-separated file from a
    /// locale whose decimal separator is a comma.
    pub decimal_comma: bool,
    /// Columns imported as text, 1-based, each with its header: leading
    /// zeros (zip codes, IDs), digit strings too long for a number (card
    /// and account numbers), or a leading `+` (phone numbers).
    pub text_columns: Vec<(usize, String)>,
    /// Some cell starts with `=`.
    pub formulas: bool,
}

impl Default for CsvShape {
    fn default() -> Self {
        CsvShape {
            delimiter: b',',
            encoding: Encoding::Utf8,
            decimal_comma: false,
            text_columns: Vec::new(),
            formulas: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    /// With a byte-order mark: Excel's "Unicode Text".
    Utf16,
    /// Not valid UTF-8: what Excel writes for "CSV" on a Western Windows.
    Windows1252,
}

/// A workbook's sheets, in tab order, and whether the first, the one a
/// CSV gets, has formulas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkbookShape {
    pub sheets: Vec<String>,
    pub formulas: bool,
}

/// Reads `path` as `from` describes it. `None` when it cannot be read or
/// is not what its extension says, so the notes stay whole and the import
/// falls back to the plain default.
pub fn read(path: &Path, from: Format) -> Option<TableShape> {
    match from {
        Format::Csv => sniff_csv(&std::fs::read(path).ok()?).map(TableShape::Csv),
        Format::Xlsx => xlsx(path).map(TableShape::Workbook),
        Format::Ods => ods(path).map(TableShape::Workbook),
        _ => None,
    }
}

/// The `--infilter` value for importing a CSV of this shape. The tokens of
/// LibreOffice's CSV filter options, in order:
///
/// 1. Field separator, as a character code: 44 `,`, 59 `;`, 9 tab, 124 `|`.
/// 2. Text delimiter: 34, the double quote.
/// 3. Character set: 76 UTF-8 (a UTF-8 byte-order mark is skipped), 1
///    Windows-1252, 65535 UTF-16 by its byte-order mark. (LibreOffice's
///    help lists 75 for UTF-8; 75 is UTF-7, which garbles the file.)
/// 4. First line to read: 1, so the header row becomes row 1.
/// 5. Column formats, `column/format` pairs: 2 is Text, for the columns in
///    `text_columns`. Every other column is Standard.
/// 6. Language, which sets the decimal separator: 1033 (English, US), or
///    1031 (German) for a file with decimal commas.
/// 7. Quoted field as text: false. Quoting says nothing about type in
///    most writers' output; the column formats decide.
/// 8. Detect special numbers: false, so `1/2`, `3-4` and `Mar 5` are not
///    turned into dates. An ISO 8601 date still is, which is unambiguous.
/// 9. and 10. Export only: save as shown, export formulas.
/// 11. Remove spaces: false. The cells keep what the file holds.
/// 12. Export only: which sheet.
/// 13. Import as formulas: false, so a cell starting with `=` is kept as
///     text and never run. Left out, it is true, for old LibreOffices'
///     sake.
/// 14. Export only: byte-order mark. On import one is detected anyway.
/// 15. Detect scientific notation: false, so an ID like `1E5` is not read
///     as 100000. It can only be false with token 8 false.
pub fn import_filter(shape: &CsvShape) -> String {
    let charset = match shape.encoding {
        Encoding::Utf8 => 76,
        Encoding::Windows1252 => 1,
        Encoding::Utf16 => 65535,
    };
    let columns: Vec<String> = shape
        .text_columns
        .iter()
        .map(|(i, _)| format!("{i}/2"))
        .collect();
    let language = if shape.decimal_comma { 1031 } else { 1033 };
    format!(
        "Text - txt - csv (StarCalc):{},34,{charset},1,{},{language},false,false,false,false,false,,false,,false",
        shape.delimiter,
        columns.join("/")
    )
}

/// Sets the import options for a CSV source from what was read of it. An
/// unread CSV keeps the default, which `Arg::CsvImport` renders.
pub(crate) fn resolve(probe: Option<&crate::MediaProbe>, r: &mut crate::video::ResolvedVideo) {
    if let Some(TableShape::Csv(shape)) = probe.and_then(|p| p.table.as_ref()) {
        r.csv_import = Some(import_filter(shape));
    }
}

/// The delimiters a CSV is tried with, in order of preference on a tie.
const DELIMITERS: [u8; 4] = *b",;\t|";

/// How many records decide the delimiter.
const SNIFF_RECORDS: usize = 50;

/// Reads a CSV's encoding, delimiter, decimal separator and the columns
/// that must stay text. Every record is looked at for the columns: a zip
/// code with a leading zero may first appear on row 40,000.
pub fn sniff_csv(bytes: &[u8]) -> Option<CsvShape> {
    let (text, encoding) = decode(bytes)?;
    let delimiter = DELIMITERS
        .iter()
        .copied()
        .max_by_key(|&d| {
            let counts: Vec<usize> = Records::new(&text, d)
                .take(SNIFF_RECORDS)
                .map(|r| r.len())
                .collect();
            let first = counts.first().copied().unwrap_or(0);
            let consistent = counts.iter().all(|&c| c == first);
            // `max_by_key` keeps the last of equal keys, so the order of
            // preference is reversed into the key.
            let preference = DELIMITERS.len() - DELIMITERS.iter().position(|&x| x == d).unwrap();
            (consistent && first > 1, first, preference)
        })
        .unwrap_or(b',');

    let mut header: Vec<String> = Vec::new();
    let mut text_columns: Vec<bool> = Vec::new();
    let (mut commas, mut points) = (0usize, 0usize);
    let mut formulas = false;
    for (row, record) in Records::new(&text, delimiter).enumerate() {
        if row == 0 {
            header = record.clone();
        }
        if text_columns.len() < record.len() {
            text_columns.resize(record.len(), false);
        }
        for (i, field) in record.iter().enumerate() {
            let f = field.trim();
            text_columns[i] |= keeps_digits(f);
            formulas |= f.starts_with('=');
            if is_decimal(f, ',') {
                commas += 1;
            } else if is_decimal(f, '.') {
                points += 1;
            }
        }
    }
    let text_columns = text_columns
        .iter()
        .enumerate()
        .filter(|(_, &text)| text)
        .map(|(i, _)| (i + 1, header.get(i).cloned().unwrap_or_default()))
        .collect();
    Some(CsvShape {
        delimiter,
        encoding,
        // With a comma between fields, a decimal comma would be quoted and
        // is far rarer than a thousands separator.
        decimal_comma: delimiter != b',' && commas > points,
        text_columns,
        formulas,
    })
}

/// A value whose digits a number would lose: a leading zero (`02134`), more
/// digits than a double holds (`4111111111111111`), or a leading `+`
/// (`+447946095800`).
fn keeps_digits(f: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    (f.len() >= 2 && f.starts_with('0') && digits(f))
        || (f.len() >= 16 && digits(f))
        || f.strip_prefix('+').is_some_and(digits)
}

/// `-12,5` with `sep` as the decimal separator: digits on both sides.
fn is_decimal(f: &str, sep: char) -> bool {
    let f = f.strip_prefix('-').unwrap_or(f);
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    f.split_once(sep)
        .is_some_and(|(a, b)| digits(a) && digits(b))
}

/// The text of a CSV and the encoding LibreOffice is to read it as.
fn decode(bytes: &[u8]) -> Option<(String, Encoding)> {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return Some((String::from_utf8_lossy(rest).into_owned(), Encoding::Utf8));
    }
    let utf16 = |rest: &[u8], big: bool| {
        let units: Vec<u16> = rest
            .chunks_exact(2)
            .map(|c| {
                if big {
                    u16::from_be_bytes([c[0], c[1]])
                } else {
                    u16::from_le_bytes([c[0], c[1]])
                }
            })
            .collect();
        Some((String::from_utf16_lossy(&units), Encoding::Utf16))
    };
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return utf16(rest, false);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return utf16(rest, true);
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => Some((text.to_string(), Encoding::Utf8)),
        Err(_) => Some((
            bytes.iter().map(|&b| windows_1252(b)).collect(),
            Encoding::Windows1252,
        )),
    }
}

/// One Windows-1252 byte as a character: Latin-1, except 0x80-0x9F.
fn windows_1252(b: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8D}', 'Ž',
        '\u{8F}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9D}',
        'ž', 'Ÿ',
    ];
    match b {
        0x80..=0x9F => HIGH[usize::from(b - 0x80)],
        _ => char::from(b),
    }
}

/// The records of a CSV, each a list of unquoted fields: fields separated
/// by `delimiter`, a field in double quotes may hold the delimiter and line
/// breaks, and `""` inside one is a quote.
struct Records<'a> {
    rest: std::iter::Peekable<std::str::Chars<'a>>,
    delimiter: char,
}

impl<'a> Records<'a> {
    fn new(text: &'a str, delimiter: u8) -> Self {
        Records {
            rest: text.chars().peekable(),
            delimiter: char::from(delimiter),
        }
    }
}

impl Iterator for Records<'_> {
    type Item = Vec<String>;

    fn next(&mut self) -> Option<Vec<String>> {
        self.rest.peek()?;
        let mut record = Vec::new();
        let mut field = String::new();
        let mut quoted = false;
        while let Some(c) = self.rest.next() {
            match c {
                '"' if quoted && self.rest.peek() == Some(&'"') => {
                    self.rest.next();
                    field.push('"');
                }
                '"' if quoted => quoted = false,
                '"' if field.is_empty() => quoted = true,
                c if quoted => field.push(c),
                c if c == self.delimiter => record.push(std::mem::take(&mut field)),
                '\r' if self.rest.peek() == Some(&'\n') => {}
                '\n' => break,
                c => field.push(c),
            }
        }
        record.push(field);
        Some(record)
    }
}

/// An .xlsx's sheet names, from `xl/workbook.xml`, and whether the first
/// has a formula, from the part `xl/_rels/workbook.xml.rels` maps it to.
fn xlsx(path: &Path) -> Option<WorkbookShape> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path).ok()?).ok()?;
    let workbook = entry_text(&mut zip, "xl/workbook.xml")?;
    let sheets: Vec<(String, String)> = tags(&workbook, "<sheet ")
        .map(|tag| Some((attr(tag, "name")?, attr(tag, "r:id")?)))
        .collect::<Option<_>>()?;
    let (_, first_id) = sheets.first()?;
    let rels = entry_text(&mut zip, "xl/_rels/workbook.xml.rels")?;
    let target = tags(&rels, "<Relationship ")
        .find(|tag| attr(tag, "Id").as_deref() == Some(first_id))
        .and_then(|tag| attr(tag, "Target"))?;
    // Relative to xl/, or absolute within the package.
    let part = match target.strip_prefix('/') {
        Some(absolute) => absolute.to_string(),
        None => format!("xl/{target}"),
    };
    let formulas = contains(zip.by_name(&part).ok()?, &[b"<f>", b"<f "])?;
    Some(WorkbookShape {
        sheets: sheets.into_iter().map(|(name, _)| name).collect(),
        formulas,
    })
}

/// An .ods's sheet names, and whether the first has a formula, from
/// `content.xml`: a sheet's start tag names it, and a formula is a
/// `table:formula` attribute on a cell before the second sheet starts.
/// Every sheet is in that one part, so it is read whole, as LibreOffice
/// will read it next.
fn ods(path: &Path) -> Option<WorkbookShape> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path).ok()?).ok()?;
    let mut content = String::new();
    zip.by_name("content.xml")
        .ok()?
        .read_to_string(&mut content)
        .ok()?;
    let starts: Vec<usize> = content
        .match_indices("<table:table ")
        .map(|(i, _)| i)
        .collect();
    let sheets = starts
        .iter()
        .map(|&i| attr(&content[i..], "table:name"))
        .collect::<Option<Vec<_>>>()?;
    let first = starts.first()?;
    let end = starts.get(1).copied().unwrap_or(content.len());
    Some(WorkbookShape {
        sheets,
        formulas: content[*first..end].contains("table:formula="),
    })
}

fn entry_text<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
) -> Option<String> {
    let mut text = String::new();
    zip.by_name(name).ok()?.read_to_string(&mut text).ok()?;
    Some(text)
}

/// Every start tag in `xml` that begins with `open`, up to its `>`.
fn tags<'a>(xml: &'a str, open: &'a str) -> impl Iterator<Item = &'a str> + 'a {
    xml.match_indices(open).filter_map(move |(i, _)| {
        let rest = &xml[i..];
        rest.find('>').map(|end| &rest[..end])
    })
}

/// The value of `name="..."` in a start tag, with XML's five escapes
/// undone.
fn attr(tag: &str, name: &str) -> Option<String> {
    let start = tag.find(&format!(" {name}=\""))? + name.len() + 3;
    let len = tag[start..].find('"')?;
    Some(
        tag[start..start + len]
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&"),
    )
}

/// Whether `reader` holds any of `needles`, read in chunks so a sheet of
/// any size costs a fixed amount of memory. The tail of each chunk is
/// kept, so a needle split across two chunks is still found.
fn contains(mut reader: impl Read, needles: &[&[u8]]) -> Option<bool> {
    let keep = needles.iter().map(|n| n.len()).max().unwrap_or(0);
    let mut buf = vec![0u8; 64 * 1024];
    let mut window: Vec<u8> = Vec::new();
    loop {
        let n = reader.read(&mut buf).ok()?;
        if n == 0 {
            return Some(false);
        }
        window.extend_from_slice(&buf[..n]);
        if needles
            .iter()
            .any(|needle| window.windows(needle.len()).any(|w| w == *needle))
        {
            return Some(true);
        }
        let cut = window.len().saturating_sub(keep);
        window.drain(..cut);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn sniff(text: &str) -> CsvShape {
        sniff_csv(text.as_bytes()).unwrap()
    }

    fn text_columns(shape: &CsvShape) -> Vec<(usize, &str)> {
        shape
            .text_columns
            .iter()
            .map(|(i, h)| (*i, h.as_str()))
            .collect()
    }

    /// The values a number would lose digits of keep their column as text:
    /// a leading zero, more digits than a double holds, a leading plus.
    /// Anywhere in the column, not only on the first row.
    #[test]
    fn columns_whose_digits_a_number_would_lose_stay_text() {
        let s = sniff(
            "zip,qty,card,phone,price\n\
             10001,3,12,555,1.5\n\
             02134,4,4111111111111111,+447946095800,2\n",
        );
        assert_eq!(
            text_columns(&s),
            vec![(1, "zip"), (3, "card"), (4, "phone")]
        );
        assert_eq!(s.delimiter, b',');
        assert!(!s.decimal_comma && !s.formulas);
    }

    /// Zero itself, a decimal below one, and a 15-digit number are numbers.
    #[test]
    fn ordinary_numbers_are_not_kept_as_text() {
        let s = sniff("a,b,c\n0,0.5,123456789012345\n");
        assert!(s.text_columns.is_empty(), "{s:?}");
    }

    #[test]
    fn a_quoted_field_is_judged_by_its_value() {
        let s = sniff("id,note\n\"007\",\"a, b\"\n\"12\",\"line\nbreak\"\n");
        assert_eq!(text_columns(&s), vec![(1, "id")]);
    }

    #[test]
    fn the_delimiter_is_the_one_that_splits_every_line_alike() {
        assert_eq!(sniff("a;b;c\n1,5;2;3\n").delimiter, b';');
        assert_eq!(sniff("a\tb\nx, y\tz\n").delimiter, b'\t');
        assert_eq!(sniff("a|b\n1|2\n").delimiter, b'|');
        assert_eq!(sniff("a,b\r\n1,2\r\n").delimiter, b',');
        assert_eq!(sniff("one column\nonly\n").delimiter, b',');
    }

    /// Decimal commas are only believed in a file whose fields are not
    /// comma-separated, and only when they outnumber decimal points.
    #[test]
    fn decimal_commas_come_with_semicolons() {
        assert!(sniff("name;price\nCafé;1,5\nThé;-12,25\n").decimal_comma);
        assert!(!sniff("name;price\nCafé;1.5\n").decimal_comma);
        assert!(!sniff("name,price\nCafé,\"1,5\"\n").decimal_comma);
    }

    #[test]
    fn a_cell_starting_with_equals_is_noticed() {
        assert!(sniff("a,b\n1,=SUM(A1:A2)\n").formulas);
        assert!(sniff("a,b\n1, =1+1\n").formulas);
        assert!(!sniff("a,b\n1,x=1\n").formulas);
    }

    #[test]
    fn the_encoding_is_utf8_utf16_by_its_mark_or_else_windows_1252() {
        let shape = |bytes: &[u8]| sniff_csv(bytes).unwrap();
        assert_eq!(
            shape(b"\xEF\xBB\xBFname\nCaf\xC3\xA9\n").encoding,
            Encoding::Utf8
        );
        assert_eq!(shape(b"name\nCaf\xC3\xA9\n").encoding, Encoding::Utf8);
        let win = shape(b"name;qty\nCaf\xE9;0042\n");
        assert_eq!(win.encoding, Encoding::Windows1252);
        assert_eq!(text_columns(&win), vec![(2, "qty")]);
        let mut utf16 = vec![0xFF, 0xFE];
        utf16.extend(
            "id\tname\n007\tZoë\n"
                .encode_utf16()
                .flat_map(u16::to_le_bytes),
        );
        let wide = shape(&utf16);
        assert_eq!(wide.encoding, Encoding::Utf16);
        assert_eq!(wide.delimiter, b'\t');
        assert_eq!(text_columns(&wide), vec![(1, "id")]);
    }

    #[test]
    fn windows_1252_decodes_its_own_high_range() {
        let decoded: String = [0x80u8, 0x93, 0x94, 0xE9]
            .iter()
            .map(|&b| windows_1252(b))
            .collect();
        assert_eq!(decoded, "€“”é");
    }

    /// Every token in its place: separator, quote, character set, first
    /// line, text columns, language, then the switches.
    #[test]
    fn the_import_filter_spells_every_token() {
        assert_eq!(
            import_filter(&CsvShape::default()),
            "Text - txt - csv (StarCalc):44,34,76,1,,1033,false,false,false,false,false,,false,,false"
        );
        let euro = CsvShape {
            delimiter: b';',
            encoding: Encoding::Windows1252,
            decimal_comma: true,
            text_columns: vec![(1, "zip".into()), (4, "id".into())],
            formulas: false,
        };
        let filter = import_filter(&euro);
        let tokens: Vec<&str> = filter.split_once(':').unwrap().1.split(',').collect();
        assert_eq!(tokens.len(), 15, "{filter}");
        assert_eq!(&tokens[..6], ["59", "34", "1", "1", "1/2/4/2", "1031"]);
    }

    fn zip_with(files: &[(&str, &str)]) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut zip = zip::ZipWriter::new(file.reopen().unwrap());
        for (name, text) in files {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(text.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
        file
    }

    #[test]
    fn an_xlsx_lists_its_sheets_and_checks_the_first_for_formulas() {
        let book = |first_sheet: &str| {
            zip_with(&[
                (
                    "xl/workbook.xml",
                    r#"<workbook><sheets><sheet name="Q&amp;A" sheetId="1" r:id="rId7"/><sheet name="Second" sheetId="2" r:id="rId8"/></sheets></workbook>"#,
                ),
                (
                    "xl/_rels/workbook.xml.rels",
                    r#"<Relationships><Relationship Id="rId8" Target="worksheets/sheet1.xml"/><Relationship Id="rId7" Target="/xl/worksheets/sheet2.xml"/></Relationships>"#,
                ),
                ("xl/worksheets/sheet1.xml", "<c><f>1+1</f></c>"),
                ("xl/worksheets/sheet2.xml", first_sheet),
            ])
        };
        let plain = xlsx(book("<c><v>1</v></c>").path()).unwrap();
        assert_eq!(plain.sheets, vec!["Q&A", "Second"]);
        assert!(!plain.formulas, "the formula is on the second sheet");
        let with = xlsx(book(r#"<c><f t="shared" ref="A1:A2">B1*2</f></c>"#).path()).unwrap();
        assert!(with.formulas);
    }

    #[test]
    fn an_ods_lists_its_sheets_and_checks_the_first_for_formulas() {
        let book = |first: &str| {
            zip_with(&[(
                "content.xml",
                &format!(
                    r#"<office:spreadsheet><table:table table:name="Orders">{first}</table:table><table:table table:name="Notes"><table:table-cell table:formula="of:=1"/></table:table></office:spreadsheet>"#
                ),
            )])
        };
        let plain = ods(book("<table:table-cell/>").path()).unwrap();
        assert_eq!(plain.sheets, vec!["Orders", "Notes"]);
        assert!(!plain.formulas);
        let with = ods(book(r#"<table:table-cell table:formula="of:=[.A1]*2"/>"#).path()).unwrap();
        assert!(with.formulas);
    }

    /// A needle split across two reads is still found.
    #[test]
    fn contains_finds_a_needle_across_chunks() {
        let mut text = vec![b'x'; 64 * 1024 - 2];
        text.extend_from_slice(b"<f>1</f>");
        assert_eq!(contains(&text[..], &[b"<f>"]), Some(true));
        assert_eq!(contains(&b"<v>1</v>"[..], &[b"<f>", b"<f "]), Some(false));
    }

    #[test]
    fn a_file_that_is_not_what_it_claims_reads_as_nothing() {
        let junk = zip_with(&[("mimetype", "nothing")]);
        assert_eq!(read(junk.path(), Format::Xlsx), None);
        assert_eq!(read(junk.path(), Format::Ods), None);
        assert_eq!(read(Path::new("no/such.csv"), Format::Csv), None);
    }
}
