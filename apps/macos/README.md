# ConvKit for Mac (prototype)

A SwiftUI toolbox for conv. Each tool does one job, and every tool runs on your Mac.

Finding a tool
- The sidebar groups the tools under Video, Audio, Images, Documents, Text,
  Developer and Files. Search at the top matches names, descriptions and
  formats, so "heic" finds the picture tools and "shrink" finds Compress
  Video. Return opens the first match.
- Home shows the popular tools, the ones you used last, and every section.
- Drop a file on Home and it lists the tools that fit that file. Pick one and
  the file is already in it. Files opened from Finder or the Dock work the same
  way, unless the tool showing takes them.

The tools
- Video: Compress Video, Convert Video, Trim Video, Video to GIF, Extract
  Audio, Change Speed, Resize Video, Turn Video, Remove Metadata.
- Audio: Convert Audio, Trim Audio, Change Speed, Remove Metadata.
- Images: Convert Image, Resize Image, Turn Image, Remove Location, Make an Icon.
- Documents: Merge PDFs, Split PDF, Photos to PDF, Convert Document.
- Text: QR Code, ASCII Art.
- Developer: Format JSON (`conv fmt` as you type: tidy, pack, sort, check,
  or write another language), Convert Data (JSON, YAML, TOML and CSV files).
- Files: Convert Anything (every format and option), Scan Folder.
- conv: Backends (install and update what conv uses), Formats, What's New.

Most tools are Convert with a narrower form: the same `conv FILE .ext --flags`,
but showing only the options for that job. More Options carries the files
and settings into Convert Anything.

Options that don't fit the files are greyed out, and hovering says why: a
PNG has no quality setting, a picture has no frame rate, End and a length
can't both be set. Convert Anything greys out everything until a file is
added. What fits comes from conv itself: `conv capabilities` for each format,
and what a format's own sources take on the way to it when a file keeps its
format. A file a tool doesn't take is listed but left out, with a pointer to
Convert Anything.

The app runs the `conv` binary and doesn't copy any of conv's rules. The name
of each new file, and any problem, come from `conv --dry-run --json` as you
change options. Conversions run with `--json`.

It never overwrites or forces anything on its own. If the new file already
exists, the row offers Replace. If a size limit is too small, the row shows
conv's warning and offers Convert Anyway.

A tool only shows when your conv has the command behind it, so new conv
commands can get a tool without breaking older installs.

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
