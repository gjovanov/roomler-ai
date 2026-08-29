#!/usr/bin/env python3
"""Generate the desktop companion's icons from the product's brand mark.

Why this exists: the icons shipped as 1x1 PLACEHOLDERS (67-byte PNGs). Windows
tolerated them; macOS did not — `tao`'s tray setup panics before the app
window ever appears:

    invalid icon: The specified dimensions (1x1) don't match the number of
    pixels supplied by the `rgba` argument (0)

and a panic in that callback "cannot unwind", so the process aborts. The
companion could not start on macOS at all.

Generated rather than hand-drawn so the mark stays in step with
`ui/public/favicon.svg` (a #1565C0 rounded square with a white R) and so
regenerating is a command rather than an afternoon in a paint program.

    python3 scripts/gen-tray-icons.py

Requires Pillow. Writes into agents/roomler-desktop/icons/.
"""

import pathlib
import sys

try:
    from PIL import Image, ImageDraw, ImageFont
except ImportError:  # pragma: no cover - developer tooling
    sys.exit("this needs Pillow:  pip install pillow")

OUT = pathlib.Path(__file__).resolve().parent.parent / "agents/roomler-desktop/icons"
BRAND = (21, 101, 192, 255)  # #1565C0, from favicon.svg
FONT_CANDIDATES = [
    "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
    "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
    "C:/Windows/Fonts/arialbd.ttf",
]

# 4x supersample, then LANCZOS down — the rounded corners and the R's bowl
# both alias badly at 22px otherwise, which is exactly the size the menu bar
# renders at.
SS = 4


def _font(px: int):
    for path in FONT_CANDIDATES:
        if pathlib.Path(path).exists():
            return ImageFont.truetype(path, px)
    return None


def _mark(size: int, fg, bg):
    """The brand mark: rounded square `bg`, letter R in `fg`."""
    n = size * SS
    img = Image.new("RGBA", (n, n), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    # favicon.svg uses rx=6 on a 32px box — keep that ratio at every size.
    d.rounded_rectangle([0, 0, n - 1, n - 1], radius=int(n * 6 / 32), fill=bg)

    font = _font(int(n * 0.66))
    if font is None:
        # No font anywhere: fall back to a ring so the icon is still a valid,
        # recognisable shape rather than a blank square.
        inset = int(n * 0.28)
        d.ellipse([inset, inset, n - inset, n - inset], fill=(0, 0, 0, 0))
    else:
        # `anchor="mm"` centres on the glyph's own box, which is what makes the
        # R sit optically centred rather than baseline-aligned.
        d.text((n / 2, n / 2 - int(n * 0.04)), "R", font=font, fill=fg, anchor="mm")
    return img.resize((size, size), Image.LANCZOS)


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)

    # App icon — full colour.
    _mark(512, (255, 255, 255, 255), BRAND).save(OUT / "icon.png")

    # Menu-bar icon. macOS TEMPLATE images use the alpha channel only and are
    # tinted by the system, so this is black-on-transparent: one asset that is
    # correct in both the light and the dark menu bar. 44px = 22pt @2x.
    tray = _mark(44, (0, 0, 0, 0), (0, 0, 0, 255))
    tray.save(OUT / "tray.png")

    # Windows wants the classic sizes in one file.
    _mark(256, (255, 255, 255, 255), BRAND).save(
        OUT / "icon.ico", sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (256, 256)]
    )

    for f in ("icon.png", "tray.png", "icon.ico"):
        p = OUT / f
        with Image.open(p) as im:
            print(f"  {f}: {im.size[0]}x{im.size[1]} {im.mode}, {p.stat().st_size} bytes")


if __name__ == "__main__":
    main()
