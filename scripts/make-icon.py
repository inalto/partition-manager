"""Builds the application icon from the logo and writes it where the build expects it.

    python scripts/make-icon.py        (needs Pillow)

Reads assets/logo.png (the logo on a plain, near-white background), cuts the background
away and writes, under crates/pm-gui/assets/:
  partition-manager.ico   16 to 256 px: embedded in the executable, used by the installer
                          and the shortcuts
  icon-256.rgba           256x256 raw RGBA: the window and taskbar icon and the About window

Run it again whenever assets/logo.png changes, and commit the two files it writes.
"""
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw, ImageFilter

ROOT = Path(__file__).resolve().parent.parent
LOGO = ROOT / "assets" / "logo.png"
ASSETS = ROOT / "crates" / "pm-gui" / "assets"

# How far from the background colour a pixel may be and still be background, as the sum of
# the differences of its three channels. Kept small: the logo has off-white parts (the
# border of the shield) that touch the background and must stay.
TOLERANCE = 9
KEY = (255, 0, 255)


def cut_out(logo: Image.Image) -> Image.Image:
    """`logo` with the background that is connected to its corners made transparent."""
    rgb = logo.convert("RGB")
    w, h = rgb.size
    work = rgb.copy()
    for corner in [(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)]:
        if work.getpixel(corner) != KEY:
            ImageDraw.floodfill(work, corner, KEY, thresh=TOLERANCE)
    r, g, b = work.split()
    background = ImageChops.multiply(
        ImageChops.multiply(r.point(lambda v: 255 if v == 255 else 0), g.point(lambda v: 255 if v == 0 else 0)),
        b.point(lambda v: 255 if v == 255 else 0),
    )
    # one pixel in, then softened: no white fringe, no jagged edge
    alpha = ImageChops.invert(background).filter(ImageFilter.MinFilter(3)).filter(ImageFilter.GaussianBlur(1.0))
    out = rgb.convert("RGBA")
    out.putalpha(alpha)
    return out


def squared(img: Image.Image) -> Image.Image:
    """`img` cropped to what is visible, centred on a transparent square with a small margin."""
    box = img.getchannel("A").point(lambda v: 255 if v > 8 else 0).getbbox()
    img = img.crop(box)
    side = round(max(img.size) * 1.04)
    out = Image.new("RGBA", (side, side), (0, 0, 0, 0))
    out.alpha_composite(img, ((side - img.width) // 2, (side - img.height) // 2))
    return out


def main() -> None:
    art = squared(cut_out(Image.open(LOGO)))
    ASSETS.mkdir(parents=True, exist_ok=True)
    sizes = [16, 24, 32, 48, 64, 128, 256]
    frames = [art.resize((n, n), Image.LANCZOS) for n in sizes]
    frames[-1].save(ASSETS / "partition-manager.ico", format="ICO", append_images=frames[:-1],
                    sizes=[(n, n) for n in sizes])
    (ASSETS / "icon-256.rgba").write_bytes(frames[-1].tobytes())
    stale = ASSETS / "icon-128.rgba"
    if stale.exists():
        stale.unlink()
    print("written to", ASSETS)


if __name__ == "__main__":
    main()
