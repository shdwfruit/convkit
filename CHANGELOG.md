# Changelog

## 0.7.0 - 2026-11-04

### New

- **JSON, YAML, TOML and CSV, converted by conv itself.** `conv data.json
  .yaml`, `conv Cargo.toml .json`, `conv orders.csv .json` and the other
  ways round run with no backend, keeping every key in its place and every
  number's digits. What a format can't hold, like a null in TOML or NaN in
  JSON, is refused with where it is. YAML is read safely, with aliases
  capped against "billion laughs" files. A CSV's columns of numbers and of
  true and false become numbers and booleans and the rest stay text, and
  nested JSON becomes dotted columns. Large JSON and CSV files stream.
  `--copy` copies a JSON, YAML or TOML result as its text.
- **`conv fmt`** pretty-prints, packs (`--minify`), sorts (`--sort-keys`)
  and checks (`--check`) JSON, YAML and TOML from a file, a pipe or
  `--paste`, and prints it in colour, writes a file, or copies it with
  `--copy`. An invalid file gets the line, the column, a caret under the
  spot and the usual cause. `--lenient` reads JSON5 and JSONC, and `--to`
  converts as it goes.
- **Text and developer tools.** `conv base64`, `hash`, `uuid`, `jwt`,
  `time`, `link`, `count`, `case`, `chars`, `lorem` and `diff` do the small
  jobs people otherwise paste into a website, here and offline. `conv hash
  FILE --check HASH` says plainly whether a download matches, and `--sums
  SHA256SUMS` checks a release. `conv jwt` shows a token's claims and
  expiry, and checks an HMAC signature with a secret it asks for without
  showing. `conv time` turns timestamps into dates and back, saying which
  unit it took. `conv link` takes the tracking out of links and unwraps
  Google, Facebook, Outlook and other redirects; only `--follow` goes
  online. `conv chars` flags invisible characters, bidi controls and
  lookalike letters, and `conv diff` marks the changed words inside
  changed lines. Each reads its argument, a file (`-f`), a pipe or the
  clipboard (`--paste`), and takes `--copy` and `--json`.
- **`--compress` makes a PDF smaller with the same look.** `conv
  scan.pdf --compress` writes `scan-compressed.pdf`: images drawn above
  225 dpi go to 150 dpi, high-quality JPEGs are saved again at quality 80,
  and photos stored without loss become JPEGs when they look the same.
  Text, links, bookmarks, form fields, tags and attachments are kept as they
  are, and CMYK stays CMYK. The result is never bigger than the original;
  when it saves little, conv measures 150 dpi and suggests the `--max-size`
  that reaches it. Runs on qpdf and conv's own image pass, with no AGPL code.
- **`--max-size` takes PDFs.** `conv deck.pdf --max-size 5mb` tries qpdf's
  lossless rewrite, then lower image resolutions, and keeps the first one
  under the size; below 120 dpi it asks first. A signed PDF asks before any
  rewrite.
- **`conv merge` and `conv split` for PDFs.** `conv merge a.pdf b.pdf out.pdf`
  joins PDFs in order; a folder adds every PDF in it. `conv split report.pdf`
  writes one file per page, and `conv split report.pdf 1-3 4-z` one per range
  (`z` is the last page, `5-1` reverses). Pages no range covers are left out
  with a warning. Both run on qpdf, so text and links come through untouched.
  The merged file keeps the first file's bookmarks only, and permission
  restrictions are dropped; conv notes both when they apply.
- **`conv pages` picks, reorders, deletes and turns a PDF's pages.**
  `conv pages report.pdf 3 1-2 5-z` writes those pages in that order, as
  `report-3_1-2_5-12.pdf`. `--delete 4,6-7` leaves pages out, and
  `--rotate right:2,4` turns them (`--rotate right` turns them all). Page
  numbers always mean the input's pages. Bookmarks, links and metadata
  stay, and conv notes a bookmark that now goes nowhere.
- **Passwords on PDFs.** `conv report.pdf --password` writes
  `report-locked.pdf`, locked with AES-256 and a password asked twice
  without showing it; `--restrict print,copy,edit` turns those off as well.
  `--unlock` writes a copy without the password, and a `--to pdf` batch of
  either writes into a `locked` or `unlocked` folder. `merge`, `split` and
  `pages` open locked PDFs too, asking for the password or reading
  `CONVKIT_PASSWORD`, and `--password` locks what they write. A password
  never goes on qpdf's command line.
- **`conv merge` takes pictures among PDFs, and joins video and audio.**
  `conv merge scan.pdf receipt.jpg out.pdf` puts each picture on a page the
  size of the PDF page next to it, turned to suit it, with the picture at
  its own size or shrunk to fit. `conv merge a.mp4 b.mov out.mp4` and
  `conv merge *.mp3 album.mp3` join video and audio files, without
  re-encoding when they match. When they don't, every clip is encoded once
  to the first one's size, frame rate and sound, scaled and padded rather
  than stretched, with silence for a clip without sound, and a note says
  which files differed and how. `--resize`, `--crf` and `--max-size` work
  on a merge of videos.
- **qpdf is a managed backend.** `conv install qpdf` downloads qpdf 12.4.2 for
  all five platforms, checks its SHA-256, and installs it as a folder. The
  Linux build needs glibc 2.34 or newer. `--qpdf-path` and `CONVKIT_QPDF`
  point conv at another copy.
- **`--paste` and `--copy`.** `conv --paste .jpg` converts the picture or
  the files on the clipboard: a screenshot is written here as
  `clipboard.jpg`, and files copied in Finder or Explorer are converted
  beside themselves. `conv --paste` alone saves a copied picture as it is.
  `--copy` puts the result on the clipboard, a picture as the picture and
  its file, an md, csv or html file as its text, and anything else as the
  file. `conv qr` takes `--copy` too. `--paste` works wherever conv takes
  a file: `conv ascii`, `conv trim`, and `conv qr`, where a copied link is
  what the code holds and a copied picture is the logo. Over SSH both go
  through the terminal, in kitty and Ghostty 1.4 or later.
- **Photos go into a PDF without ImageMagick.** `conv a.jpg b.png out.pdf`
  now runs on Typst, which `conv install typst` sets up. A JPEG goes in as
  it is, without being re-encoded, and `--strip-metadata` strips it the same
  way. PNG and WebP pages are kept without loss, and a note says when that
  makes the PDF much larger. GIF to PDF works too. HEIC, AVIF, TIFF and
  BMP, and `--quality`, `--resize` and `--rotate`, still go through
  ImageMagick, as does any merge when Typst isn't installed.
- **Text read aloud.** `conv notes.txt .m4a` reads a text into a sound file
  (also .mp3, .wav and .aiff), and md, html, docx, odt and rtf are read as
  their text. `conv speak "Hello" hello.m4a` reads the words given, a pipe,
  `-f FILE` or `--paste`, and `conv speak --voices` lists the voices.
  `--voice` and `--rate` pick the voice and the words per minute. macOS uses
  `say` with the best Premium or Enhanced voice you have, and says how to
  get one; Windows uses its own voices, and Linux espeak-ng. A Piper voice
  file (`--voice en_US-lessac-medium.onnx`) reads with Piper anywhere.
- **Text, web pages and RTF.** `.txt` converts to pdf, docx, html, md and
  rtf, in UTF-8, UTF-16 or Windows-1252, and docx, odt, rtf, md and html
  convert to `.txt`. A saved web page converts to pdf, docx, md, txt and rtf
  without anything being downloaded: a picture on the web becomes a link,
  and a warning says so. RTF converts to pdf, docx and txt through
  LibreOffice (pandoc without it), and docx, odt, md and html to RTF.
- **AIFF.** Audio and video convert to and from `.aiff`, cuts and
  `--speed` included.
- **`conv beautify` for screenshots.** `conv beautify shot.png` writes
  `shot-beautified.png`: the screenshot on a gradient, with padding,
  rounded corners and a soft shadow. `--bg` picks one of six gradients, a
  colour, your own gradient or `transparent`, and `--padding`, `--radius`
  and `--no-shadow` change the rest. The screenshot is never scaled, so a
  Retina one stays sharp, and `conv beautify --paste --copy` goes from the
  clipboard to the clipboard.
- **`conv colors` gives a picture's main colours.** It prints about six, with
  their shares, as HEX, RGB and HSL, with swatches in a terminal. `--at X,Y`
  gives one pixel's colour, and `--code` writes them as CSS, SwiftUI or
  Tailwind. It reads every picture conv converts.
- **`conv wifi` shows a saved network's password.** `conv wifi Home` prints
  its security and password, and `--qr` writes a code that joins it, as
  does `conv qr --wifi Home`. On macOS the keychain asks first. The
  password is printed only to a terminal unless `--show-password` is given.
- **A PDF's pages as pictures.** `conv report.pdf .png` saves every page
  into `report/` as `report-01.png` and on, and a PDF with one page gives
  one file. JPEG, WebP and HEIC work too. `--pages 1-3,7` picks pages,
  `--dpi 300` draws them sharper than the default 150, and `--resize` sets
  their size. Comments, stamps and filled-in form fields show as they do in
  a viewer. PNG and JPEG run on qpdf, Typst and ffmpeg, which conv
  installs; WebP and HEIC need ImageMagick. conv asks before writing more
  than 1,000 files, and when a run needs several backends it can install,
  it offers each in turn.
- **`--crop`.** `conv clip.mp4 --crop 9:16` keeps the largest 9:16 box in
  the middle of the picture; `1280x720+0+140` is a box at a place,
  `1280x720` one in the middle, and `auto` cuts away a video's black bars
  or a picture's plain border. It works on photos, video and GIFs, in the
  picture as it is shown after its orientation and `--rotate`, and before
  `--resize`. One file is named `-cropped`, and a batch writes into
  `cropped/`.
- **`--audio` and `--mix`.** `conv clip.mp4 --audio song.mp3` replaces a
  video's sound and copies the picture. `--mix` plays the new sound under
  the old one, a quarter as loud after measuring both, and `--mix=50%` sets
  how loud. The video's length decides: a longer sound stops at its end, a
  shorter one leaves the rest silent, with a note. `--start`/`--end` cut the
  video and `--speed` retimes it, and the new sound plays from the clip's
  start at its own speed. Named `-audio` or `-mixed`.
- **`--subtitles`.** `conv clip.mp4 --subtitles subs.srt` burns an .srt,
  .vtt or .ass file's subtitles into the picture: white with an outline,
  near the bottom, sized to the picture and wrapped to fit, italics and bold
  kept. conv draws them itself in the system's fonts, with right-to-left
  and other scripts set properly, so it works with an ffmpeg that has no
  subtitle filter, as Homebrew's doesn't. A cut or speed change moves them
  with the video. Video and GIF targets; named `-subtitled`.
- **A GIF changes speed and cuts as a GIF.** `conv anim.gif --speed 0.5`,
  `conv anim.gif --start 1 --end 3` and `conv trim anim.gif` used to ask
  for `--to mp4`; now the GIF stays a GIF, at its own size and frame
  timing. Browsers play a GIF no faster than 50 fps, so one sped past that
  drops frames to keep its speed, and a note says so.
- **HEIC as a target.** `conv photo.jpg .heic` writes HEIC through
  ImageMagick, at a quality that lands near what a phone writes, keeping
  the colour profile and EXIF with the usual location note.
  `--strip-metadata` keeps a HEIC as HEIC. It needs a libheif with an HEVC
  encoder; ImageMagick's Windows build has none, and conv says so and
  suggests AVIF or JPEG.
- **`--rotate right|left|180`** turns photos, videos and GIFs. A JPEG kept
  as JPEG, or an mp4 or mov that is copied, has only its orientation tag
  reset when that is what was wrong, with nothing re-encoded; otherwise the
  pixels turn. Either way every app shows the same picture afterwards.
  Named `-rotated`, and a batch writes into `rotated/`.
- **Workbooks fit the page.** An xlsx, xls or ods going to PDF has the
  columns widened whose numbers would print as `###`, and a sheet wider
  than its page scaled down to the page's width. A workbook that sets its
  own scale, fit or print range is left alone, and a note says what
  changed.
- **`conv info` shows what a file is.** For video and audio: the container,
  duration, bitrate and each stream's codec, size, frame rate, rotation,
  HDR, sample rate, channels and language, with subtitle and chapter
  counts. For pictures: the size, colour profile, bit depth, transparency,
  frames, orientation tag, camera, date taken and whether a GPS position is
  recorded. For PDFs: pages, page size, encryption and producer. Each file
  also lists what conv can convert it into. `--json` gives the same as
  data. A missing ffprobe or ImageMagick is named, with how to install it,
  next to what could be shown without it.

### Changed

- **`--resize` alone keeps the format.** `conv photo.jpg --resize 50%`
  writes `photo-50pct.jpg`, and `conv clip.mp4 --resize 1280` writes
  `clip-1280w.mp4`, instead of asking for `--to`. A file conv can't resize
  into its own format is refused with the fix.
- **WebM encodes about three times faster.** VP9 now runs at libvpx's
  speed 4 rather than its default of 1. Files come out a little larger at
  the same `--crf`, with no visible change.

### Fixed

- **Xvid and DivX video is re-encoded into mp4 and mov.** It used to be
  copied, and the file played in no browser and not in QuickTime. A note
  says why it was re-encoded; mkv still copies.
- **On Linux, a program conv had just written or unpacked could fail to
  start**, as `conv install` does with a new backend. conv now tries again
  for a moment.

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
