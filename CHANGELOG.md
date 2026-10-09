# Changelog

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
