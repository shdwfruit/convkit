# convkit — Design

**Date:** 2026-08-29
**Status:** Approved, pending implementation plan
**Binary:** `conv` · **Crate/package:** `convkit`

---

## 1. Problem

Converting a file between everyday formats — a video to a GIF, a phone photo to
JPEG, a Word document to PDF — still routes most people through an
advertising-funded website that requires uploading the file. The local tools
that do this well (ffmpeg, ImageMagick, LibreOffice, pandoc) are excellent and
free, but each has a different interface, and the correct invocation for a given
conversion is not discoverable. Nobody memorises a palette-generating ffmpeg
filter chain.

`convkit` is a local, cross-platform CLI that maps a source format and a target
format onto an expert-tuned invocation of the right backend, and runs it
offline.

## 2. Prior art, and why this exists anyway

A survey of the landscape (2026-08-29) produced findings that directly shaped
this design. They are recorded here because they are the reason several tempting
features are cut.

**The niche is genuinely empty.** Searching GitHub for `"yt-dlp" file conversion`
returns zero results. The three nearest tools each fail on a different axis:

| Tool | Stars | Why it does not close the gap |
|---|---|---|
| ConvertX | 18.6k | Dockerised web app with a login screen; AGPL-3.0, so its routing table cannot be reused permissively |
| unoserver | 931 | Real CLI with real adoption, but LibreOffice-only; README states Windows/macOS are "as of yet untested" |
| CloudConvert CLI | — | Universal, MIT, maintained — and **67 npm downloads per month** |

That last figure is the most instructive number in the survey. A working,
universal, CLI-shaped converter has effectively no users because it needs an API
key and uploads your files. **Local operation is the moat, not a nice-to-have.**

**The counterweight.** Searching "universal file converter cli" returns 23
repositories whose highest star count is 4. At least four independent 2025–26
projects built the same clever idea — a multi-hop conversion graph over
ffmpeg/ImageMagick/LibreOffice/pandoc — and each has zero stars. Over the same
period the tools that actually won the file-conversion space were `markitdown`
(176k stars) and `docling` (65k stars): many-formats-in, **one**-format-out.
N-to-1 beat N-to-N by two orders of magnitude.

**Conclusion carried into this design: the router is not the product.** Format
coverage is commodity and has been built repeatedly to no audience. This
project's reason to exist is (a) output quality that beats what a competent
person types unaided, (b) genuinely working offline installation on three
platforms, and (c) being legible to machines, which is a distribution channel
that did not exist when those 23 repositories were written.

### Constraints established by the survey

- Shelling out to GPL binaries does **not** impose GPL on this tool; the FSF's
  own FAQ places subprocess invocation under mere aggregation. Bundling would
  instead create a perpetual source-hosting obligation. This design stays on the
  subprocess side of that line.
- LibreOffice has **no static relocatable binary**. A uniform dependency policy
  across backends is therefore impossible; the resolver must be per-backend.
- Neither `cmd.exe` nor PowerShell expands globs for a native executable
  (verified). Batch mode on Windows is impossible without internal expansion.
- `soffice` returns exit code 0 on failure. Output must be stat-ed.
- pandoc **cannot read PDF** — confirmed in pandoc's source, which maps `.pdf` to
  a deliberate "unknown reader" error. No `pdf → *` route may use pandoc.
- GitHub's `releases/latest` endpoint is rate-limited to 60 requests/hour per IP,
  shared by everyone behind one NAT. Download URLs must be pinned.
- Unsigned macOS arm64 binaries fail with an undiagnosable `Killed: 9` unless
  ad-hoc signed. Gatekeeper quarantine is a separate, lesser issue.

## 3. Non-goals

Explicitly out of scope for v1, each because it is either a solved-and-ignored
problem or a scope multiplier:

- The multi-hop conversion routing graph
- Ebooks (calibre), OCR, ML-based PDF parsers (docling, marker, MinerU)
- Data/tabular formats, archives, geospatial, fonts, 3D models
- Config files, user-defined aliases, a plugin API
- Raw `--backend-args` passthrough to the underlying tool
- Any cloud or API-key-based conversion path
- A GUI

## 4. Product shape

One sentence: *the file conversion command you should have typed, that works
offline and tells machines what it can do.*

Three pillars, in priority order:

**4.1 Defaults are the product.** Not coverage. Every supported pair carries a
hand-tuned invocation that measurably beats a competent 30-second attempt, and a
test proving it.

**4.2 Offline, always.** The network is touched only by an explicit
`conv install`. Files never leave the machine. This is the moat.

**4.3 Legible to machines.** `--json` on every command, `--dry-run` printing the
exact backend command, structured errors carrying their own remediation, and
`conv capabilities` emitting the whole conversion table so nothing must be
guessed.

Pillar 4.3 has a useful side effect: `--dry-run` *is* the quality proof. It
prints the expert command, so the tool teaches what it is doing. This is the
README's opening example.

## 5. Architecture

### 5.1 Language: Rust

Chosen over Go. Go's advantage is real — `GOOS`/`GOARCH` cross-compiles every
target from one runner, and its contributor pool is broader — but three factors
outweigh it:

- `cargo-dist` solves the release matrix *and* generates the shell installer,
  PowerShell installer, Homebrew formula, and MSI. That attacks the hardest
  problem in the project directly.
- The `wild` crate solves the verified Windows globbing blocker by parsing the
  raw command line. In Go this is hand-rolled.
- crates.io is a distribution channel with no Go equivalent (`go install`
  requires Go on the user's machine).

`clap`, `indicatif`, and `rayon` are best-in-class for this shape of tool.
Runtime startup cost is irrelevant next to a backend process.

### 5.2 Workspace layout

```
convkit/
├─ crates/
│  ├─ convkit-core/          # library. Never prints. Returns typed results.
│  │  ├─ format.rs           # extension → Format, plus content sniffing
│  │  ├─ registry.rs         # the conversion table: (Format, Format) → Recipe
│  │  ├─ recipe.rs           # backend + argv template + pre/post validation
│  │  ├─ backend.rs          # trait: probe(), version(), install_hint()
│  │  ├─ resolve.rs          # per-backend discovery & managed installs
│  │  └─ exec.rs             # subprocess, temp+atomic rename, progress events
│  └─ conv/                  # binary: arg parsing, human vs --json rendering, batch
└─ docs/
```

**The one invariant that makes everything else cheap:** nothing in
`convkit-core` writes to stdout or stderr. It returns typed values and emits
progress events. This is why `--json` is free rather than a retrofit, and why
the planned `conv mcp` frontend and OS integration become thin consumers of the
same crate instead of forks.

### 5.3 Conversions are data

A pair maps to a `Recipe`: which backend, an argv template, pre-checks, and
post-validation. The registry is a static table. Contributing a better
`mp4 → gif` therefore means editing one table entry and adding one test — no
code — and every quality claim stays reviewable in a single file.

### 5.4 Dependency resolution — per backend

| Backend | Policy | Reason |
|---|---|---|
| `ffmpeg` | Managed install offered | Static relocatable builds exist |
| `magick` | Managed install offered | Portable builds exist |
| `pandoc` | Managed install offered | ~30MB standalone official releases |
| `soffice` | **Detect only, never managed** | No relocatable binary exists |

Resolution order per backend:

1. Explicit flag (`--ffmpeg-path`)
2. Environment variable (`CONVKIT_FFMPEG`)
3. Managed directory (`%LOCALAPPDATA%\convkit\bin`, `~/.local/share/convkit/bin`)
4. `PATH`
5. Known platform install locations

Rules:

- **Never invoke winget/brew/apt on the user's behalf.** It needs elevation,
  fails in non-interactive sessions, and cannot refresh the PATH of the shell
  that is already running. `conv doctor` prints the command; the user runs it.
- **Managed installs use a pinned asset URL and a pinned SHA-256.** Never
  `releases/latest`.
- **On macOS arm64, ad-hoc sign after download** (`codesign --force --sign -`).

### 5.5 Execution

- Write to a temporary file in the destination directory, then atomically
  rename. Ctrl-C must never leave a truncated file that appears valid.
- Always `stat` the produced output and treat a missing or zero-byte result as
  failure, regardless of the backend's exit code.
- Pass `-env:UserInstallation=<temp profile>` to every `soffice` invocation so
  concurrent runs do not collide over a shared user profile.

### 5.6 Batch

`rayon`, bounded to the core count by default and overridable with `-j`.
`indicatif` for progress. Continue on error; collect failures into a summary
table (or a JSON array under `--json`) and exit non-zero.

## 6. The v1 conversion table

| Family | Pairs | Backend |
|---|---|---|
| Video | `mp4·mov·mkv·webm·avi` → `mp4·webm` | ffmpeg |
| Audio | any audio or video input → `mp3·m4a·wav·flac` | ffmpeg |
| GIF | `mp4·mov·webm` → `gif`; `gif` → `mp4` | ffmpeg |
| Photos | `heic·heif` → `jpg·png` | magick |
| Web images | `png·jpg·webp·avif·tiff·bmp`, all directions | magick |
| Vector | `svg` → `png·jpg` | magick |
| Images → PDF | one or many images → `pdf` | magick |
| Office → PDF | `docx·xlsx·pptx·odt·ods` → `pdf` | soffice |
| PDF → editable | `pdf` → `docx` | soffice |
| Markdown | `md` → `docx·html` | pandoc |
| Markdown → PDF | `md` → `pdf` | pandoc → soffice |

Approximately 40 pairs from 4 backends.

**On `md → pdf`:** the conventional route requires a LaTeX toolchain (~400MB).
Instead this is a single recipe with two steps — pandoc emits `.docx`, soffice
renders it to PDF. This is a hardcoded two-step recipe, **not** the multi-hop
routing graph, which remains cut. No other multi-step recipes ship in v1.

Unknown or unsupported extensions hard-error with a "did you mean" suggestion.
Never silently default to a format.

## 7. The six defaults that constitute the product

Each has a corresponding test.

1. **Auto-remux.** When only the container changes and the codecs are already
   compatible, use `-c copy`. Lossless, and a stream copy instead of a
   re-encode — measured at 3.3–26.2× faster on clips from 2 to 30 seconds and
   up to 72.1× on a 1080p/60s clip, growing with clip length and resolution
   rather than sitting at a fixed multiplier. See
   `docs/defaults-calibration.md` §1 for the full measurement (5–7 timed runs
   per clip size, argv and raw samples included). Probe first; transcode only
   when forced.
2. **GIF via generated palette.** `palettegen`/`paletteuse` with
   `stats_mode=diff`, 15fps, width capped at 640. Naive
   `ffmpeg -i in.mp4 out.gif` uses a fixed 256-colour web palette.
3. **HEIC handled correctly.** Preserve the ICC colour profile; honour EXIF
   orientation. Failing either is why converted phone photos come out washed out
   or rotated.
4. **Quality anchors.** CRF 20 with AAC 160k for video; quality 92 for JPEG.
   Visually transparent without bloat.
5. **PDF → DOCX honesty.** Perform the conversion, and attach a `fidelity`
   warning to the result stating that PDF carries no paragraph structure to
   recover, so the output is positioned text boxes rather than a flowing
   document. Per the 5.2 invariant this is a value returned by core, rendered
   once as a note by the CLI frontend and carried as a `warnings` array under
   `--json`.
6. **Atomic output.** Temp file plus rename, plus stat-ing the result. See 5.5.

## 8. CLI surface

```
conv in.mp4 out.gif              # primary: target inferred from output extension
conv in.mp4 .gif                 # same basename, new extension
conv *.heic --to jpg             # batch; glob expanded internally
conv ./photos --to jpg -o ./out  # folder input
conv doctor                      # what is installed, what is missing, how to fix
conv install ffmpeg              # explicit, opt-in, pinned URL + SHA-256
conv capabilities                # the full conversion table
```

Globs are expanded **inside the binary** via `wild`, parsing the raw command
line — mandatory for Windows batch support.

**Global flags:** `--json`, `--dry-run`, `-o/--outdir`, `-j/--jobs`,
`-y/--overwrite`, `-q/--quiet`, and one `--<backend>-path` override per backend
(`--ffmpeg-path`, `--magick-path`, `--pandoc-path`, `--soffice-path`) as
required by the 5.4 resolution order.

**Batch semantics.** Folder input is **non-recursive** in v1; `--recursive`
is deferred. Without `-o`, each output is written alongside its input with the
same basename and the new extension. With `-o`, the flat set of outputs is
written into that directory, and a basename collision between two inputs is an
error rather than a silent overwrite.

**Overwrite policy:** refuse by default. In batch, a collision skips that file
and is reported, rather than aborting the run. Zero-decision defaults do not
extend to destroying data.

## 9. Errors and exit codes

Every failure carries its own remediation, so an agent can self-heal rather than
guess:

```json
{ "ok": false,
  "error": { "code": "backend_missing", "backend": "ffmpeg",
    "message": "ffmpeg not found",
    "remediation": { "managed": "conv install ffmpeg",
                     "manual": "winget install Gyan.FFmpeg" } } }
```

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Conversion failed |
| 2 | Usage error, or unsupported format pair |
| 3 | Required backend missing |
| 4 | Batch completed with some failures |

## 10. Testing

Byte-exact golden files are rejected as an approach: ffmpeg 9.0 and ffmpeg 7.1
produce different bytes for identical input, so such a suite breaks on every
backend upgrade. The suite therefore splits in two.

**10.1 Recipe snapshot tests — the primary suite.** Assert the exact argv
generated for every pair in the registry. These require **no backends
installed**, run in milliseconds on every platform, and test precisely what the
product is: the flags. They are nearly free to write, because the snapshot is
the `--dry-run` output.

**10.2 Output property tests.** Run real conversions against tiny fixtures (a
two-second clip, one HEIC photo), then assert *properties* via `ffprobe` and
`magick identify`: codec is h264, the GIF palette is generated rather than fixed,
the ICC profile survived, dimensions match, file size falls in a sane band. Never
byte equality.

CI runs 10.1 on every push across all three platforms, and 10.2 in a separate job
with backends installed.

## 11. Release and distribution

`cargo-dist` produces the release matrix and generates the shell installer,
PowerShell installer, Homebrew formula, and MSI. Published to crates.io as
`convkit`, installing a binary named `conv`.

**Naming rationale:** `anyconv` is taken by a competing conversion CLI published
to npm in May 2026. `forge`, `cast`, and `anvil` are Foundry binaries and would
collide on a developer's PATH. `convkit` was verified absent from crates.io,
PyPI, npm, and Homebrew (formula and cask). The binary is `conv` following the
ripgrep/`rg` pattern. It must **never** be named `convert`, which is the
destructive Windows FAT→NTFS tool — the same collision that caused ImageMagick v7
to rename its binary to `magick`.

## 12. Roadmap

- **v1** — this document.
- **v1.1** — `conv mcp`, an MCP server frontend over `convkit-core`, published to
  MCP registries. The distribution play.
- **v2** — OS integration: a Windows right-click "Convert to…" menu and a macOS
  Quick Action, both driven by the same binary. Reaches the non-terminal users
  who constitute the actual conversion-website traffic.

## 13. Risks

1. **Adoption.** 23 comparable projects have a maximum of 4 stars. Mitigated by
   competing on quality and machine-legibility rather than coverage, but not
   eliminated.
2. **Backend installation on three platforms** is the single most likely thing to
   sink the project. Four comparable projects retreated to Docker to escape it.
   Mitigated by `conv doctor` plus explicit `conv install`, and by accepting that
   LibreOffice will always be a manual install.
3. **Backend version drift.** Mitigated by property-based rather than byte-exact
   tests, and by recording the probed backend version in `--json` output.
