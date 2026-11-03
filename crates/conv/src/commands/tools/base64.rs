//! `conv base64`: encode or decode base64, and data: URIs.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use convkit_core::tools::base64::{self as b64, Alphabet};
use convkit_core::{ConvError, ErrorCode};
use serde_json::json;

use super::{Common, Done, Input, Origin, Paint, Piped};
use crate::cli::Cli;
use crate::{input, output};

pub const ABOUT: &str = "\
Encodes text or a file as base64, or decodes it.

conv base64 \"hello\"              aGVsbG8=
conv base64 -f logo.png --data-uri  data:image/png;base64,...
conv base64 -d aGVsbG8=            hello

Decoding takes either alphabet (standard + / or URL-safe - _), with or \
without = padding, and ignores line breaks. A data: URI decodes too, and \
its type names the file.

Decoded text is printed. Decoded binary, and anything from a data: URI, is \
written as decoded.EXT here (or in -o DIR), named for what it is, unless \
it is piped somewhere: conv base64 -d ... > logo.png. With --json it is \
always written, and the JSON names the file.";

#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    /// The text to encode, or the base64 or data: URI to decode; - reads
    /// what is piped in.
    pub text: Option<String>,
    /// Decode instead of encode.
    #[arg(short = 'd', long)]
    pub decode: bool,
    /// Encode with the URL-safe alphabet (- and _ instead of + and /,
    /// without = padding), as URLs and JWTs carry it.
    #[arg(long, conflicts_with = "decode")]
    pub url: bool,
    /// Encode as a data: URI, data:TYPE;base64,..., with the type read
    /// from the data itself (or the file's extension).
    #[arg(long, conflicts_with_all = ["decode", "url"])]
    pub data_uri: bool,
    /// Write the decoded data into this folder, as decoded.EXT.
    #[arg(short = 'o', long, value_name = "DIR", requires = "decode")]
    pub outdir: Option<PathBuf>,
    /// Overwrite decoded.EXT if it is already there.
    #[arg(short = 'y', long, requires = "decode")]
    pub overwrite: bool,
    #[command(flatten)]
    pub io: Common,
}

pub fn run(cli: &Cli, args: &Args) -> i32 {
    let input = match super::read(cli, args.text.as_deref(), &args.io, Piped::Read, "base64") {
        Ok(Some(i)) => i,
        Ok(None) => {
            return super::fail(
                cli,
                &super::nothing("base64", "the text or file to encode, or base64 to decode"),
            )
        }
        Err(e) => return super::fail(cli, &e),
    };
    let done = if args.decode {
        decode(cli, args, input)
    } else {
        Ok(encode(args, input))
    };
    match done {
        Ok(Some(done)) => super::finish(cli, args.io.copy, done),
        Ok(None) => 0,
        Err(e) => super::fail(cli, &e),
    }
}

fn encode(args: &Args, input: Input) -> Option<Done> {
    let alphabet = if args.url {
        Alphabet::UrlSafe
    } else {
        Alphabet::Standard
    };
    let ext = input
        .origin
        .path()
        .and_then(|p| p.extension())
        .and_then(|e| e.to_str())
        .map(str::to_string);
    let (encoded, mime) = if args.data_uri {
        let mime = b64::mime_for(&input.bytes, ext.as_deref());
        (b64::data_uri(&input.bytes, &mime), Some(mime))
    } else {
        (b64::encode(&input.bytes, alphabet), None)
    };
    let json = json!({
        "results": [{
            "encoded": encoded,
            "bytes": input.bytes.len(),
            "alphabet": alphabet.name(),
            "mime": mime,
        }]
    });
    Some(Done::new(encoded.clone(), Some(encoded), json))
}

/// Decodes, then prints text, writes a file, or sends the bytes down the
/// pipe. `None` when the bytes went straight to stdout.
fn decode(cli: &Cli, args: &Args, input: Input) -> Result<Option<Done>, ConvError> {
    let from_arg = input.origin == Origin::Arg;
    let text = input.text()?;
    let decoded = b64::decode(&text).map_err(super::failed)?;
    let as_text = std::str::from_utf8(&decoded.bytes)
        .ok()
        .filter(|s| b64::is_text(s));
    // A data: URI names a file of its type; only plain text prints.
    let plain_text_uri = decoded
        .mime
        .as_deref()
        .is_none_or(|m| m.to_ascii_lowercase().starts_with("text/plain"));
    let prints = as_text.is_some() && plain_text_uri && args.outdir.is_none();
    if prints {
        let text = as_text.expect("checked").to_string();
        let json = json!({
            "results": [{
                "text": text,
                "bytes": decoded.bytes.len(),
                "mime": decoded.mime,
                "alphabet": decoded.alphabet.map(Alphabet::name),
            }]
        });
        return Ok(Some(Done::new(text.clone(), Some(text), json)));
    }
    // Binary down a pipe: the bytes themselves, as base64 -d does.
    if args.outdir.is_none() && !cli.json && !std::io::stdout().is_terminal() {
        let mut out = std::io::stdout().lock();
        out.write_all(&decoded.bytes)
            .and_then(|()| out.flush())
            .map_err(|e| super::failed(format!("couldn't write the decoded data: {e}")))?;
        return Ok(None);
    }
    let ext = decoded
        .mime
        .as_deref()
        .and_then(b64::ext_for_mime)
        .or_else(|| b64::sniff(&decoded.bytes).and_then(b64::ext_for_mime))
        .unwrap_or(if as_text.is_some() { "txt" } else { "bin" });
    let name = PathBuf::from(format!("decoded.{ext}"));
    let path = input::in_outdir(&name, args.outdir.as_deref())?;
    if path.exists() && !args.overwrite {
        return Err(ConvError::new(
            ErrorCode::OutputExists,
            format!("{} exists; pass -y to overwrite", path.display()),
        ));
    }
    output::ensure_dir(args.outdir.as_deref())?;
    output::write_file(&path, &decoded.bytes)?;
    if from_arg && as_text.is_none() && decoded.mime.is_none() {
        super::note(
            cli,
            "the decoded data isn't text, so it was written to a file; pipe it (> FILE) to name it yourself",
        );
    }
    let mime = decoded
        .mime
        .clone()
        .or_else(|| b64::sniff(&decoded.bytes).map(str::to_string));
    let human = super::written_human(&path, decoded.bytes.len() as u64, Paint::stdout());
    let json = json!({
        "results": [{
            "output": std::path::absolute(&path).unwrap_or(path.clone()),
            "bytes": decoded.bytes.len(),
            "mime": mime,
            "alphabet": decoded.alphabet.map(Alphabet::name),
        }]
    });
    let mut done = Done::new(human, None, json);
    done.copy_file = Some(path);
    Ok(Some(done))
}
