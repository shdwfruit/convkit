---
name: convkit
description: Convert, edit and inspect files locally with the `conv` CLI (convkit) instead of writing ffmpeg, ImageMagick, pandoc, LibreOffice or qpdf commands or Python scripts by hand. Use whenever a task needs a file in another format (video to GIF or MP4, HEIC to JPG, DOCX to PDF, Markdown to PDF or DOCX, audio out of a video), a media edit (cut, resize, crop, rotate, speed, denoise, shrink to a size, strip metadata), PDF work (merge, split, reorder or delete pages, compress, lock or unlock, pages to images), zip or tar archives, or a file's facts (format, streams, dimensions, pages). Also use it for checksums, base64, JWTs, Unix timestamps, UUIDs, JSON/YAML/TOML formatting and cleaning tracking out of links, rather than working them out by hand.
---

# convkit (`conv`)

`conv` maps a source and target format onto a tuned command for the right
backend and runs it on this machine. Files never leave the computer. Its
defaults are better than what you would type from memory (lossless remux
when the codecs fit, palette-generated GIFs, orientation and colour
profiles kept), so prefer `conv` over hand-written ffmpeg/magick/pandoc
commands or ad-hoc scripts.

Check it is installed with `conv --version`. If it isn't, tell the user
and point them at https://github.com/shdwfruit/convkit#install. Don't run
an installer yourself without asking.

## How to call it

- `conv IN OUT`: `conv clip.mkv clip.mp4`
- `conv IN .ext`: the output is written next to the input with that extension.
- `conv DIR --to jpg -o out/`: a batch run on every matching file in DIR.
- `conv IN --flag`: a knob alone keeps the format and adds a suffix, e.g.
  `conv clip.mp4 --max-size 10mb` writes `clip-10mb.mp4`.

**Always pass `--json`.** Every command then prints exactly one JSON
document on stdout with a top-level `"ok"` and one command-specific plural
key, such as `results` (conversions), `plans` (`--dry-run`), `pairs`
(`capabilities`), `files` (`info`, `scan`, `hash`), `backends` (`doctor`),
`times` or `links`. With `--json`, conv
never prompts and never installs anything. It refuses and exits instead.

Exit codes: `0` success, `1` conversion failed, `2` usage error,
unsupported pair, output exists or confirmation needed, `3` backend
missing, `4` a batch partly failed.

## Workflow

1. **Find out what's possible. Don't guess.**
   - `conv info FILE --json`: what a file actually is (codecs, size,
     dimensions, pages, rotation, GPS present) and what it can become.
   - `conv capabilities FMT --json`: a format's targets and sources, the
     tuning flags each pair accepts, and the baked-in defaults.
   - `conv scan DIR --json`: what each file in a folder can become.
   - `conv doctor --json`: which backends are installed.
2. **Preview when it matters.** `--dry-run --json` returns the exact
   backend argv plus `warnings`, without running anything. Use it before a
   batch, before a lossy or expensive job, or when the user wants to see
   the command.
3. **Run it**, then read the result: `output`, `bytes`, `remuxed`,
   `warnings`, `notes`. Tell the user the output path and size, and pass on
   any warnings or notes, since they flag lossy or surprising results.
   Don't print `backend_output`. It is raw backend stderr, only useful when
   debugging a `conversion_failed`.

Leave the defaults alone unless the user asked for something specific.
Every knob is listed in `conv --help`. A flag that doesn't apply to a pair
is an error, not silently ignored, so check `capabilities` first.

## Errors

A failure carries `error.code`, `error.message` and often
`error.remediation`. Act on the code:

| code | what to do |
|---|---|
| `backend_missing` | Show the user `remediation` (`managed` is a `conv install X` command; `manual` is the package-manager command). Ask before running either: they download from the network. |
| `output_exists` | Pick another output name, or ask before adding `-y`/`--overwrite`. Only overwrite without asking if you created that file earlier in this task. |
| `confirmation_required` | conv wants a yes for something drastic (an extreme `--max-size`, a large `--upscale`, rewriting a signed PDF). Explain the message to the user and add `--yes` only if they agree. |
| `password_protected` | The user must supply the password. conv reads it from `CONVKIT_PASSWORD` and never from the command line. Ask them to set it in their own shell, or have them run the command. Don't put a password in the chat. |
| `unsupported_pair`, `unknown_format` | Check `conv capabilities FMT --json`. Often there's a two-step route, or a nearby format (the message suggests one). |
| `invalid_invocation` | Read the message. It names the flag or argument that's wrong. |
| `input_not_found`, `invalid_data`, `unsafe_archive` | Report it. Don't retry blindly. |
| `conversion_failed` | Now read `backend_output` for the cause, then report it or fix the input. |

In a batch, each element of `results` has its own `ok` and `error`, and
the rest of the batch still runs.

## Privacy and the network

These rules keep convkit's offline promise, so follow them:

- Only `conv install`, `conv update` and `conv link --follow` use the
  network. Ask before running any of them.
- Don't run `conv wifi` unless the user asks for that specific Wi-Fi
  password or QR code. Even its errors list the saved network names.
- Don't use `--copy` or `--paste` unless asked: they read and write the
  user's clipboard.
- Avoid `conv trim`. It's an interactive terminal UI. Use
  `--start`/`--end`/`--duration` instead.

## Common tasks

```console
conv clip.mov clip.mp4                   # remux if the codecs fit, else encode
conv clip.mp4 clip.gif                   # palette-tuned GIF
conv clip.mp4 .mp3                       # extract the audio
conv clip.mp4 --start 1:02 --end 1:10    # exact cut -> clip-1m02s-1m10s.mp4
conv clip.mp4 --max-size 10mb            # fit under a size (video, PDF)
conv clip.mp4 .jpg --every 2s            # stills -> clip/clip-0001.jpg ...
conv photo.heic .jpg                     # keeps orientation and colour profile
conv ~/Pictures --to webp -o web/        # batch into a folder
conv photo.jpg --strip-metadata          # GPS etc. removed -> photo-stripped.jpg
conv photo.jpg --resize 1600x            # fit within; never enlarges
conv report.docx report.pdf              # LibreOffice
conv notes.md notes.pdf                  # pandoc, then LibreOffice
conv scan.pdf --compress                 # smaller, looks the same
conv report.pdf .png --pages 1-3         # pages to pictures
conv merge a.pdf b.pdf out.pdf           # also videos or audio: a.mp4 b.mov out.mp4
conv split report.pdf 1-3 4-z            # one file per range (z = last page)
conv pages report.pdf 3 1-2 5-z          # reorder/keep pages
conv photos/ .zip                        # pack; conv photos.zip unpacks
conv hash file.iso --check <sha256>      # checksums; never compute one in your head
conv base64 -d aGVsbG8=                  # base64 / data: URIs
conv jwt "$TOKEN"                        # decode a JWT locally
conv time 1730000000                     # timestamps <-> dates
conv fmt data.json                       # pretty-print/validate JSON, YAML, TOML
conv link "https://x.com/?utm_source=a"  # strip tracking, offline
```

Add `--json` to each when you're the one reading the output.
