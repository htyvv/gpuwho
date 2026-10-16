"""Render the README screenshot from tests/fixtures/cluster.json (made-up servers and users).

    pip install pillow
    python scripts/screenshot.py docs/screenshot.png [--width 236] [--procs]

rich lays out the dashboard exactly as in a terminal; each cell is then painted with
Pillow. Box-drawing and block characters are drawn as shapes so they join up cleanly.
"""
import argparse
import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)

from PIL import Image, ImageDraw, ImageFont  # noqa: E402
from rich.console import Console  # noqa: E402
from rich.terminal_theme import TerminalTheme  # noqa: E402

from gpuwho import ui  # noqa: E402
from tests.conftest import load_cluster  # noqa: E402

# Catppuccin Mocha
THEME = TerminalTheme(
    (30, 30, 46), (205, 214, 244),
    [(69, 71, 90), (243, 139, 168), (166, 227, 161), (249, 226, 175),
     (137, 180, 250), (245, 194, 231), (148, 226, 213), (186, 194, 222)],
    [(88, 91, 112), (243, 139, 168), (166, 227, 161), (249, 226, 175),
     (137, 180, 250), (245, 194, 231), (148, 226, 213), (166, 173, 200)],
)
FONTS = ["/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf", "/Library/Fonts/DejaVuSansMono.ttf"]


def blend(a, b, t):
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("--width", type=int, default=236)
    ap.add_argument("--procs", action="store_true")
    ap.add_argument("--font", default=next((f for f in FONTS if os.path.exists(f)), None))
    ap.add_argument("--size", type=int, default=26)
    args = ap.parse_args()

    console = Console(width=args.width, force_terminal=True, color_system="truecolor", file=open(os.devnull, "w"))
    renderable = ui.dashboard(load_cluster(), 3, args.procs, args.width)
    lines = console.render_lines(renderable, console.options, pad=True)

    regular = ImageFont.truetype(args.font, args.size)
    bold = ImageFont.truetype(args.font.replace(".ttf", "-Bold.ttf"), args.size)
    cw = regular.getlength("M")
    ascent, descent = regular.getmetrics()
    ch = ascent + descent
    pad, bar = 36, 56
    W, H = int(cw * args.width + 2 * pad), int(ch * len(lines) + 2 * pad + bar)
    img = Image.new("RGB", (W, H), THEME.background_color)
    d = ImageDraw.Draw(img)
    for i, color in enumerate([(243, 139, 168), (249, 226, 175), (166, 227, 161)]):
        d.ellipse((pad + i * 34, 22, pad + i * 34 + 18, 40), fill=color)
    stroke = max(2, args.size // 12)

    for row, segments in enumerate(lines):
        x, y = pad, pad + bar + row * ch
        for seg in segments:
            style = seg.style
            fg, bg = THEME.foreground_color, None
            if style and style.color:
                fg = tuple(style.color.get_truecolor(THEME))
            if style and style.bgcolor:
                bg = tuple(style.bgcolor.get_truecolor(THEME, foreground=False))
            fg = tuple(fg)
            if style and style.dim:
                fg = blend(fg, THEME.background_color, 0.45)
            font = bold if style and style.bold else regular
            for c in seg.text:
                x0, x1, y1 = x, x + cw, y + ch
                xm, ym = round(x + cw / 2), round(y + ch / 2)
                if bg:
                    d.rectangle((x0, y, x1, y1), fill=bg)
                if c == "█":
                    d.rectangle((x0, y + 3, x1, y1 - 3), fill=fg)
                elif c == "░":
                    d.rectangle((x0, y + 3, x1, y1 - 3), fill=blend(fg, THEME.background_color, 0.55))
                elif c == "─":
                    d.line((x0, ym, x1, ym), fill=fg, width=stroke)
                elif c == "│":
                    d.line((xm, y, xm, y1), fill=fg, width=stroke)
                elif c in "╭╮╰╯":
                    r = cw / 2
                    box = {"╭": (xm, ym, xm + 2 * r, ym + 2 * r, 180, 270),
                           "╮": (xm - 2 * r, ym, xm, ym + 2 * r, 270, 360),
                           "╰": (xm, ym - 2 * r, xm + 2 * r, ym, 90, 180),
                           "╯": (xm - 2 * r, ym - 2 * r, xm, ym, 0, 90)}[c]
                    d.arc(box[:4], box[4], box[5], fill=fg, width=stroke)
                    if c in "╭╮":
                        d.line((xm, ym + r, xm, y1), fill=fg, width=stroke)
                    else:
                        d.line((xm, y, xm, ym - r), fill=fg, width=stroke)
                    if c in "╭╰":
                        d.line((xm + r, ym, x1, ym), fill=fg, width=stroke)
                    else:
                        d.line((x0, ym, xm - r, ym), fill=fg, width=stroke)
                elif c != " ":
                    d.text((x0, y), c, font=font, fill=fg)
                if style and style.underline and c != " ":
                    d.line((x0, y1 - 3, x1, y1 - 3), fill=fg, width=stroke)
                x += cw
    img.save(args.out, optimize=True)
    print("wrote %s (%dx%d)" % (args.out, W, H))


if __name__ == "__main__":
    main()
