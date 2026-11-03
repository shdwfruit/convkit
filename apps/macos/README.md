# ConvKit for Mac (prototype)

A SwiftUI toolkit for conv, with a tool in the sidebar for each thing conv does.

Files
- Convert: drop or paste files in, pick a format and any of conv's options (size
  limit, resize, turn, quality, colors, frame rate, CRF, cuts, speed, metadata),
  press Convert. A copied picture or screenshot can be pasted too, and the result
  can go straight to the clipboard.
- Trim: play a video or audio file, mark clips with I and O, write them all.
  A clip can keep just the sound.
- Merge PDFs: join PDFs in the order you drag them into.
- Split PDF: one file a page, or one for each range you type, with the pages shown.
- Scan Folder: what each file in a folder could become. Send any of them to Convert.

Make
- QR Code: type a link or text, see the code conv made and checked, save or copy it.
- ASCII Art: drop a picture, GIF or video, tune the look, copy the text or save it.

conv
- Backends: which programs conv uses, where they are, and Install or Update for
  the ones conv manages.
- Formats: every format, what it turns into, and which options apply.
- What's New: the changelog built into conv.

A tool only shows when your conv has the command behind it, so new conv
commands can get a tool without breaking older installs.

The app runs the `conv` binary and doesn't copy any of conv's rules. In Convert, formats come
from `conv capabilities`. The name of each new file, and any problem, come
from `conv --dry-run --json` as you change options. Conversions run with
`--json`.

It never overwrites or forces anything on its own. If the new file already
exists, the row offers Replace. If a size limit is too small, the row shows
conv's warning and offers Convert Anyway.

## Build

Needs macOS 14 and the Swift toolchain (Xcode or the Command Line Tools).

    ./build-app.sh                 # build/ConvKit.app, uses conv from your PATH
    ./build-app.sh --bundle-conv   # also copies ../../target/release/conv inside

Or run it straight from the package:

    swift run ConvKit ~/Movies/clip.mov

The app looks for conv in Settings, then `$CONV_BIN`, then inside the app,
then `~/.local/bin`, `/opt/homebrew/bin`, `/usr/local/bin` and `~/.cargo/bin`.

## Not yet

- No progress bar for a single file, because conv's `--json` mode reports at the end.
- One file at a time.
- Unsigned. Gatekeeper will ask the first time it's opened from a download.
