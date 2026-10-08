//! What an Office file holds that LibreOffice will not say: whether it is
//! encrypted with a password, and whether it carries macros.
//!
//! LibreOffice reports a password-protected `.doc` or `.xls` as "source
//! file could not be loaded", the same words as a corrupt file, and it can
//! do worse with a password-protected `.docx`: one it does not recognise
//! as encrypted is imported as plain text, so the PDF is pages of the
//! encrypted bytes and the run exits 0. It also drops macros on the way to
//! `.docx`/`.xlsx`/`.pptx`, which cannot hold them, without a word.
//!
//! `.doc`, `.xls` and `.ppt` are OLE compound files ([MS-CFB]): a small FAT
//! file system inside one file. A password-protected `.docx`/`.xlsx`/`.pptx`
//! is one too, with the real zip encrypted inside it. This module reads
//! that container just far enough to find a few streams by name and look
//! at their first bytes. It reads headers and chains, never a whole file.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::Format;

/// What `traits` found. Both are `false` for a file that is not a compound
/// file at all: a zipped `.docx`, or the RTF or HTML that old `.doc` files
/// sometimes really are.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OfficeTraits {
    pub encrypted: bool,
    pub macros: bool,
}

/// Reads `path`'s traits. `None` when the file cannot be read, or claims to
/// be a compound file but is damaged, so a caller keeps the notes that
/// depend on it.
pub fn traits(path: &Path) -> Option<OfficeTraits> {
    let mut file = File::open(path).ok()?;
    let mut magic = [0u8; 8];
    match file.read_exact(&mut magic) {
        Ok(()) if magic == MAGIC => {}
        Ok(()) => return Some(OfficeTraits::default()),
        // Shorter than the magic: not a compound file either.
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Some(OfficeTraits::default())
        }
        Err(_) => return None,
    }
    let mut cfb = Cfb::open(file)?;
    Some(OfficeTraits {
        encrypted: encrypted(&mut cfb)?,
        macros: macros(&mut cfb)?,
    })
}

/// The application a person removes the password with, for the message.
pub fn app_for(from: Format) -> &'static str {
    match from {
        Format::Xls | Format::Xlsx => "Excel",
        Format::Ppt | Format::Pptx => "PowerPoint",
        _ => "Word",
    }
}

const MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
/// Sector ids at or above this are markers (end of chain, free, FAT), not
/// sectors.
const MAX_REGULAR_SECTOR: u32 = 0xFFFF_FFFA;
const NO_STREAM: u32 = 0xFFFF_FFFF;
const STORAGE: u8 = 1;
const STREAM: u8 = 2;

/// A Word file's FIB opens with this (Word 97 and later) or `0xA5DC` (Word
/// 6 and 95); bit 8 of the flags word at offset 10 is `fEncrypted`.
const WORD_IDENTS: [u16; 2] = [0xA5EC, 0xA5DC];
const WORD_ENCRYPTED: u16 = 0x0100;
/// Excel's BOF and EOF records, and FILEPASS, which follows BOF in the
/// workbook globals of an encrypted file. Record headers stay in the clear
/// when the rest is encrypted.
const XLS_BOF: u16 = 0x0809;
const XLS_EOF: u16 = 0x000A;
const XLS_FILEPASS: u16 = 0x002F;
/// PowerPoint's CurrentUserAtom carries this token instead of 0xE391C05F
/// when the presentation is encrypted.
const PPT_ENCRYPTED_TOKEN: u32 = 0xF3D1C4DF;
/// PowerPoint refers to its VBA project from inside the document stream:
/// DocumentContainer > DocInfoListContainer > VBAInfoContainer >
/// VBAInfoAtom, whose first field is the project's persist id.
const PPT_DOCUMENT: u16 = 0x03E8;
const PPT_DOC_INFO_LIST: u16 = 0x07D0;
const PPT_VBA_INFO: u16 = 0x03FF;
const PPT_VBA_INFO_ATOM: u16 = 0x0400;

fn encrypted(cfb: &mut Cfb) -> Option<bool> {
    // A password-protected .docx/.xlsx/.pptx: the zip itself, encrypted.
    if cfb.root_entry("EncryptedPackage").is_some() {
        return Some(true);
    }
    if let Some(word) = cfb.root_entry("WordDocument") {
        let fib = cfb.read(&word, 0, 12)?;
        let ident = u16_at(&fib, 0);
        return Some(WORD_IDENTS.contains(&ident) && u16_at(&fib, 10) & WORD_ENCRYPTED != 0);
    }
    if let Some(book) = cfb
        .root_entry("Workbook")
        .or_else(|| cfb.root_entry("Book"))
    {
        return xls_has_filepass(cfb, &book);
    }
    if let Some(user) = cfb.root_entry("Current User") {
        let atom = cfb.read(&user, 0, 16)?;
        return Some(u32_at(&atom, 12) == PPT_ENCRYPTED_TOKEN);
    }
    Some(false)
}

/// Walks the workbook globals from BOF to EOF looking for FILEPASS.
fn xls_has_filepass(cfb: &mut Cfb, book: &Entry) -> Option<bool> {
    let mut pos = 0;
    for i in 0..4096 {
        if pos + 4 > book.size {
            return Some(false);
        }
        let header = cfb.read(book, pos, 4)?;
        let kind = u16_at(&header, 0);
        if i == 0 && kind != XLS_BOF {
            return Some(false);
        }
        match kind {
            XLS_FILEPASS => return Some(true),
            XLS_EOF => return Some(false),
            _ => pos += 4 + u64::from(u16_at(&header, 2)),
        }
    }
    Some(false)
}

fn macros(cfb: &mut Cfb) -> Option<bool> {
    // Word keeps its VBA project in a `Macros` storage, Excel in
    // `_VBA_PROJECT_CUR`.
    let storage = |cfb: &Cfb, name| cfb.root_entry(name).is_some_and(|e| e.kind == STORAGE);
    if storage(cfb, "Macros") || storage(cfb, "_VBA_PROJECT_CUR") {
        return Some(true);
    }
    match cfb.root_entry("PowerPoint Document") {
        Some(doc) => {
            let path = [
                PPT_DOCUMENT,
                PPT_DOC_INFO_LIST,
                PPT_VBA_INFO,
                PPT_VBA_INFO_ATOM,
            ];
            ppt_has_vba(cfb, &doc, 0, doc.size, &path)
        }
        None => Some(false),
    }
}

/// Follows the record path `path` in `entry` between `start` and `end`:
/// the first type is looked for among the records there, and each match's
/// children are searched for the rest. Only containers on the path are
/// entered, so this reads a few dozen headers, not every record. At the
/// end of the path is a VBAInfoAtom, and a project exists only if its
/// persist id is not zero: LibreOffice writes the container into every
/// .ppt it saves, with a zero id.
fn ppt_has_vba(cfb: &mut Cfb, entry: &Entry, start: u64, end: u64, path: &[u16]) -> Option<bool> {
    let Some((&wanted, rest)) = path.split_first() else {
        return Some(start + 4 <= end && u32_at(&cfb.read(entry, start, 4)?, 0) != 0);
    };
    let mut pos = start;
    for _ in 0..65_536 {
        if pos + 8 > end {
            return Some(false);
        }
        let header = cfb.read(entry, pos, 8)?;
        let len = u64::from(u32_at(&header, 4));
        let body = pos + 8;
        if u16_at(&header, 2) == wanted
            && ppt_has_vba(cfb, entry, body, (body + len).min(end), rest)?
        {
            return Some(true);
        }
        pos = body + len;
    }
    Some(false)
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

#[derive(Debug, Clone)]
struct Entry {
    name: String,
    kind: u8,
    left: u32,
    right: u32,
    child: u32,
    start: u32,
    size: u64,
}

/// An open compound file: its allocation tables and directory.
struct Cfb {
    file: File,
    sector_size: u64,
    fat: Vec<u32>,
    mini_fat: Vec<u32>,
    entries: Vec<Entry>,
    /// Streams smaller than this live in the mini stream, in 64-byte
    /// sectors chained through `mini_fat`.
    mini_cutoff: u64,
    /// The mini stream itself, which is the root entry's own data.
    mini_stream: Vec<u32>,
}

impl Cfb {
    /// Reads the header, the FAT, the mini FAT and the directory. `None`
    /// for anything inconsistent: the file is damaged, and guessing would
    /// only invent an answer.
    fn open(mut file: File) -> Option<Cfb> {
        let len = file.metadata().ok()?.len();
        let mut header = [0u8; 512];
        file.seek(SeekFrom::Start(0)).ok()?;
        file.read_exact(&mut header).ok()?;
        let sector_size: u64 = match u16_at(&header, 0x1E) {
            9 => 512,
            12 => 4096,
            _ => return None,
        };
        // Every chain is bounded by the sectors the file can hold, so a
        // corrupt table cannot loop or allocate without limit.
        let sectors = len / sector_size;
        let fat_sectors = u64::from(u32_at(&header, 0x2C));
        if fat_sectors > sectors {
            return None;
        }
        let mut cfb = Cfb {
            file,
            sector_size,
            fat: Vec::new(),
            mini_fat: Vec::new(),
            entries: Vec::new(),
            mini_cutoff: u64::from(u32_at(&header, 0x38)),
            mini_stream: Vec::new(),
        };

        // The FAT's own sectors are listed in the header's DIFAT, then in
        // a chain of DIFAT sectors for a file too large for 109 of them.
        let mut fat_ids: Vec<u32> = (0..109)
            .map(|i| u32_at(&header, 0x4C + i * 4))
            .take_while(|&id| id <= MAX_REGULAR_SECTOR)
            .collect();
        let per_sector = (sector_size / 4) as usize;
        let mut difat = u32_at(&header, 0x44);
        for _ in 0..u32_at(&header, 0x48).min(sectors as u32) {
            if difat > MAX_REGULAR_SECTOR {
                break;
            }
            let ids = cfb.sector(difat)?;
            fat_ids.extend(
                (0..per_sector - 1)
                    .map(|i| u32_at(&ids, i * 4))
                    .take_while(|&id| id <= MAX_REGULAR_SECTOR),
            );
            difat = u32_at(&ids, (per_sector - 1) * 4);
        }
        fat_ids.truncate(fat_sectors as usize);
        for id in fat_ids {
            let sector = cfb.sector(id)?;
            cfb.fat
                .extend((0..per_sector).map(|i| u32_at(&sector, i * 4)));
        }

        let mini_fat = cfb.chain(u32_at(&header, 0x3C))?;
        for id in mini_fat {
            let sector = cfb.sector(id)?;
            cfb.mini_fat
                .extend((0..per_sector).map(|i| u32_at(&sector, i * 4)));
        }

        // A version 3 file keeps only the low half of each stream size;
        // the high half is undefined there.
        let v3 = sector_size == 512;
        for id in cfb.chain(u32_at(&header, 0x30))? {
            let sector = cfb.sector(id)?;
            for raw in sector.chunks_exact(128) {
                let name_len = (u16_at(raw, 0x40) as usize).min(64);
                let units: Vec<u16> = raw[..name_len]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .take_while(|&u| u != 0)
                    .collect();
                let size = u64::from_le_bytes(raw[0x78..0x80].try_into().unwrap());
                cfb.entries.push(Entry {
                    name: String::from_utf16_lossy(&units),
                    kind: raw[0x42],
                    left: u32_at(raw, 0x44),
                    right: u32_at(raw, 0x48),
                    child: u32_at(raw, 0x4C),
                    start: u32_at(raw, 0x74),
                    size: if v3 { size & 0xFFFF_FFFF } else { size },
                });
            }
        }
        let root = cfb.entries.first()?.clone();
        cfb.mini_stream = cfb.chain(root.start)?;
        Some(cfb)
    }

    /// One whole sector.
    fn sector(&mut self, id: u32) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; self.sector_size as usize];
        self.file
            .seek(SeekFrom::Start((u64::from(id) + 1) * self.sector_size))
            .ok()?;
        self.file.read_exact(&mut buf).ok()?;
        Some(buf)
    }

    /// A chain through the FAT, from `start` to its end.
    fn chain(&self, start: u32) -> Option<Vec<u32>> {
        follow(&self.fat, start)
    }

    /// The root storage's direct child named `name`. The directory is a
    /// tree, and the same names recur below the root: an Excel chart
    /// embedded in a Word file has its own `Workbook` stream.
    fn root_entry(&self, name: &str) -> Option<Entry> {
        let mut stack = vec![self.entries.first()?.child];
        let mut seen = 0;
        while let Some(id) = stack.pop() {
            seen += 1;
            if id == NO_STREAM || seen > self.entries.len() {
                continue;
            }
            let e = self.entries.get(id as usize)?;
            if e.name.eq_ignore_ascii_case(name) && matches!(e.kind, STORAGE | STREAM) {
                return Some(e.clone());
            }
            stack.push(e.left);
            stack.push(e.right);
        }
        None
    }

    /// `len` bytes of `entry` from `offset`, or `None` if the stream is
    /// shorter or its chain is broken.
    fn read(&mut self, entry: &Entry, offset: u64, len: usize) -> Option<Vec<u8>> {
        if offset + len as u64 > entry.size {
            return None;
        }
        let mini = entry.size < self.mini_cutoff;
        let (unit, chain) = if mini {
            (64, follow(&self.mini_fat, entry.start)?)
        } else {
            (self.sector_size, self.chain(entry.start)?)
        };
        let mut out = Vec::with_capacity(len);
        let mut pos = offset;
        while out.len() < len {
            let id = *chain.get((pos / unit) as usize)?;
            let within = pos % unit;
            let take = ((unit - within) as usize).min(len - out.len());
            let at = if mini {
                // A mini sector lives at its offset in the mini stream,
                // which is itself a chain of ordinary sectors.
                let in_stream = u64::from(id) * 64 + within;
                let sector = *self
                    .mini_stream
                    .get((in_stream / self.sector_size) as usize)?;
                (u64::from(sector) + 1) * self.sector_size + in_stream % self.sector_size
            } else {
                (u64::from(id) + 1) * self.sector_size + within
            };
            let mut buf = vec![0u8; take];
            self.file.seek(SeekFrom::Start(at)).ok()?;
            self.file.read_exact(&mut buf).ok()?;
            out.extend(buf);
            pos += take as u64;
        }
        Some(out)
    }
}

/// Follows a chain through `table` from `start`. A chain longer than the
/// table, or one that points outside it, is corrupt.
fn follow(table: &[u32], start: u32) -> Option<Vec<u32>> {
    let mut out = Vec::new();
    let mut id = start;
    while id <= MAX_REGULAR_SECTOR {
        if out.len() > table.len() {
            return None;
        }
        out.push(id);
        id = *table.get(id as usize)?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const END: u32 = 0xFFFF_FFFE;
    const FREE: u32 = 0xFFFF_FFFF;

    /// One directory entry for `compound`: a name, a stream's bytes (or
    /// `None` for a storage), and the index of the storage it sits in, 0
    /// being the root.
    struct Node {
        name: &'static str,
        data: Option<Vec<u8>>,
        parent: usize,
    }

    fn stream(name: &'static str, data: Vec<u8>) -> Node {
        Node {
            name,
            data: Some(data),
            parent: 0,
        }
    }

    fn storage(name: &'static str) -> Node {
        Node {
            name,
            data: None,
            parent: 0,
        }
    }

    /// A version 3 compound file holding `nodes`, written the plain way:
    /// one FAT sector, the directory, then each stream in ordinary
    /// sectors. The mini stream, which small real files use, is covered by
    /// reading the committed fixtures.
    fn compound(nodes: &[Node]) -> tempfile::NamedTempFile {
        const SECTOR: usize = 512;
        const STREAM_SECTORS: usize = 8;
        let entries = nodes.len() + 1;
        let dir_sectors = entries.div_ceil(4);
        // Chains sectors `first..=last` in order.
        let link = |fat: &mut [u32], first: usize, last: usize| {
            for (s, id) in (first..).zip(&mut fat[first..=last]) {
                *id = if s == last { END } else { s as u32 + 1 };
            }
        };
        let mut fat = vec![FREE; SECTOR / 4];
        fat[0] = 0xFFFF_FFFD; // the FAT's own sector
        link(&mut fat, 1, dir_sectors);
        let mut next = 1 + dir_sectors;
        let mut starts = Vec::new();
        for node in nodes {
            if node.data.is_some() {
                starts.push(next as u32);
                link(&mut fat, next, next + STREAM_SECTORS - 1);
                next += STREAM_SECTORS;
            } else {
                starts.push(END);
            }
        }

        let mut header = vec![0u8; SECTOR];
        header[..8].copy_from_slice(&MAGIC);
        let fields: [(usize, &[u8]); 10] = [
            (0x18, &0x3Eu16.to_le_bytes()),
            (0x1A, &3u16.to_le_bytes()),
            (0x1C, &0xFFFEu16.to_le_bytes()),
            (0x1E, &9u16.to_le_bytes()),
            (0x20, &6u16.to_le_bytes()),
            (0x2C, &1u32.to_le_bytes()),
            (0x30, &1u32.to_le_bytes()),
            (0x38, &4096u32.to_le_bytes()),
            (0x3C, &END.to_le_bytes()),
            (0x44, &END.to_le_bytes()),
        ];
        for (at, bytes) in fields {
            header[at..at + bytes.len()].copy_from_slice(bytes);
        }
        for i in 0..109 {
            let id: u32 = if i == 0 { 0 } else { FREE };
            header[0x4C + i * 4..0x50 + i * 4].copy_from_slice(&id.to_le_bytes());
        }

        // Each storage's children hang off its `child` as a chain of right
        // siblings.
        let mut child = vec![NO_STREAM; entries];
        let mut right = vec![NO_STREAM; entries];
        for (i, node) in nodes.iter().enumerate().rev() {
            right[i + 1] = child[node.parent];
            child[node.parent] = i as u32 + 1;
        }
        let mut dir = vec![0u8; dir_sectors * SECTOR];
        let names = std::iter::once("Root Entry").chain(nodes.iter().map(|n| n.name));
        for (i, name) in names.enumerate() {
            let e = &mut dir[i * 128..(i + 1) * 128];
            let units: Vec<u16> = name.encode_utf16().chain([0]).collect();
            for (j, u) in units.iter().enumerate() {
                e[j * 2..j * 2 + 2].copy_from_slice(&u.to_le_bytes());
            }
            e[0x40..0x42].copy_from_slice(&(units.len() as u16 * 2).to_le_bytes());
            let (kind, start, size) = match i {
                0 => (5, END, 0),
                _ => match &nodes[i - 1].data {
                    Some(d) => (STREAM, starts[i - 1], d.len() as u64),
                    None => (STORAGE, END, 0),
                },
            };
            e[0x42] = kind;
            e[0x43] = 1;
            e[0x44..0x48].copy_from_slice(&NO_STREAM.to_le_bytes());
            e[0x48..0x4C].copy_from_slice(&right[i].to_le_bytes());
            e[0x4C..0x50].copy_from_slice(&child[i].to_le_bytes());
            e[0x74..0x78].copy_from_slice(&start.to_le_bytes());
            e[0x78..0x80].copy_from_slice(&size.to_le_bytes());
        }

        let mut bytes = header;
        bytes.extend(fat.iter().flat_map(|id| id.to_le_bytes()));
        bytes.extend(dir);
        for node in nodes {
            if let Some(d) = &node.data {
                let mut padded = d.clone();
                padded.resize(SECTOR * STREAM_SECTORS, 0);
                bytes.extend(padded);
            }
        }
        let mut file = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut file, &bytes).unwrap();
        file
    }

    fn read(nodes: &[Node]) -> OfficeTraits {
        traits(compound(nodes).path()).expect("a well-formed compound file")
    }

    /// Pads a stream to 4096 bytes, so it is not a mini stream and its
    /// size covers whatever was written into it.
    fn sized(mut data: Vec<u8>) -> Vec<u8> {
        data.resize(4096, 0);
        data
    }

    fn fib(flags: u16) -> Vec<u8> {
        let mut fib = vec![0u8; 12];
        fib[..2].copy_from_slice(&0xA5ECu16.to_le_bytes());
        fib[10..12].copy_from_slice(&flags.to_le_bytes());
        sized(fib)
    }

    fn biff(list: &[(u16, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (kind, body) in list {
            out.extend(kind.to_le_bytes());
            out.extend((body.len() as u16).to_le_bytes());
            out.extend(*body);
        }
        sized(out)
    }

    #[test]
    fn a_word_file_is_encrypted_when_its_fib_says_so() {
        assert!(read(&[stream("WordDocument", fib(0x0100))]).encrypted);
        assert!(!read(&[stream("WordDocument", fib(0x0004))]).encrypted);
    }

    #[test]
    fn an_excel_file_is_encrypted_when_filepass_follows_bof() {
        let bof = (XLS_BOF, &[0u8; 16][..]);
        let filepass = (XLS_FILEPASS, &[0u8; 54][..]);
        let eof = (XLS_EOF, &[][..]);
        assert!(read(&[stream("Workbook", biff(&[bof, filepass, eof]))]).encrypted);
        let codepage = (0x0042, &[0xE4, 0x04][..]);
        assert!(!read(&[stream("Workbook", biff(&[bof, codepage, eof]))]).encrypted);
        // Not workbook globals at all: nothing is claimed.
        assert!(!read(&[stream("Workbook", biff(&[filepass]))]).encrypted);
    }

    #[test]
    fn a_powerpoint_file_is_encrypted_when_its_current_user_token_says_so() {
        let atom = |token: u32| {
            let mut a = vec![0u8; 16];
            a[12..].copy_from_slice(&token.to_le_bytes());
            sized(a)
        };
        assert!(read(&[stream("Current User", atom(PPT_ENCRYPTED_TOKEN))]).encrypted);
        assert!(!read(&[stream("Current User", atom(0xE391_C05F))]).encrypted);
    }

    #[test]
    fn an_encrypted_package_is_a_password_protected_docx_xlsx_or_pptx() {
        let t = read(&[
            stream("EncryptionInfo", sized(vec![4, 0, 4, 0])),
            stream("EncryptedPackage", sized(vec![1; 8])),
        ]);
        assert!(t.encrypted);
    }

    #[test]
    fn word_and_excel_macros_are_a_vba_storage_at_the_root() {
        assert!(read(&[stream("WordDocument", fib(0)), storage("Macros")]).macros);
        assert!(read(&[storage("_VBA_PROJECT_CUR")]).macros);
        assert!(!read(&[stream("WordDocument", fib(0))]).macros);
    }

    /// A VBA project inside an embedded object belongs to that object, not
    /// to the file.
    #[test]
    fn only_entries_at_the_root_count() {
        let t = read(&[
            stream("WordDocument", fib(0)),
            storage("ObjectPool"),
            Node {
                name: "_VBA_PROJECT_CUR",
                data: None,
                parent: 2,
            },
        ]);
        assert!(!t.macros, "{t:?}");
    }

    /// PowerPoint refers to its project from inside its document stream.
    /// LibreOffice writes that reference into every .ppt with a zero id,
    /// which is no project at all.
    #[test]
    fn powerpoint_macros_are_a_nonzero_vba_reference() {
        let record = |ver: u8, kind: u16, body: Vec<u8>| {
            let mut r = vec![ver, 0x00];
            r.extend(kind.to_le_bytes());
            r.extend((body.len() as u32).to_le_bytes());
            r.extend(body);
            r
        };
        let deck = |persist_id: u32| {
            let mut atom = persist_id.to_le_bytes().to_vec();
            atom.extend([0; 8]);
            let atom = record(0x02, PPT_VBA_INFO_ATOM, atom);
            let info = record(0x0F, PPT_VBA_INFO, atom);
            let list = record(0x0F, PPT_DOC_INFO_LIST, info);
            sized(record(0x0F, PPT_DOCUMENT, list))
        };
        assert!(read(&[stream("PowerPoint Document", deck(3))]).macros);
        assert!(!read(&[stream("PowerPoint Document", deck(0))]).macros);
    }

    /// RTF or HTML saved as .doc, and a zipped .docx, are not compound
    /// files: they hold neither, which is an answer, not a failed read.
    #[test]
    fn a_file_that_is_not_a_compound_file_holds_neither() {
        let samples: [&[u8]; 4] = [
            b"{\\rtf1\\ansi hello}",
            b"<html></html>",
            b"PK\x03\x04",
            b"x",
        ];
        for bytes in samples {
            let mut f = tempfile::NamedTempFile::new().unwrap();
            std::io::Write::write_all(&mut f, bytes).unwrap();
            assert_eq!(traits(f.path()), Some(OfficeTraits::default()));
        }
    }

    /// A damaged compound file is unread, so the notes that depend on it
    /// stay, rather than a guess.
    #[test]
    fn a_damaged_compound_file_is_unread() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        let mut bytes = MAGIC.to_vec();
        bytes.resize(700, 0xAB);
        std::io::Write::write_all(&mut f, &bytes).unwrap();
        assert_eq!(traits(f.path()), None);
        assert_eq!(traits(Path::new("no/such/file.doc")), None);
    }

    /// The committed fixtures were saved with a password by LibreOffice.
    /// Their small streams live in the mini stream, which `compound` above
    /// never writes.
    #[test]
    fn the_password_fixtures_read_as_encrypted() {
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures");
        let names = [
            "encrypted.doc",
            "encrypted.xls",
            "encrypted.docx",
            "default-password.xls",
        ];
        for name in names {
            let t = traits(&fixtures.join(name)).unwrap();
            assert!(t.encrypted && !t.macros, "{name}: {t:?}");
        }
        let plain = traits(&fixtures.join("sample.docx")).unwrap();
        assert_eq!(plain, OfficeTraits::default());
    }
}
