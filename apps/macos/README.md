# ConvKit for Mac (prototype)

A SwiftUI window for conv. Drop files in, pick what you want, press Convert.

It runs the `conv` binary and doesn't copy any of conv's rules. Formats come
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
