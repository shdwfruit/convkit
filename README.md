# convkit

One command for everyday file conversion. `conv` maps a source format and a
target format onto an expert-tuned invocation of the right backend — ffmpeg,
ImageMagick, LibreOffice, pandoc, or Typst — and runs it locally: 115
conversion pairs across 27 formats (`conv capabilities` is the source of
truth). Files never leave your machine; only `conv install`/`conv update`
ever touch the network.

```console
$ conv clip.mkv clip.mp4
OK clip.mp4 - 55 KB - 0.1s - stream copy, no re-encode
  /home/rick/Videos/clip.mp4
```

When the source codecs already fit the target container, convkit remuxes
instead of re-encoding — lossless, and measured 3.3× faster on a 2-second
clip up to 71.7× on a 60-second 1080p one. On a real terminal `OK`/`FAIL`
render as green/red `✓`/`✗`; piped or redirected output (CI, `| tee`) is
plain ASCII with no escape codes.

## Install

Prebuilt binaries cover Windows x64, macOS x64/arm64, and Linux x64/arm64:

```console
# Linux / macOS
$ curl --proto '=https' --tlsv1.2 -LsSf \
    https://github.com/shdwfruit/convkit/releases/latest/download/convkit-installer.sh | sh
```

```powershell
# Windows
> irm https://github.com/shdwfruit/convkit/releases/latest/download/convkit-installer.ps1 | iex
```

```console
# Homebrew (macOS / Linux)
$ brew install shdwfruit/tap/convkit
```

An MSI (`convkit-x86_64-pc-windows-msvc.msi`) is also attached to each
release. The Windows binaries are not code-signed, so SmartScreen warns on
first run.

From source — requires Rust 1.85+ and a C toolchain on every platform (the
HTTPS downloader's `ring` dependency compiles C at build time):

```console
$ git clone https://github.com/shdwfruit/convkit
$ cd convkit
$ cargo install --path crates/conv
```

convkit does no encoding itself — see [Backends](#backends), or just run a
conversion and let `conv` offer to install what's missing.

## Quick start

```console
conv in.mp4 out.gif              # target inferred from output extension
conv in.mp4 .gif                 # same basename, new extension
conv *.heic --to jpg             # batch; globs expanded by conv itself, so this works on Windows too
conv ./photos --to jpg -o ./out  # folder input, non-recursive, outputs redirected
conv a.png b.png out.pdf         # merge two or more images into one PDF
```

A single conversion reports size, elapsed time, and the absolute path the
result landed at:

```console
$ conv clip.mp4 clip.gif
OK clip.gif - 492 KB - 0.2s
  /home/rick/Videos/clip.gif
  note  The whole filtered stream is buffered in memory for palette generation, so very long inputs are slow and memory-hungry rather than being silently truncated.
```

`--dry-run` prints the exact backend command instead of running it — here,
the per-clip palette generation behind convkit's GIF default:

```console
$ conv clip.mp4 clip.gif --dry-run
ffmpeg -i clip.mp4 -vf 'fps=15,scale=w=min(640\,iw):h=-2:flags=lanczos,split[a][b];[a]palettegen=stats_mode=diff[p];[b][p]paletteuse=dither=bayer:bayer_scale=3' -loop 0 -y clip.gif
note: The whole filtered stream is buffered in memory for palette generation, so very long inputs are slow and memory-hungry rather than being silently truncated.
```

Multi-step recipes are automatic: `md → pdf` needs no LaTeX — pandoc renders
to `.docx`, then LibreOffice renders that to PDF, behind one command:

```console
$ conv sample.md sample.pdf --dry-run
pandoc sample.md --standalone --resource-path . -o sample.convkit-step0.docx
soffice "-env:UserInstallation=<per-run temp profile>" --headless --norestore --convert-to pdf --outdir . sample.convkit-step0.docx
```

For the image → PDF merge: three or more positionals, where every one but
the last is an image and the last is `.pdf`, become one multi-page PDF — one
page per input, in argument order; formats can mix. A directory there
expands to its images in natural order (`p2` before `p10`). Two positionals
are always an ordinary pair: `conv a.png out.pdf` converts one file.

A failure never looks like a success, and carries its own fix — `try` names
whichever package manager is on `PATH` (winget/scoop/choco, brew,
apt-get/dnf/pacman):

```console
$ conv report.xlsx report.pdf
FAIL report.xlsx -> pdf
  soffice not found
  try  brew install --cask libreoffice
```

Exit codes, for scripting: `0` success, `1` conversion failed, `2` usage
error or unsupported pair, `3` a required backend is missing, `4` a batch
partly failed.

## Tuning knobs

Three flags override named defaults on image conversions:

- `--resize <GEOMETRY>` — fit within the geometry, aspect preserved:
  `1600x900`, `1600x` (width only), `x900` (height only), or `50%`.
- `--quality <1-100>` — lossy image targets (jpg/webp/avif) and image → pdf;
  the default is 92.
- `--colors <2-256>` — palette reduction on raster targets.

A flag that doesn't apply to the requested pair is refused with the reason,
never silently ignored:

```console
$ conv in.jpg out.png --quality 80
FAIL in.jpg -> png
  png is lossless; --quality applies to jpg/webp/avif targets and image -> pdf
```

Video/GIF knobs (fps, CRF) are not implemented yet. `conv capabilities
<format>` lists which flags apply to which pair.

## Discovering what it can do

`conv capabilities` lists every registered pair and the backend(s) behind it:

```console
$ conv capabilities
Video:
  mp4    -> mov    ffmpeg
  mp4    -> mkv    ffmpeg
  mp4    -> webm   ffmpeg
  ...
Document:
  docx   -> pdf     soffice
  md     -> html    pandoc
  ...
```

`conv capabilities <format>` shows one format's view — what converts to and
from it, its baked-in defaults, and which tuning flags apply per target:

```console
$ conv capabilities jpg
jpg (Image)

  as source, converts to:
    jpg -> png      [--resize --colors]
    jpg -> webp     [--resize --quality --colors]
    ...
  as target, accepts: heic heif png webp avif tiff bmp svg
  tuning flags when writing jpg: --resize --quality --colors

  defaults: quality 92 (override with --quality)
```

`conv scan` turns the same question around: instead of what convkit can do
in general, what can it do with what is in front of you right now? It lists
the files in a directory (the current one by default) and what each could
become:

```console
$ conv scan
README       --
already.jpg  Image   -> png webp avif tiff bmp pdf
archive.zip  --
clip.mp4     Video   -> mov mkv webm mp3 m4a wav flac gif
notes.md     Doc     -> pdf docx html
photo.heic   Image   -> jpg png webp avif tiff bmp pdf
```

It is a pure lookup on the file extension: nothing is opened, decoded or
probed, and no backend runs, so it stays instant on a large directory. That
also means it reports what convkit *supports*, not what this machine can
currently run — for that, see `conv doctor`.

Files convkit does not recognise are listed with `--` rather than hidden, so
an unconvertible file is never a silent omission. Only regular files are
listed: a directory is read one level deep, and subdirectories inside it are
neither descended into nor shown. A path that does not exist is reported as
an error rather than described, and exits 2.

Common extension aliases resolve to the same format, so `photo.jpeg`,
`photo.jpe` and `photo.jfif` are all JPEGs, `scan.tif` is a TIFF, and
`page.htm` is HTML. A few of those are read-only: ImageMagick has no JFIF
coder, so convkit reads `.jfif` but writes JPEGs as `.jpg`, and asking for a
`.jfif` output says so rather than writing a file whose bytes do not match
its name.

`--dry-run` prints the real backend command without running anything — it
never probes inputs or creates directories. `-v/--verbose` streams each
spawned command and the backend's full output to stderr as a job runs.

## Batch conversion

`--to <format>` converts many inputs at once — a glob, a list of files, or a
folder (non-recursive). `-o/--outdir` redirects outputs; `-j/--jobs` sets
parallelism (default: core count). A batch prints one summary line; per-job
failures still print in full, on stderr:

```console
$ conv ./photos --to jpg -o ./out
OK 2 converted - 0 skipped - 0 failed - 0.1s
  /home/rick/out
```

Existing outputs are never overwritten by default — a collision skips that
one file and reports it; `-y/--overwrite` opts in. `-q/--quiet` silences
success output but never a failure or a backend warning.

## Backends

convkit dispatches to six binaries, each invoked as a subprocess:

| Backend | Used for | `conv install` | Pinned version |
|---|---|---|---|
| `ffmpeg` / `ffprobe` | video, audio, GIF, remux | Yes | 9.0.1 |
| `magick` (ImageMagick) | images, HEIC/HEIF read, images → PDF | No — use your package manager | — |
| `pandoc` | Markdown → HTML/DOCX; parses docx/odt for the PDF fallback | Yes | 3.11 |
| `typst` | PDF engine for the docx/odt → pdf fallback | Yes | 0.15.1 |
| `soffice` (LibreOffice) | Office documents ⇄ PDF | No — manual install, always | — |

Managed versions are pinned per convkit build
(`crates/convkit-core/src/manifest.rs`), checksum-verified, and cover all
five prebuilt targets. On Windows x64, ffmpeg and ffprobe ship in one
upstream zip, so `conv install ffmpeg` provisions both; elsewhere they are
two separate downloads that land at the same pinned version.

When LibreOffice is missing, `docx → pdf` and `odt → pdf` fall back to
pandoc + Typst — lower fidelity, and the result says so in a warning. That
fallback exists only for those two pairs: `xlsx`/`pptx` → pdf and the second
step of `md → pdf` need LibreOffice specifically. Markdown is one-directional
— `md → html` and `md → docx` exist; there is no `html`/`docx` → `md`.

### Resolution order

Each backend resolves in this order, first match wins:

1. `--<backend>-path` (e.g. `--ffmpeg-path /opt/ffmpeg`) — one such flag per
   backend
2. `CONVKIT_<BACKEND>` (e.g. `CONVKIT_FFMPEG=/opt/ffmpeg`) — the executable
   name, uppercased
3. The managed directory `conv install` writes to
   (`%LOCALAPPDATA%\convkit\bin` on Windows, `$XDG_DATA_HOME/convkit/bin` or
   `~/.local/share/convkit/bin` elsewhere)
4. `PATH`
5. Well-known install locations (LibreOffice's default Windows/macOS paths)

An override or env var naming a path that doesn't exist is a hard error
naming the flag and the bad path — it never silently falls through to the
next candidate.

### doctor and install

`conv doctor` reports what's installed, where it resolved from, and how to
fix what isn't:

```console
$ conv doctor
ffmpeg    9.0.1      /opt/homebrew/bin/ffmpeg     (PATH)
ffprobe   9.0.1      /opt/homebrew/bin/ffprobe    (PATH)
magick    7.1.2-30   /opt/homebrew/bin/magick     (PATH)
pandoc    3.10.2     /opt/homebrew/bin/pandoc     (PATH)
soffice   missing    manual install only  |  brew install --cask libreoffice
typst     0.15.1     /opt/homebrew/bin/typst      (PATH)
```

`conv install <backend>` downloads and checksum-verifies one managed
backend. A conversion that fails on a missing managed backend offers to
install it and retry (interactive sessions only, never under `--json`/
`--quiet`); `--yes` pre-answers the prompt for scripts, `--no-install`
always fails with the structured `backend_missing` error instead.

## The update story

`conv update` brings managed backends to the versions this build of convkit
pins — "up to date" means matching the pin, never "newest upstream."
`conv update --check` reports state without changing anything and exits
non-zero only when a managed copy is outdated or an update failed.

Copies that resolve from `PATH`, an env var, or an override are reported as
`external` and never touched, downgraded, or counted against `--check`, no
matter how far they sit from the pin. `magick` and `soffice` are reported
with the package-manager command that would update them, but `conv update`
never runs a package manager. It never replaces `conv` itself either — it
prints the command that would, based on how `conv` was installed. Installing
a newer convkit is what advances the pinned backend versions.

## Machine-readable output

`--json` works on every command and emits exactly one JSON document: an
`"ok"` boolean plus one command-specific plural key (`results`, `plans`,
`backends`, `pairs`, `files`). A conversion writes every job — success or failure —
into a single `"results"` array on stdout, so a half-failed batch is still
one parseable document; the exit code signals pass/fail. A success element:

```json
{
  "ok": true, "input": "clip.mp4", "output": "clip.gif",
  "bytes": 504183, "elapsed_ms": 168, "remuxed": false,
  "backends": [{"backend": "ffmpeg", "version": "9.0.1"}],
  "warnings": ["The whole filtered stream is buffered in memory ..."],
  "notes": [],
  "backend_output": [{"backend": "ffmpeg", "stderr": "ffmpeg version 9.0.1 ..."}]
}
```

A failure carries a structured `error` with a stable `code` (e.g.
`backend_missing`), a message, and remediation commands. `backend_output`
always holds each step's raw output, tail-capped at 16 KiB; `notes` is the
small distilled subset worth a person's attention, usually empty on a clean
run. For the full shapes of every command, run it with `--json` — the binary
is the reference.

## Troubleshooting

**Debian/Ubuntu: HEIC fails with `Unsupported codec`.** `apt-get install
imagemagick` ships without an HEVC decoder there; install
`libheif-plugin-libde265` alongside it. Homebrew and Windows ImageMagick
builds include it already.

**Windows: SmartScreen warns on first run.** The binaries aren't code-signed
yet — see [Install](#install).

**Scripting around a missing backend.** Every `backend_missing` error's
remediation names both the `conv install` command (when one exists) and the
manual package-manager command, in plain output and in `--json`.

## Design notes

- [`docs/defaults-calibration.md`](docs/defaults-calibration.md) — every
  default, measured, with the exact commands to reproduce each figure.
- [`docs/superpowers/specs/2026-08-29-convkit-design.md`](docs/superpowers/specs/2026-08-29-convkit-design.md) —
  the original design rationale: prior art, why Rust, the non-goals list.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), your
choice. convkit invokes ffmpeg, ImageMagick, LibreOffice, pandoc, and Typst
as subprocesses; it does not link against, embed, or redistribute any of
them, and each keeps its own license. `conv install` downloads official
upstream builds straight from their publishers.
