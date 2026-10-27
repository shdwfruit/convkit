# Changelog

## Unreleased

### New

- **`--paste` and `--copy`.** `conv --paste .jpg` converts the picture or
  the files on the clipboard: a screenshot is written here as
  `clipboard.jpg`, and files copied in Finder or Explorer are converted
  beside themselves. `conv --paste` alone saves a copied picture as it is.
  `--copy` puts the result on the clipboard, a picture as the picture and
  its file, an md, csv or html file as its text, and anything else as the
  file. `conv qr` takes `--copy` too. Over SSH both go through the
  terminal, in kitty and Ghostty 1.4 or later.

## 0.6.1 - 2026-10-23

### New

- **`conv changelog` shows what each version added.** It lists every
  version, newest first, from the changelog built into conv, so it works
  offline. `conv changelog 0.5.0` shows one version, and `--since` with the
  version you had shows what's new after an upgrade.

## 0.6.0 - 2026-10-23

### New

- **ASCII art with `conv ascii`.** `conv ascii photo.jpg` prints a picture
  as ASCII art, sized to the window, and `conv ascii photo.jpg .png` writes
  it as a picture; `.txt`, `.html` and any picture format conv writes work
  too. `--chars` picks the style: `ascii` (the default, with `| / \ - _`
  along clear edges), `blocks` (two colours a character), `braille` (eight
  dots a character, the most detail), or your own characters from least ink
  to most, like `--chars " .:oO@"`. Blocks keep the picture's colours
  wherever colour can show, and `--color` adds them to the other styles.
  `--width`, `--height`, `--bg light`, `--contrast` and `--brightness`
  shape it, and `--clipboard` copies the text for Discord, Slack or GitHub.
  A transparent border is left out, and transparent parts stay empty.
- **`conv ascii -i` tunes the art with keys.** The arrows change the width
  and contrast, `[ ]` the brightness, `c` the style, `b` the background and
  `o` colour, on a preview drawn on the background the art is made for. `w`
  writes it, `y` copies it, and `q` prints the command that makes it again.
- **QR codes with `conv qr`.** `conv qr "https://example.com"` writes
  `qr.png`, or an SVG, JPG or other format by name. The text is in the code
  itself, so it never expires. `--logo` puts a logo in the middle and
  `--size` sets the size. conv reads the code back before writing it, and
  writes nothing if it doesn't give back the exact text.
- **`--speed` and `--shift-pitch`.** `conv lecture.mp4 --speed 1.5` writes
  `lecture-1.5x.mp4`, for video, GIF and audio, from 0.01x to 100x. The
  sound keeps its pitch unless `--shift-pitch` lets it move, like a tape.
  Subtitles and chapters are dropped, with a note, since their times would
  no longer match.

### Fixed

- **`-o` with a named output.** `conv a.png b.jpg -o out` wrote `b.jpg`
  beside the input and left `out/` empty. It now writes `out/b.jpg`, and
  `conv a.png .jpg -o out` writes `out/a.jpg`. A name with its own folder
  plus `-o` is refused, since that gives two places to write.

## 0.5.1 - 2026-10-12

### New

- **Opus and Ogg.** `.opus` and `.ogg` files, such as WhatsApp and Telegram
  voice notes, convert to mp3, m4a, wav and flac, and any audio or video
  file converts to them. An `.ogg` target gets Opus, always at 48 kHz.
  Opus, and Vorbis going to `.ogg`, is stream-copied, so `.opus` and `.ogg`
  convert into each other without re-encoding. Ogg cannot hold cover art,
  so it is dropped, with a note when there was some. They can be cut and
  stripped like the other audio formats, and their tags carry over into
  the others.
- **Word, Excel and PowerPoint 97-2003.** `.doc`, `.xls` and `.ppt` convert
  to docx, xlsx and pptx, and to PDF, through LibreOffice. An old `.doc`
  that is really RTF or a saved web page converts too. A password-protected
  file fails with an error that says so, under its own code,
  `password_protected`, and a note says when macros are dropped.
- **Icons.** png, jpg, webp and svg convert to `.ico` with every size from
  16 to 256 px, and an `.ico` converts to png from its largest size. A
  source smaller than 256 px is enlarged for the sizes above it, with a
  warning that names them. A picture that is not square is centered on a
  transparent square rather than stretched.
- **CSV.** CSV converts to xlsx and ods, and xlsx and ods convert to CSV.
  conv reads the CSV first for its encoding, separator and decimal commas,
  and keeps a column with leading zeros or long numbers as text, so a zip
  code like `02134` and a card number keep every digit, and a cell starting
  with `=` is not run. A workbook writes its first sheet, as values, in
  UTF-8, with a note when it has more than one.

## 0.5.0 - 2026-10-09

### New

- **Cut a clip with `--start`, `--end` and `--duration`.** `conv talk.mp4
  --start 1:02 --end 1:10` writes `talk-1m02s-1m10s.mp4`, and an output name
  changes the format as well (`clip.gif`, `.mp3`). Times are seconds, `m:ss`
  or `h:mm:ss`, and a leading `-` counts back from the end. The cut is exact
  to the frame. A cut that starts after 0 re-encodes the video, and so does
  one from 0 of video with B-frames, with a note that says why; any other
  cut from 0 is a stream copy. `--max-size` sizes the clip rather than the
  whole file, and `--strip-metadata`, `--fps`, `--resize` and `--crf` work
  as usual. A `--to` batch writes into a folder named after the range
  (`1m02s-1m10s/`), and `--json` gets a `range` object.
- **`conv trim FILE` cuts clips in the terminal.** The frame under the
  slider is drawn at the top, with a bar of the video's colours and a bar of
  its loudness below. The arrow keys move the slider, `c` marks a clip's
  start and then its end, and `w` writes every clip the way the flags would.
  A clip can keep the picture alone or the sound alone. The picture uses
  kitty's or iTerm2's graphics where the terminal has them and text
  characters elsewhere; `--graphics` overrides the choice.

## 0.4.0 - 2026-10-09

### New

- **Location note.** Phone photos and videos record where they were taken,
  and a converted copy keeps that in most formats. conv now says so when it
  applies: "The source records a GPS location, and the jpg keeps it; add
  --strip-metadata to remove it." It reads an image's EXIF and a video or
  audio file's tags. A WebP source, a location kept only in XMP, and a HEIC
  photo on ImageMagick 6 get no note.
- **`--strip-metadata`** removes the location and the rest of the metadata:
  camera, serial number, owner, capture time. An image keeps only its colour
  profile and is turned upright first. Video and audio keep their title,
  artist, album, album artist, composer, genre and track and disc numbers,
  and a stream copy stays a stream copy. `conv photo.jpg --strip-metadata`
  writes `photo-stripped.jpg`; a jpg, webp or avif is re-encoded to do it,
  and a note says so. Documents are not covered yet and refuse the flag.

### Changed

- **Sized and stripped batches write into a folder.** A `--to` batch with
  `--max-size` or `--strip-metadata` now writes into a folder next to its
  files, named after the flag (`clips/10mb/`, `photos/stripped/`), unless
  `-o` says where. Both flags take files already in the target format as
  inputs, so running the same batch again used to size or strip its own
  results. Until now `--max-size` batches wrote beside their inputs. A
  single file still gets `clip-10mb.mp4` beside it.
- **Same-named inputs no longer stop a batch.** Two inputs that would write
  one file, like `a.jpg` and `a.png` with `--to webp`, keep their source
  format in the name (`a-jpg.webp`, `a-png.webp`) instead of the whole batch
  being refused.

## 0.3.1 - 2026-10-02

### Changed

- **Notes only when they apply.** Converting to jpg, png or bmp no longer
  warns about transparency or extra frames the source doesn't have, so a
  phone photo converts without a note. A GIF made from a video of 30 seconds
  or less no longer gets the memory note. When conv can't tell, the note
  still shows. `warnings` in `--json` follows the same rule, so it can now be
  empty where it wasn't before.

## 0.3.0 - 2026-10-02

### New

- **Video file too large? `--max-size` makes it fit.** `conv clip.mp4 --max-size 5mb`
  writes `clip-5mb.mp4` at or under 5 MB, for upload limits and attachments.
  conv chooses the resolution, frame rate and bitrates together, so a tight
  target trims a little from each rather than everything from one. It encodes
  in two passes, measures the result, and plans again if it came out over (three
  attempts at most). Targets: mp4, mov, mkv and webm. A target too small to look
  good is converted only after a warning, a suggested size and a `[y/N]`
  question.
- **Video knobs.** `--fps` caps the frame rate and `--crf` sets the encoder's
  quality anchor on video and GIF targets. `--resize` now applies to video and
  GIF as well as images. A cap the source already satisfies changes nothing,
  and a stream copy stays a stream copy.
- **`--upscale`.** `--resize` never enlarges, on any target. `--upscale` lets it,
  with a warning that enlarging adds no detail and makes a larger file. Above
  four times the source's pixels, conv also shows the output size and a rough
  file-size estimate, and asks `[y/N]`.
- `--yes` now answers these questions as well as the backend-install prompt.
  Without a terminal to ask and without `--yes`, nothing is converted and conv
  exits 2 (`confirmation_required` in `--json`).

### Changed

- **Images no longer enlarge without `--upscale`.** Until 0.3.0, `--resize
  1600x900` on a 320x240 image produced 1200x900; it now leaves the image at
  320x240. Add `--upscale` for the old result.
- **`conv capabilities --json`:** `defaults` moved from the top level into each
  `targets[]` row, because a default belongs to a pair (mkv → mp4 and mkv → webm
  carry different CRFs). A key with no default is left out rather than `null`.
- **GIF:** an explicit `--fps` or `--resize` replaces the GIF defaults (15 fps,
  640 px wide), capped at the source, even when the source already satisfies it.

### Fixed

- `conv capabilities` listed `quality 92` as a default for video formats, which
  refuse `--quality`. Its "tuning flags when writing X" line also claimed flags
  that only some sources into X take.
- `conv --help` printed internal developer notes under several flags.

## 0.2.0 - 2026-09-01

- `conv scan` lists the files in a directory and what each can become.
- `.jfif` is read as JPEG.
- Homebrew updates: each release publishes its formula to `shdwfruit/tap`, so
  `brew upgrade convkit` sees new versions.
- `conv capabilities --json` spells `kind` in lowercase (`"image"`), matching
  `conv scan`.

## 0.1.0

First release.
