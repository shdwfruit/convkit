# Changelog

## Unreleased

### New

- **`conv merge` and `conv split` for PDFs.** `conv merge a.pdf b.pdf out.pdf`
  joins PDFs in order; a folder adds every PDF in it. `conv split report.pdf`
  writes one file per page, and `conv split report.pdf 1-3 4-z` one per range
  (`z` is the last page, `5-1` reverses). Pages no range covers are left out
  with a warning. Both run on qpdf, so text and links come through untouched.
  The merged file keeps the first file's bookmarks only, and permission
  restrictions are dropped; conv notes both when they apply.
- **qpdf is a managed backend.** `conv install qpdf` downloads qpdf 12.4.2 for
  all five platforms, checks its SHA-256, and installs it as a folder. The
  Linux build needs glibc 2.34 or newer. `--qpdf-path` and `CONVKIT_QPDF`
  point conv at another copy.

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
