# convkit

One command for everyday file conversion. Runs entirely on your machine.

```console
$ conv clip.mp4 clip.gif
clip.gif (527 KB)
note: The whole filtered stream is buffered in memory for palette generation, so very long inputs are slow and memory-hungry rather than being silently truncated.
```

That is not `ffmpeg -i clip.mp4 clip.gif`. Here is what it actually ran:

```console
$ conv clip.mp4 clip.gif --dry-run
ffmpeg -i clip.mp4 -vf fps=15,scale=w=min(640\,iw):h=-2:flags=lanczos,split[a][b];[a]palettegen=stats_mode=diff[p];[b][p]paletteuse=dither=bayer:bayer_scale=3 -loop 0 -y clip.gif
note: The whole filtered stream is buffered in memory for palette generation, so very long inputs are slow and memory-hungry rather than being silently truncated.
```

The default `ffmpeg -i clip.mp4 out.gif` path uses a fixed 256-colour web
palette. convkit generates a palette from the actual frames instead
(`palettegen`/`paletteuse` with `stats_mode=diff`, dithered), which is a
**quality** win, not a size one: on the fixture above the tuned GIF uses 51%
more distinct colours on the first frame (231 vs. 153) and 46% more across
the whole clip than the naive command — but it is also 16.5% *larger*, despite
encoding half as many frames (the 15fps cap doing its job). A richer palette
and ordered dithering cost bytes; nothing about this recipe makes files
smaller. Both figures, and the exact commands used to reproduce them, are in
[`docs/defaults-calibration.md`](docs/defaults-calibration.md) §2 — that file
is where every quantitative claim on this page comes from, and it says so
when a claim *isn't* confirmed, too.

The other flagship default is auto-remux: converting `mkv → mp4` (or any
compatible container pair) does a stream copy instead of a re-encode when
the source codecs already fit the target container, which is lossless *and*
faster — measured at 3.3× on a 2-second, 640×360 clip, 26.2× at 30 seconds/
720p, and 71.7× at 60 seconds/1080p (5–7 timed runs per size, argv and raw
samples included). The multiplier isn't a fixed constant; it climbs with
clip length and resolution, because transcode cost scales with total pixels
and a stream copy's cost doesn't. See
[`docs/defaults-calibration.md`](docs/defaults-calibration.md) §1 for the
full measurement.

## Why this exists

Searching "universal file converter cli" turns up 23 repositories whose
highest star count is 4. At least four independent projects built the same
idea — a router over ffmpeg/ImageMagick/LibreOffice/pandoc — and none of them
found an audience. Over the same period, the tools that actually won the
file-conversion space (`markitdown`, `docling`) went the other direction:
many formats in, **one** format out.

The conclusion this project is built on: **the router is not the product.**
Format coverage is commodity and has been built repeatedly to no audience.
What's actually missing is (a) output quality that beats what a competent
person types unaided, (b) offline installation that genuinely works, and (c)
being legible to machines — `--json`, `--dry-run`, structured errors with
their own remediation — none of which those 23 repositories offer. A working,
universal, MIT-licensed CLI converter with an API instead of local execution
(CloudConvert's) gets, per its own npm download count, about 67 downloads a
month. Local operation is the moat here, not a nice-to-have.

`--dry-run` is the actual product demo: it prints the expert-tuned command
instead of running it, so the tool teaches you what it's doing rather than
hiding it behind a black box.

## Install

No release has been tagged yet, so the prebuilt-binary path below isn't live
yet — build from source (further down) in the meantime. The release pipeline
(`cargo-dist`, configured in `dist-workspace.toml`) is wired up so that once a
version tag is pushed, each GitHub release carries prebuilt archives for five
targets — `x86_64-pc-windows-msvc`, `x86_64-apple-darwin`,
`aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`, and
`aarch64-unknown-linux-gnu` (no Windows arm64 build yet) — plus:

```console
# Linux / macOS, once a release exists
$ curl --proto '=https' --tlsv1.2 -LsSf \
    https://github.com/shdwfruit/convkit/releases/latest/download/convkit-installer.sh | sh
```

```powershell
# Windows, once a release exists
> irm https://github.com/shdwfruit/convkit/releases/latest/download/convkit-installer.ps1 | iex
```

An MSI (`convkit-x86_64-pc-windows-msvc.msi`) and a Homebrew formula
(`convkit.rb`, attached to the release for anyone maintaining a tap) are
generated too. `cargo install convkit` will also work once the crate is
published to crates.io — it installs a binary named `conv`, not `convkit`.

## Usage

```console
conv in.mp4 out.gif              # primary: target inferred from output extension
conv in.mp4 .gif                 # same basename, new extension
conv *.heic --to jpg             # batch; glob expanded internally (works on Windows too — see below)
conv ./photos --to jpg -o ./out  # folder input, non-recursive, outputs redirected
```

Globs are expanded **inside the binary** (via the `wild` crate, parsing the
raw command line) because neither `cmd.exe` nor PowerShell expands globs for
a native executable — without this, batch mode would simply not work on
Windows.

A single conversion's output is deliberately compact — success, size, elapsed
time, and always the absolute path the result actually landed at, since that
last part is the one thing an ambiguous output path (`conv video.mp4
out.gif`, no directory in sight) can't answer on its own:

```console
$ conv clip.mp4 clip.gif
✓ clip.gif · 527 KB · 0.2s
  C:\Users\Rick Xie\Videos\clip.gif
  note  long inputs buffer entirely in memory for palette generation
```

A lossless remux says so, since that's the good outcome worth calling out:

```console
$ conv clip.mkv clip.mp4
✓ clip.mp4 · 4.2 MB · 0.1s · stream copy, no re-encode
  C:\Users\Rick Xie\Videos\clip.mp4
```

A batch gets one summary line instead of a wall of per-job success lines —
per-job *failures* still print in full, on stderr:

```console
$ conv ./photos --to jpg -o ./out
✓ 12 converted · 1 skipped · 0 failed · 8.3s
  C:\Users\Rick Xie\Photos\out
```

And a failure never looks like a success:

```console
$ conv report.pptx report.pdf
✗ report.pptx → pdf
  soffice not found
  try  winget install TheDocumentFoundation.LibreOffice
```

Colour and the `✓`/`✗` glyphs appear only on a real terminal; piped or
redirected output (`conv ... | tee log`, a CI job) degrades to plain ASCII
with no escape codes — `OK`/`FAIL` in place of the glyphs, `-` in place of
`·`. `-q/--quiet` silences success output entirely (a batch summary
included) but never a failure. `--json` is unaffected by any of this — see
[Machine-readable output](#machine-readable-output---json) below.

Multi-step recipes are automatic and invisible. `md → pdf`, for instance,
needs no LaTeX toolchain — pandoc renders to `.docx`, then LibreOffice
renders that to PDF, as two backend invocations behind one command:

```console
$ conv sample.md .pdf --dry-run
pandoc sample.md --standalone -o sample.convkit-step0.docx
soffice -env:UserInstallation=<per-run temp profile> --headless --norestore --convert-to pdf --outdir . sample.convkit-step0.docx
```

Other flags: `-o/--outdir`, `-j/--jobs` (batch parallelism, defaults to the
core count), `-y/--overwrite` (refused by default — a batch collision skips
that one file and reports it rather than aborting the run), `-q/--quiet`,
`--json`, `--dry-run`, `--yes`/`--no-install` (see [Installing a missing
backend on the
fly](#installing-a-missing-backend-on-the-fly) below), and one
`--<backend>-path` override per backend.

**`conv doctor`** reports what's installed and how to fix what isn't:

```console
$ conv doctor
ffmpeg    9.0.1-ess… C:\Users\...\AppData\Local\convkit\bin\ffmpeg.exe (MANAGED)
ffprobe   9.0.1-ess… C:\Users\...\AppData\Local\convkit\bin\ffprobe.exe (MANAGED)
magick    missing    manual install only  |  winget install ImageMagick.ImageMagick
pandoc    3.11       C:\Users\...\AppData\Local\convkit\bin\pandoc.exe (MANAGED)
soffice   missing    manual install only  |  winget install TheDocumentFoundation.LibreOffice
```

(`winget`/`scoop`/`choco`/`brew`/`apt`/`dnf`/`pacman` are all detected;
whichever is on `PATH` is what gets suggested.)

**`conv capabilities`** lists every registered conversion pair and which
backend(s) drive it — 107 pairs across 27 formats as of this writing:

```console
$ conv capabilities
Video:
  mp4    -> webm   ffmpeg
  mp4    -> gif    ffmpeg
  mkv    -> mp4    ffmpeg
  ...
Image:
  heic   -> jpg    magick
  png    -> webp   magick
  ...
Document:
  pdf    -> docx   soffice
  md     -> pdf    pandoc, soffice
  ...
```

Exit codes, for scripting: `0` success, `1` conversion failed, `2` usage
error or unsupported pair, `3` a required backend is missing, `4` a batch
partly failed.

## Backends convkit drives

convkit itself does no encoding — it dispatches to four external tools, each
kept at arm's length as a subprocess (see Licensing below):

| Backend | Used for | `conv install`? |
|---|---|---|
| `ffmpeg` / `ffprobe` | video, audio, GIF, remux | **Yes** — `conv install ffmpeg` fetches a pinned, checksummed build of both, for Windows x64, macOS x64, macOS arm64, and Linux x64 (four platform/arch builds; Linux arm64 has no managed build yet even though convkit itself targets it — see Install above) |
| `pandoc` | Markdown ⇄ HTML/DOCX | **Yes** — same mechanism, the same four platform/arch builds |
| `magick` (ImageMagick) | image conversions, HEIC/HEIF read, images → PDF | **No.** ImageMagick's official builds ship a portable Windows binary only as `.7z`, a Linux binary only as an AppImage, and no standalone macOS binary at all — none of those clear the "plain zip/tar.gz, verified" bar `conv install` holds every managed backend to. Install it with your package manager. |
| `soffice` (LibreOffice) | Office documents ⇄ PDF | **No, ever.** LibreOffice has no relocatable binary at all, on any platform. This is a permanent policy, not a gap. |

On Debian/Ubuntu, plain `apt-get install imagemagick` gives you ImageMagick 6
*without* an HEVC decoder, so converting an iPhone HEIC photo fails with
`convert: Unsupported feature: Unsupported codec` even though ImageMagick is
installed and every other conversion works fine — install
`libheif-plugin-libde265` alongside it to fix that. This is a Debian/Ubuntu
packaging decision (HEVC decoding is split into its own plugin package there),
not a convkit limitation — macOS Homebrew and Windows builds of ImageMagick
include HEVC decoding already.

`conv install` never touches the network without being asked, downloads only
from a pinned release tag (never a `releases/latest` alias, which is
rate-limited per-IP and shared by everyone behind one NAT), and verifies a
SHA-256 checksum before anything is written to disk.

### Installing a missing backend on the fly

A conversion that fails because a backend is missing used to mean: read the
error, run `conv install <backend>` yourself, then re-run the original
command. When the backend is one convkit can actually provision, and the
session is interactive, `conv` now offers to close that loop for you:

```console
$ conv clip.mkv clip.mp4
ffmpeg is required for this conversion and isn't installed.
Install it now? (also installs ffprobe) [y/N] y
downloading https://github.com/GyanD/codexffmpeg/releases/download/9.0.1/ffmpeg-9.0.1-essentials_build.zip ...
✓ clip.mp4 · 4.2 MB · 0.1s · stream copy, no re-encode
  C:\Users\Rick Xie\Videos\clip.mp4
```

ffmpeg and ffprobe ship in that one zip upstream, so the prompt says so up
front, and both land from the single download above — not two prompts, not
two fetches. `conv install ffprobe` on a fresh machine does the same thing
in reverse: same manifest entry either way, so whichever of the two names
you ask for, both get installed together.

Answering anything other than `y`/`yes` — or the prompt never appearing at
all — leaves today's behaviour exactly as it was: the structured
`backend_missing` error, unchanged, exit code `3`. The prompt only appears
when *every one* of these hold:

- The backend actually has a managed build for this platform
  (`soffice`/LibreOffice never qualifies — it has no relocatable binary and
  can never be auto-installed, prompt or no prompt — and neither does
  `magick`/ImageMagick, which is `conv install`-eligible in principle but has
  no verified manifest entry on any platform today).
- The session is interactive: stdin and stderr are both real terminals,
  checked properly (not guessed) — piped stdin, a CI runner, and anything
  else non-interactive always gets the plain structured error instead, with
  no hang.
- Neither `--json` nor `--quiet` was passed.

Two flags cover the cases a live prompt can't: `--yes` assumes yes without
ever touching stdin (for a script that wants the install-then-retry
behaviour anyway), and `--no-install` refuses to prompt or install under any
circumstance, always failing with the plain error. Passing both together is
a usage error. This still keeps convkit's offline-by-default promise —
"the network is touched only by an explicit `conv install`" — because a
`y` at this prompt *is* that explicit consent; nothing is ever downloaded
silently.

`convkit-core` itself never prompts or prints anything — this whole flow
lives in the `conv` binary, which catches the structured error, asks the
question, and retries. `--json` output is completely unaffected: a
missing-backend conversion under `--json` reports exactly the same envelope
it always has (see below), and exits `3`, with no prompt ever offered.

## Machine-readable output (`--json`)

Every command accepts `--json` and always writes exactly one JSON document,
and that document always has the same shape: an `"ok"` field, plus one
command-specific plural key — `"results"` for a conversion, `"plans"` for
`--dry-run`, `"backends"` for `doctor`, `"pairs"` for `capabilities` — even
when there's only one item in it. Older versions of this tool had four
different shapes here (a bare, `ok`-less array for a real conversion; a
singular `"plan"` key for a one-job `--dry-run` but a plural `"plans"` for a
multi-job one); a consumer no longer has to branch on job count or command
to find the data.

For `doctor`, `capabilities`, and `install`, the document goes to stdout on
success and to stderr on failure. For a conversion it's slightly different
by design: every job in a batch — success *or* failure — is reported inside
the one `"results"` array on stdout, so a batch that's half missing a
backend still gets one document a script can parse start to finish; only an
error caught before any job exists at all (an unrecognised `--to`, a
malformed invocation) is its own top-level document on stderr instead.
Either way, the exit code is what tells you pass/fail, never which stream
the JSON landed on.

A real conversion (`ok: true`, one `results` element):

```console
$ conv clip.mp4 clip.gif --json
{
  "ok": true,
  "results": [
    {
      "backends": [
        {
          "backend": "ffmpeg",
          "version": "9.0.1-essentials_build-www.gyan.dev"
        }
      ],
      "bytes": 539921,
      "elapsed_ms": 202,
      "input": "clip.mp4",
      "ok": true,
      "output": "clip.gif",
      "remuxed": false,
      "warnings": [
        "The whole filtered stream is buffered in memory for palette generation, so very long inputs are slow and memory-hungry rather than being silently truncated."
      ]
    }
  ]
}
```

`elapsed_ms` is the only field this tool's human-readable output redesign added to the JSON contract — purely additive, and always present on a successful job; the rest of the shape (`ok` plus a plural key, on every document, every command) is unchanged.

And a real one against a machine with no ffmpeg anywhere on `PATH`, no
managed install, and no package manager detected either (exit code 3) — a
missing backend carries its own fix, so a script or an agent can self-heal
instead of guessing:

```console
$ conv clip.mp4 clip.gif --json
{
  "ok": false,
  "results": [
    {
      "error": {
        "backend": "ffmpeg",
        "code": "backend_missing",
        "message": "ffmpeg not found",
        "remediation": {
          "managed": "conv install ffmpeg",
          "manual": "install ffmpeg from https://ffmpeg.org/download.html"
        }
      },
      "input": "clip.mp4",
      "ok": false,
      "output": "clip.gif"
    }
  ]
}
```

`remediation.manual` is the exact package-manager command when one is
detected on `PATH` (as in the `doctor` table above — `winget install
ImageMagick.ImageMagick`, `brew install ffmpeg`, and so on across seven
supported package managers), falling back to the tool's official download
page, as above, only when none is.

`--dry-run --json` emits the same plan(s) shown by plain `--dry-run`, always
under a `"plans"` array (`{"ok": ..., "dry_run": true, "plans": [...]}`),
structured instead of rendered as shell text; `conv doctor --json` and `conv
capabilities --json` mirror their human output the same way.

## Licensing

convkit itself is dual-licensed under [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE), your choice — the conventional Rust default.

convkit **invokes** ffmpeg, ImageMagick, LibreOffice, and pandoc as
subprocesses; it does not link against, embed, or redistribute any of them.
Each backend keeps its own licence: ffmpeg is LGPL-2.1+ by default and
GPL-2+ once built with a GPL-licensed component such as libx264 (common in
prebuilt binaries, including the ones `conv install` fetches — treat a given
ffmpeg build as GPL unless you've checked its own `-version`/license banner
says otherwise); ImageMagick uses its own permissive, Apache-style
"ImageMagick License"; LibreOffice is MPL-2.0 (with some LGPLv3+-licensed
code); pandoc is GPL-2.0-or-later. None of that attaches to convkit itself:
the FSF's own GPL FAQ places invoking a separate program at arm's length
under "mere aggregation," not a combined or derivative work, and shelling
out to a GPL binary no more makes convkit GPL than a shell script calling
`grep` would. convkit bundles none of these tools' source or binaries and
ships none of them — `conv install` downloads official upstream builds
straight from their publishers, which is also *why* it works this way rather
than convkit vendoring its own compiled copies. See
`docs/superpowers/specs/2026-08-29-convkit-design.md` §2 for the fuller
reasoning this constraint was designed around.

## Building from source & contributing

```console
$ git clone https://github.com/shdwfruit/convkit
$ cd convkit
$ cargo build --workspace
```

Requires Rust 1.85+ and, less obviously, **a working C toolchain on every
platform, including Windows** — `convkit-core` depends on `ureq` for `conv
install`'s HTTPS downloads, which pulls in `rustls` → `ring`, and `ring`
compiles C/assembly at build time regardless of target OS. If `cargo build`
fails inside `ring`'s build script, install one:

- **Linux:** `build-essential` (Debian/Ubuntu) or your distro's equivalent (`gcc`, `make`)
- **macOS:** Xcode Command Line Tools — `xcode-select --install`
- **Windows:** the "Desktop development with C++" workload from the [Visual
  Studio Build Tools](https://visualstudio.microsoft.com/downloads/#build-tools-for-visual-studio),
  or a `mingw-w64` toolchain if targeting `*-gnu`

Before sending a change, the standing gates (also what CI runs, with **no
backends installed** for the default suite — that's the whole point, see
`.github/workflows/ci.yml`):

```console
$ cargo fmt --all -- --check
$ cargo clippy --workspace --all-targets -- -D warnings
$ cargo test --workspace
```

That's 133 tests plus 4 more gated behind `#[ignore]` (real conversions
against real backends — run them with `cargo test --workspace -- --ignored`
once ffmpeg/ImageMagick/LibreOffice/pandoc are on `PATH`; CI's `integration`
job runs all four of these against real backends on Ubuntu, unconditionally).
One of the four, `heic_to_jpg_preserves_orientation_and_stays_reasonably_sized`,
runs against a real iPhone HEIC photo committed at
`tests/fixtures/photo.heic` (4032x3024, 1.58 MB — see
`docs/defaults-calibration.md` for why it's that large and what EXIF it
does and doesn't carry).

`convkit-core` (the library crate: planning, resolution, execution) never
writes to stdout or stderr itself — all user-facing output belongs to the
`conv` binary crate. CI's `test` job enforces this directly: a "core must
not print" step greps `crates/convkit-core/src` for stray `println!`/
`eprintln!`/`print!`/`eprint!`/`dbg!` and fails the build if it finds any
(see `.github/workflows/ci.yml`).
