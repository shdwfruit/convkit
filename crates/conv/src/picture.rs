//! Reading a picture for the commands that look at its pixels (`conv
//! colors`, `conv beautify`): itself when conv decodes the format, and
//! otherwise converted to a PNG by ImageMagick first, the way `conv
//! photo.heic .png` would.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use convkit_core::picture::{self, Picture};
use convkit_core::{registry, ConvError, ErrorCode, Format, Kind};

use crate::cli::Cli;
use crate::{input, output};

/// A picture and what the reader should know about how it was read.
pub struct Read {
    pub picture: Picture,
    pub notes: Vec<String>,
}

/// Reads the picture at `path` for `command` (`conv colors`), drawing an
/// SVG `svg_scale` times its own size.
pub fn read(cli: &Cli, path: &Path, command: &str, svg_scale: u32) -> Result<Read, ConvError> {
    if path.is_dir() {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!(
                "{} is a folder; {command} takes one picture",
                path.display()
            ),
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| {
        let code = if e.kind() == std::io::ErrorKind::NotFound {
            ErrorCode::InputNotFound
        } else {
            ErrorCode::InvalidInvocation
        };
        let message = if code == ErrorCode::InputNotFound {
            format!("no such file: {}", path.display())
        } else {
            format!("cannot read {}: {e}", path.display())
        };
        ConvError::new(code, message)
    })?;
    let format = match Format::from_path(path) {
        Some(f) if f.kind() == Kind::Image => f,
        Some(_) => {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            return Err(ConvError::new(
                ErrorCode::UnsupportedPair,
                format!("{command} reads pictures, and {name} isn't one"),
            ));
        }
        None => picture::sniff(&bytes).ok_or_else(|| {
            ConvError::new(
                ErrorCode::UnsupportedPair,
                format!(
                    "{command} can't tell what kind of picture {} is",
                    path.display()
                ),
            )
        })?,
    };
    decode(cli, &bytes, format, Some(path), svg_scale)
}

/// Reads a picture piped in on stdin, telling its format from its bytes.
pub fn read_stdin(cli: &Cli, command: &str, svg_scale: u32) -> Result<Read, ConvError> {
    let mut bytes = Vec::new();
    std::io::stdin().read_to_end(&mut bytes).map_err(|e| {
        ConvError::new(
            ErrorCode::InvalidInvocation,
            format!("couldn't read what was piped in: {e}"),
        )
    })?;
    if bytes.is_empty() {
        return Err(ConvError::new(
            ErrorCode::InvalidInvocation,
            format!("nothing was piped in; give {command} a picture"),
        ));
    }
    let format = picture::sniff(&bytes).ok_or_else(|| {
        ConvError::new(
            ErrorCode::UnsupportedPair,
            "what was piped in isn't a picture conv knows",
        )
    })?;
    decode(cli, &bytes, format, None, svg_scale)
}

/// Decodes `bytes` in `format`, from the file at `path` when there is one.
fn decode(
    cli: &Cli,
    bytes: &[u8],
    format: Format,
    path: Option<&Path>,
    svg_scale: u32,
) -> Result<Read, ConvError> {
    let mut notes = Vec::new();
    if format == Format::Svg && convkit_core::ascii::svg_has_text(bytes) {
        notes.push(
            "the SVG's text is not drawn; convert its text to outlines (paths) to keep it"
                .to_string(),
        );
    }
    // ImageMagick reads what conv doesn't, and a file conv's own decoder
    // can't (a CMYK JPEG, say).
    let converts = format != Format::Png && registry::lookup(format, Format::Png).is_some();
    if picture::decodes_itself(format) {
        match picture::decode_scaled(bytes, format, svg_scale) {
            Ok(picture) => return Ok(Read { picture, notes }),
            Err(e) if !converts || format == Format::Svg => return Err(e),
            Err(_) => {}
        }
    }
    if !converts {
        return Err(ConvError::new(
            ErrorCode::UnsupportedPair,
            format!("conv can't read a .{} picture", format.ext()),
        ));
    }
    let picture = converted(cli, format, bytes, path)?;
    Ok(Read { picture, notes })
}

/// The picture converted to PNG by the ordinary `format -> png` recipe,
/// which turns it upright and keeps its colour profile. A file that holds
/// several pictures (an icon's sizes) gives the largest.
fn converted(
    cli: &Cli,
    format: Format,
    bytes: &[u8],
    path: Option<&Path>,
) -> Result<Picture, ConvError> {
    let (dir, temp) = output::private_temp("picture.png")?;
    let source: PathBuf = match path {
        Some(p) => p.to_path_buf(),
        None => {
            let piped = dir.path().join(format!("piped.{}", format.ext()));
            std::fs::write(&piped, bytes).map_err(|e| {
                ConvError::new(
                    ErrorCode::ConversionFailed,
                    format!("cannot write {}: {e}", piped.display()),
                )
            })?;
            piped
        }
    };
    output::convert(cli, format, &source, Format::Png, &temp, true)?;
    let mut pngs: Vec<PathBuf> = std::fs::read_dir(dir.path())
        .map_err(|e| {
            ConvError::new(
                ErrorCode::ConversionFailed,
                format!("cannot read {}: {e}", dir.path().display()),
            )
        })?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "png"))
        .collect();
    pngs.sort_by(|a, b| input::natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()));
    let mut best: Option<Picture> = None;
    for png in pngs {
        let bytes = std::fs::read(&png).map_err(|e| {
            ConvError::new(
                ErrorCode::ConversionFailed,
                format!("cannot read {}: {e}", png.display()),
            )
        })?;
        let p = picture::decode(&bytes, Format::Png)?;
        let size = |p: &Picture| u64::from(p.image.width) * u64::from(p.image.height);
        if best.as_ref().is_none_or(|b| size(&p) > size(b)) {
            best = Some(p);
        }
    }
    best.ok_or_else(|| {
        ConvError::new(
            ErrorCode::ConversionFailed,
            "ImageMagick wrote no picture to read",
        )
    })
}
