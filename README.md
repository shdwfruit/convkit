# convkit

One command for everyday file conversion. Runs entirely on your machine —
files never leave it, and only `conv install`/`conv update` ever touch the
network.

```console
$ conv clip.mp4 clip.gif
OK clip.gif - 492 KB - 0.2s
  /home/rick/Videos/clip.gif
  note  The whole filtered stream is buffered in memory for palette generation, so very long inputs are slow and memory-hungry rather than being silently truncated.
```

(Piped or redirected output — a CI job, `| tee log` — always looks like this,
plain ASCII with no escape codes. On a real terminal `OK`/`FAIL` become
green/red `✓`/`✗`, and ` · ` replaces ` - `; see [Output shapes](#output-shapes).)

That's not `ffmpeg -i clip.mp4 clip.gif`. `--dry-run` shows exactly what ran
instead:

```console
$ conv clip.mp4 clip.gif --dry-run
ffmpeg -i clip.mp4 -vf 'fps=15,scale=w=min(640\,iw):h=-2:flags=lanczos,split[a][b];[a]palettegen=stats_mode=diff[p];[b][p]paletteuse=dither=bayer:bayer_scale=3' -loop 0 -y clip.gif
note: The whole filtered stream is buffered in memory for palette generation, so very long inputs are slow and memory-hungry rather than being silently truncated.
```

The naive `ffmpeg -i in.mp4 out.gif` uses a fixed 256-colour web palette;
convkit generates one from the actual frames (`palettegen`/`paletteuse`,
`stats_mode=diff`, dithered) — a **quality** win, not a size one: +51% more
distinct colours on frame 0 and +46% across the whole clip on the fixture
measured in [`docs/defaults-calibration.md`](docs/defaults-calibration.md) §2,
but 16.5% *larger*, despite encoding half as many frames. Every quantitative
claim in this README traces back to that file, including the one flagship
number that turned out **not** to hold up under measurement (see below).

The other flagship default is auto-remux: `mkv → mp4` (or any compatible
container pair) does a stream copy instead of a re-encode when the source
codecs already fit the target — lossless *and* faster, measured at 3.3× on a
2-second clip up to 71.7× on a 60-second 1080p one (`docs/defaults-calibration.md`
§1). The multiplier grows with clip length and resolution because transcode
cost scales with total pixels and a stream copy's doesn't — there is no fixed
"100x", despite what an earlier version of this project's design doc claimed
before anyone measured it.

The original design rationale (why this exists, prior art, why Rust) lives in
[`docs/superpowers/specs/2026-08-29-convkit-design.md`](docs/superpowers/specs/2026-08-29-convkit-design.md) —
historical at this point (see its own status note for where it's since
diverged from the code), but still the fuller version of the "why."

## What it does

convkit maps a source format and a target format onto an expert-tuned
invocation of the right backend — ffmpeg, ImageMagick, LibreOffice, pandoc, or
Typst — and runs it locally. 115 conversion pairs across 27 formats as of this
writing; `conv capabilities` prints the current, exact list, and is the source
of truth for that number, not this README.

Three things it's built around:

- **Defaults that beat a competent 30-second attempt.** The GIF palette and
  auto-remux above are two of six; see `docs/defaults-calibration.md` for all
  of them, measured, with the exact commands to reproduce each figure.
- **Offline, always.** No network access except an explicit `conv
  install`/`conv update`.
- **Legible to machines.** `--json` on every command, `--dry-run` that prints
  the real backend command instead of running it, structured errors that
  carry their own remediation.

## Install

### Prebuilt binaries

```console
# Linux / macOS
$ curl --proto '=https' --tlsv1.2 -LsSf \
    https://github.com/shdwfruit/convkit/releases/latest/download/convkit-installer.sh | sh
```

```powershell
# Windows
> irm https://github.com/shdwfruit/convkit/releases/latest/download/convkit-installer.ps1 | iex
```

These fetch the *latest convkit release* — that's what `releases/latest`
means, and it's the right thing for the installer script itself. It's a
separate question from what's inside that release: every convkit build ships
its own manifest pinning exact backend versions (see [Backends](#backends)),
so "latest convkit" and "latest ffmpeg" are not the same promise — installing
a newer convkit is what advances the backend versions `conv install`/`conv
update` will fetch, not the other way around.

An MSI (`convkit-x86_64-pc-windows-msvc.msi`) and a Homebrew formula
(`convkit.rb`) are also attached to each release. **The Windows binaries
aren't code-signed**, so SmartScreen will show "Windows protected your PC" on
first run — there's no publisher certificate behind this project (yet).

`cargo install convkit` will work once the crate is published to crates.io.
No release has been tagged yet, so none of the above is live — build from
source below in the meantime.

Prebuilt coverage, once it exists: Windows x64, macOS x64/arm64, Linux
x64/arm64 (five targets; no Windows arm64 build).

### Build from source

```console
$ git clone https://github.com/shdwfruit/convkit
$ cd convkit
$ cargo build --workspace
```

Requires Rust 1.85+ and, less obviously, a C toolchain on every platform,
including Windows — `convkit-core` depends on `ureq` for `conv install`'s
HTTPS downloads, which pulls in `rustls` → `ring`, and `ring` compiles
C/assembly at build time regardless of target OS. If `cargo build` fails
inside `ring`'s build script, install one:

- **Linux:** `build-essential` (Debian/Ubuntu) or your distro's equivalent
- **macOS:** Xcode Command Line Tools — `xcode-select --install`
- **Windows:** the "Desktop development with C++" workload from [Visual
  Studio Build Tools](https://visualstudio.microsoft.com/downloads/#build-tools-for-visual-studio),
  or a `mingw-w64` toolchain if targeting `*-gnu`

## Usage

```console
conv in.mp4 out.gif              # target inferred from output extension
conv in.mp4 .gif                 # same basename, new extension
conv *.heic --to jpg             # batch; glob expanded internally, works on Windows too
conv ./photos --to jpg -o ./out  # folder input, non-recursive, outputs redirected
conv a.png b.png out.pdf         # merge two or more images into one PDF
```

Globs are expanded **inside the binary** (the `wild` crate parses the raw
command line), because neither `cmd.exe` nor PowerShell expands globs for a
native executable — without this, batch mode would simply not work on
Windows.

### Multi-image → PDF

Three or more positionals, where every one but the last is an image and the
last is `.pdf`, merge into one multi-page PDF — one page per input, in
argument order, and formats can mix (`conv a.png b.jpg out.pdf` is fine):

```console
$ conv a.png b.png out.pdf
OK out.pdf - 4 KB - 0.1s
  /home/rick/out.pdf
```

A directory positional in this form expands into its images sorted in
*natural* order (`p2` before `p10`, not lexicographic `p10` before `p2`), so a
scanned folder of pages comes out in the right order without renaming
anything first. Two positionals are always the ordinary pair form, never a
merge — `conv a.png out.pdf` converts one file; you need a third path to get
the merge shape.

### Output shapes

A single conversion reports success, size, elapsed time, and the absolute
path the result landed at — the one thing an ambiguous output path (`conv
video.mp4 out.gif`, no directory named) can't otherwise answer:

```console
$ conv clip.mp4 clip.gif
OK clip.gif - 492 KB - 0.2s
  /home/rick/Videos/clip.gif
  note  The whole filtered stream is buffered in memory for palette generation, so very long inputs are slow and memory-hungry rather than being silently truncated.
```

A lossless remux says so:

```console
$ conv clip.mkv clip.mp4
OK clip.mp4 - 55 KB - 0.1s - stream copy, no re-encode
  /home/rick/Videos/clip.mp4
```

A batch gets one summary line instead of a wall of per-job successes —
per-job *failures* still print in full, on stderr:

```console
$ conv ./photos --to jpg -o ./out
OK 2 converted - 0 skipped - 0 failed - 0.1s
  /home/rick/out
```

A failure never looks like a success:

```console
$ conv report.xlsx report.pdf
FAIL report.xlsx -> pdf
  soffice not found
  try  brew install --cask libreoffice
```

(`try` names whichever package manager `conv` finds on `PATH` —
`winget`/`scoop`/`choco` on Windows, `brew` on macOS, `apt-get`/`dnf`/`pacman`
on Linux; on Windows the same failure prints `winget install
TheDocumentFoundation.LibreOffice`.)

`-q/--quiet` silences success output, including a batch summary, but never a
failure. A backend-reported warning on an otherwise-successful job still
prints to stderr as `warning  <text>`, even under `--quiet` — output that
degraded silently but still exited 0 is exactly what quiet is not meant to
hide.

Multi-step recipes are automatic and invisible: `md → pdf` needs no LaTeX
toolchain — pandoc renders to `.docx`, then LibreOffice renders that to PDF,
as two backend invocations behind one command:

```console
$ conv sample.md sample.pdf --dry-run
pandoc sample.md --standalone --resource-path . -o sample.convkit-step0.docx
soffice "-env:UserInstallation=<per-run temp profile>" --headless --norestore --convert-to pdf --outdir . sample.convkit-step0.docx
```

`md → pdf` genuinely needs LibreOffice for this reason — it routes through an
intermediate `.docx`, not directly, so (unlike `docx`/`odt` → `pdf`, see
[Backends](#backends)) there is no pandoc+Typst fallback for this particular
pair.

Exit codes, for scripting: `0` success, `1` conversion failed, `2` usage error
or unsupported pair, `3` a required backend is missing, `4` a batch partly
failed.

## Backends

convkit itself does no encoding — it dispatches to six binaries, each kept at
arm's length as a subprocess (see [Licensing](#licensing)):

| Backend | Used for | Managed by `conv install`? |
|---|---|---|
| `ffmpeg` / `ffprobe` | video, audio, GIF, remux | Yes |
| `magick` (ImageMagick) | image conversions, HEIC/HEIF read, images → PDF | No — see below |
| `pandoc` | Markdown → HTML, DOCX; also the parser behind the docx/odt → pdf fallback | Yes |
| `typst` | pandoc's `--pdf-engine` for the docx/odt → pdf fallback, when LibreOffice isn't installed | Yes |
| `soffice` (LibreOffice) | Office documents ⇄ PDF | No, ever |

Markdown is one-directional: `md → html` and `md → docx` exist; there is no
`html`/`docx` → `md`. (`md → pdf` is the two-step recipe shown above, not a
third direct pandoc pair.)

`magick` is unmanaged for a narrower reason than `soffice`: a managed install
is architecturally possible for it — every official ImageMagick release is
just a Windows `.7z`, a Linux AppImage, or (on macOS) no standalone build at
all, and none of those clear the "plain zip/tar.gz, checksum-verified" bar
`conv install` holds every managed backend to. Install it with your package
manager. `soffice`/LibreOffice has no relocatable binary on *any* platform —
a permanent policy, not a gap that might close later.

**The docx/odt → pdf fallback.** When LibreOffice isn't installed, `docx →
pdf` and `odt → pdf` still work: pandoc parses the document and hands it to
Typst as `--pdf-engine`. Fidelity is lower — exact positioning and some
styling don't survive re-rendering from parsed content, and the result says
so in a warning — but the conversion succeeds instead of failing outright.
This fallback exists **only** for those two pairs: pandoc can't read
`.xlsx`/`.pptx` at all, so those stay LibreOffice-only, and `md → pdf` (above)
also has no fallback, since it needs `soffice` specifically for its second
step regardless of what else is installed.

### Pinned versions

Every convkit build pins exact backend versions in its own manifest
(`crates/convkit-core/src/manifest.rs`), each verified by downloading the
asset and hashing the bytes against an independent second source:

| Backend | Pinned version | Platforms |
|---|---|---|
| ffmpeg / ffprobe | 9.0.1 — the same release everywhere | Windows x64, macOS x64/arm64, Linux x64/arm64 |
| pandoc | 3.11 | Windows x64, macOS x64/arm64, Linux x64/arm64 |
| typst | 0.15.1 | Windows x64, macOS x64/arm64, Linux x64/arm64 |
| magick | — | none (manual install only, everywhere) |
| soffice | — | none (manual install only, permanent policy) |

Linux arm64 has full managed coverage — ffmpeg, ffprobe, pandoc, and typst all
gained a `linux`/`arm64` manifest entry together; it was the one gap among the
five targets convkit itself ships on until it closed.

**ffmpeg and ffprobe are not always one download.** On Windows x64, upstream
bundles both binaries in a single zip, so `conv install ffmpeg` fetches
ffprobe too, from that same download — the install prompt says so up front.
On Linux and macOS (both architectures), ffmpeg and ffprobe are two separate
upstream downloads: `conv install ffmpeg` installs only ffmpeg, and `conv
install ffprobe` is a second, independent fetch. Either way both land at the
same pinned version.

### Resolution order

Every backend is resolved in this order, first match wins:

1. `--<backend>-path` (e.g. `--ffmpeg-path /opt/ffmpeg`) — an explicit override
2. `CONVKIT_<BACKEND>` (e.g. `CONVKIT_FFMPEG=/opt/ffmpeg`) — an environment
   variable, the backend's executable name uppercased
3. The managed directory `conv install` writes to (`%LOCALAPPDATA%\convkit\bin`
   on Windows, `$XDG_DATA_HOME/convkit/bin` or `~/.local/share/convkit/bin`
   elsewhere)
4. `PATH`
5. A short list of well-known install locations (today: LibreOffice's default
   Windows/macOS install paths, since its own installer doesn't add them to
   `PATH`)

An override or env var that names a path which doesn't exist is a hard,
immediate error naming the flag/variable and the bad path — it never silently
falls through to the next candidate, since naming one is an explicit
assertion ("use exactly this"), not a search.

### `conv doctor`

Reports what's installed, where it resolved from, and how to fix what isn't:

```console
$ conv doctor
ffmpeg    9.0.1      /opt/homebrew/bin/ffmpeg     (PATH)
ffprobe   9.0.1      /opt/homebrew/bin/ffprobe    (PATH)
magick    7.1.2-30   /opt/homebrew/bin/magick     (PATH)
pandoc    3.10.2     /opt/homebrew/bin/pandoc     (PATH)
soffice   missing    manual install only  |  brew install --cask libreoffice
typst     0.15.1     /opt/homebrew/bin/typst      (PATH)
```

(`winget`/`scoop`/`choco`/`brew`/`apt`/`dnf`/`pacman` are all detected;
whichever is on `PATH` is what's suggested.) `conv doctor --json` adds
`source` (`override`/`env`/`managed`/`path`/`well_known`, matching the
resolution order above) and `managed_install` — whether `conv install
<backend>` could actually provision this backend on this platform, which is
`false` for `magick` on every platform, not only when it's already missing.

### Installing a missing backend on the fly

A conversion that fails on a missing, managed backend offers to install it
and retry, when the session is interactive and neither `--json` nor
`--quiet` was passed:

```
ffmpeg is required for this conversion and isn't installed.
Install it now? [y/N] y
downloading https://ffmpeg.martin-riedl.de/download/... ...
✓ clip.mp4 · 4.2 MB · 0.1s · stream copy, no re-encode
  /home/rick/Videos/clip.mp4
```

On Windows x64 this adds `(also installs ffprobe)` to the prompt, since that
one platform's download bundles both — see [Pinned versions](#pinned-versions)
above; elsewhere the prompt has nothing to add, since there's nothing bundled.
`--yes` answers yes without ever touching stdin (for a script that wants the
install-then-retry behaviour with no TTY to answer it); `--no-install`
refuses to ever prompt or install, always failing with the plain
`backend_missing` error instead. Passing both is a usage error. `soffice` and
`magick` are never offered here, regardless of flags — see [Backends](#backends)
for why neither has a manifest entry to install from.

### `conv update`

"Up to date" means matching the version *this build of convkit* has pinned —
never "whatever's newest upstream." `conv update --check` reports the state
without changing anything, and exits non-zero only if something needs it:

```console
$ conv update --check
ffmpeg   external     system 9.0.1 (not managed by convkit)
ffprobe  external     system 9.0.1 (not managed by convkit)
magick   unmanaged    installed 7.1.2-30 -- convkit can't update this; run: brew install imagemagick
pandoc   external     system 3.10.2 (not managed by convkit)
soffice  unmanaged    not installed -- convkit can't update this; run: brew install --cask libreoffice
typst    external     system 0.15.1 (not managed by convkit)

conv 0.1.0 -- installed via unknown (/path/to/conv)
  to update: download the latest release from https://github.com/shdwfruit/convkit/releases
```

A backend `conv install` could in principle manage (ffmpeg, ffprobe, pandoc,
typst) reports one of:

- **current** — the managed copy matches the pin
- **outdated** — the managed copy exists but doesn't match the pin; a plain
  `conv update` reinstalls it
- **not_installed** — never provisioned; informational, exit 0, never
  auto-downloaded (`conv install <backend>` is the explicit, one-backend way
  to get it)
- **external** — resolves from `PATH`, an env var, or an override rather than
  convkit's own managed directory (the Homebrew-installed copies in the
  transcript above, for instance). Reported for information only — **never**
  touched, downgraded, or counted against `--check`'s exit code, no matter how
  far its version sits from the pin
- **error** — a real update attempt failed (network, checksum)

`magick` and `soffice` are always `unmanaged`: reported (installed version,
plus the package-manager command that would update them) but never touched —
`conv update` never runs a package manager on your behalf. It also never
replaces the `conv` binary itself; instead it detects how `conv` was
installed (from the executable's own path, plus a cargo-dist install receipt
when one exists) and prints the command that would upgrade it.

### `conv capabilities`

Lists every registered conversion pair and which backend(s) drive it:

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

115 pairs across 27 formats as of this writing — this grows over time, so
treat `conv capabilities`/`conv capabilities --json` as the source of truth,
not the number in this paragraph.

## Flags

`-o/--outdir`, `-j/--jobs` (batch parallelism, defaults to the core count),
`-y/--overwrite` (refused by default — a batch collision skips that one file
and reports it rather than aborting the run), `-q/--quiet`, `--json`,
`--dry-run` (inert: it never probes a non-file input and never creates `-o`'s
directory), `--yes`/`--no-install` (see [Installing a missing backend on the
fly](#installing-a-missing-backend-on-the-fly)), and one `--<backend>-path`
override per backend: `--ffmpeg-path`, `--ffprobe-path`, `--magick-path`,
`--pandoc-path`, `--soffice-path`, `--typst-path`.

## Machine-readable output (`--json`)

Every command emits exactly one JSON document, always with an `"ok"` boolean
plus one command-specific plural key, even when there's only one item in it.
`doctor`/`capabilities`/`install`/`update` write it to stdout on success and
stderr on failure; a conversion always writes every job — success *or*
failure — inside one `"results"` array on stdout, so a half-failed batch is
still one document a script can parse start to finish; only a failure caught
before any job exists (a malformed invocation) gets its own document on
stderr instead. The exit code, never which stream the JSON landed on, is what
tells you pass/fail.

**A conversion** — `{"ok": bool, "results": [...]}`. Each element is a
success:

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

or a failure — `{"ok": false, "input": ..., "output": ..., "error": {"backend",
"code", "message", "remediation": {"managed"?, "manual"?}}}`.

`notes` and `backend_output` are the two additions since backends started
being watched on success, not just on failure: `backend_output` always
carries each step's raw stderr (soffice's stdout, for that one backend),
tail-capped at 16 KiB, whether or not anything went wrong; `notes` is the
distilled, plain-language subset — at most five entries — that convkit's own
classifier recognised as worth a person's attention, which is usually empty
on a clean run. A clean conversion therefore has real text in
`backend_output` but an empty `notes`; a degraded-but-still-successful one
(the backend itself warned about something) has both.

**`--dry-run --json`** — `{"ok": bool, "dry_run": true, "plans": [{"ok": true,
"plan": {...}} | {"ok": false, "error": {...}}]}`.

**A pre-job failure** (bad invocation, before any job exists at all) —
`{"ok": false, "error": {...}}`, the same `error` shape as above, on stderr.

**`conv doctor --json`** — `{"ok": true, "backends": [{"backend", "found":
true, "path", "version", "source", "managed_install"} | {"backend", "found":
false, "managed_install", "remediation"}]}`.

**`conv capabilities --json`** — `{"ok": true, "pairs": [{"from", "to",
"backends": [...]}]}`.

**`conv install --json`** — success: `{"ok": true, "backend", "path",
"installed": [{"backend", "path"}]}`, where `installed` lists every binary
this one download actually placed (the requested backend, plus any bundled
sibling — see [Pinned versions](#pinned-versions)); failure: the pre-job
failure shape above.

**`conv update --json`** — `{"ok": bool, "backends": [{"backend", "managed",
"installed", "version"?, "pinned_version"?, "action", "path"?,
"manual_hint"?, "error"?}], "conv": {"version", "exe_path"?, "install_method",
"update_hint"}}`. `action` is one of `current`/`outdated`/`not_installed`/
`external`/`updated`/`error`/`unmanaged` — see [`conv update`](#conv-update)
above. `ok` is `false` only when `--check` found something `outdated`, or a
real update hit an `error`.

## Troubleshooting

**Debian/Ubuntu: HEIC conversion fails with `Unsupported codec`** even though
ImageMagick is installed and every other conversion works. Plain `apt-get
install imagemagick` there ships without an HEVC decoder — install
`libheif-plugin-libde265` alongside it. This is a Debian/Ubuntu packaging
split (HEVC decoding is its own plugin package there), not a convkit
limitation; macOS Homebrew and Windows ImageMagick builds include it already.

**Windows: SmartScreen warns on first run.** The MSI and the standalone
binaries aren't code-signed yet — see [Install](#install).

**Scripting around a missing backend.** Every `backend_missing` error's
`remediation` names both the `conv install` command (when the manifest
supports one) and the manual package-manager command — see
[Machine-readable output](#machine-readable-output---json).

## Licensing

convkit itself is dual-licensed under [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE), your choice. It **invokes** ffmpeg, ImageMagick,
LibreOffice, pandoc, and Typst as subprocesses; it does not link against,
embed, or redistribute any of them. Each keeps its own licence: ffmpeg is
LGPL-2.1+ by default and GPL-2+ once built with a GPL-licensed component such
as libx264 (true of most prebuilt binaries, including the ones `conv install`
fetches — check a given build's own `-version` banner if this matters to
you); ImageMagick uses its own permissive "ImageMagick License"; LibreOffice
is MPL-2.0 (with some LGPLv3+-licensed code); pandoc is GPL-2.0-or-later;
Typst is Apache-2.0. None of that attaches to convkit itself — the FSF's own
GPL FAQ places invoking a separate program at arm's length under "mere
aggregation," not a combined or derivative work. convkit ships none of these
tools' source or binaries; `conv install` downloads official upstream builds
straight from their publishers. See
`docs/superpowers/specs/2026-08-29-convkit-design.md` §2 for the fuller
reasoning this constraint was designed around.

## Contributing

```console
$ cargo fmt --all -- --check
$ cargo clippy --workspace --all-targets -- -D warnings
$ cargo test --workspace
```

As of this writing (2026-08-30): **412 passed, 0 failed, 6 ignored** — the
ignored tests are real conversions against real backends; run them with
`cargo test --workspace -- --ignored` once ffmpeg/ImageMagick/pandoc/typst are
on `PATH` (LibreOffice too, for the one test that needs it). These counts
move as the suite grows — `cargo test --workspace` is the source of truth,
not this paragraph. CI's `integration` job runs the `--ignored` suite against
real backends on Ubuntu unconditionally, no skipping.

`convkit-core` (the library crate: planning, resolution, execution) never
writes to stdout or stderr itself — all user-facing output belongs to the
`conv` binary crate. CI enforces this directly: a "core must not print" step
greps `crates/convkit-core/src` for stray `println!`/`eprintln!`/`print!`/
`eprint!`/`dbg!` and fails the build if it finds any (see
`.github/workflows/ci.yml`).

## Further reading

- [`docs/defaults-calibration.md`](docs/defaults-calibration.md) — every
  quantitative claim in this README, measured, with the exact commands to
  reproduce each one, including the one that didn't hold up.
- [`docs/superpowers/specs/2026-08-29-convkit-design.md`](docs/superpowers/specs/2026-08-29-convkit-design.md) —
  the original design rationale: prior art, why Rust, the full non-goals
  list. Marked historical; see its own status note for where it has since
  diverged from the implementation.
