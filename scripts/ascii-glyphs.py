"""Draws the glyph atlas `conv ascii` uses to write PNGs.

    python3 scripts/ascii-glyphs.py

Needs Pillow. Downloads JetBrains Mono (SIL Open Font License 1.1), checks
the download against a pinned SHA-256, and draws the printable ASCII
characters `!` to `~` one per 18x36 cell, centred, into
crates/convkit-core/src/ascii/glyphs.png: a single greyscale row where
each pixel is how much ink covers it. The font's licence goes beside it.

Run it again only to change the font or the cell size; the atlas is
committed, so building conv never needs it.
"""

import hashlib
import io
import sys
import tempfile
import urllib.request
import zipfile
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

VERSION = "2.304"
URL = f"https://github.com/JetBrains/JetBrainsMono/releases/download/v{VERSION}/JetBrainsMono-{VERSION}.zip"
EXPECTED_SHA256 = "6f6376c6ed2960ea8a963cd7387ec9d76e3f629125bc33d1fdcd7eb7012f7bbf"
CELL_W, CELL_H = 18, 36
OUT = Path(__file__).resolve().parent.parent / "crates/convkit-core/src/ascii"


def fetch() -> bytes:
    with urllib.request.urlopen(URL) as r:
        data = r.read()
    digest = hashlib.sha256(data).hexdigest()
    if digest != EXPECTED_SHA256:
        sys.exit(f"{URL} has SHA-256 {digest}, expected {EXPECTED_SHA256}")
    return data


def font_size(path: str) -> int:
    """The largest pixel size whose advance fits the cell's width and whose
    ascent plus descent fits its height."""
    size = CELL_H
    while size > 1:
        font = ImageFont.truetype(path, size)
        ascent, descent = font.getmetrics()
        if font.getlength("M") <= CELL_W and ascent + descent <= CELL_H:
            return size
        size -= 1
    sys.exit("no size fits the cell")


def main() -> None:
    data = fetch()
    with zipfile.ZipFile(io.BytesIO(data)) as z, tempfile.TemporaryDirectory() as tmp:
        ttf = Path(tmp) / "JetBrainsMono-Regular.ttf"
        ttf.write_bytes(z.read("fonts/ttf/JetBrainsMono-Regular.ttf"))
        licence = z.read("OFL.txt").decode("utf-8")
        size = font_size(str(ttf))
        font = ImageFont.truetype(str(ttf), size)
        ascent, descent = font.getmetrics()
        baseline = (CELL_H - (ascent + descent)) // 2 + ascent
        chars = [chr(c) for c in range(0x21, 0x7F)]
        atlas = Image.new("L", (CELL_W * len(chars), CELL_H), 0)
        draw = ImageDraw.Draw(atlas)
        for i, ch in enumerate(chars):
            x = i * CELL_W + (CELL_W - font.getlength(ch)) / 2
            draw.text((x, baseline), ch, font=font, fill=255, anchor="ls")
    atlas.save(OUT / "glyphs.png", optimize=True)
    note = (
        f"glyphs.png is drawn from JetBrains Mono {VERSION} Regular at {size} px "
        f"by scripts/ascii-glyphs.py, and is covered by the font's licence below.\n\n"
    )
    (OUT / "JetBrainsMono-OFL.txt").write_text(note + licence)
    print(f"{len(chars)} glyphs at {size} px into {OUT / 'glyphs.png'}")


if __name__ == "__main__":
    main()
