"""Render the README images from gpuwho/demo.json (made-up servers and users).

    pip install pillow
    python scripts/screenshot.py docs/screenshot.png                  # all servers, 2 columns
    python scripts/screenshot.py docs/demo.gif --width 140 --gif      # live demo, 2 servers

rich lays the dashboard out exactly as in a terminal; each cell is then painted with
Pillow. Box-drawing and block characters are drawn as shapes so they join up cleanly.
"""
import argparse
import os
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)

from PIL import Image, ImageDraw, ImageFont  # noqa: E402
from rich.console import Console  # noqa: E402
from rich.terminal_theme import TerminalTheme  # noqa: E402

from gpuwho import demo, ui  # noqa: E402

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


class Painter:
    def __init__(self, font, size):
        self.regular = ImageFont.truetype(font, size)
        self.bold = ImageFont.truetype(font.replace(".ttf", "-Bold.ttf"), size)
        self.cw = self.regular.getlength("M")
        ascent, descent = self.regular.getmetrics()
        self.ch = ascent + descent
        self.stroke = max(2, size // 12)
        self.pad, self.bar = int(size * 1.4), int(size * 2.1)

    def paint(self, lines, cols):
        cw, ch, pad, bar = self.cw, self.ch, self.pad, self.bar
        bg0 = THEME.background_color
        img = Image.new("RGB", (int(cw * cols + 2 * pad), int(ch * len(lines) + 2 * pad + bar)), bg0)
        d = ImageDraw.Draw(img)
        dot = int(self.ch * 0.6)
        for i, color in enumerate([(243, 139, 168), (249, 226, 175), (166, 227, 161)]):
            d.ellipse((pad + i * dot * 2, bar // 3, pad + i * dot * 2 + dot, bar // 3 + dot), fill=color)
        for row, segments in enumerate(lines):
            x, y = pad, pad + bar + row * ch
            for seg in segments:
                style = seg.style
                fg = tuple(style.color.get_truecolor(THEME)) if style and style.color else THEME.foreground_color
                bg = tuple(style.bgcolor.get_truecolor(THEME, foreground=False)) if style and style.bgcolor else None
                if style and style.dim:
                    fg = blend(fg, bg0, 0.45)
                font = self.bold if style and style.bold else self.regular
                for c in seg.text:
                    self.cell(d, c, x, y, fg, bg, font, style)
                    x += cw
        return img

    def cell(self, d, c, x, y, fg, bg, font, style):
        cw, ch, stroke = self.cw, self.ch, self.stroke
        x0, x1, y1 = x, x + cw, y + ch
        xm, ym = round(x + cw / 2), round(y + ch / 2)
        if bg:
            d.rectangle((x0, y, x1, y1), fill=bg)
        if c == "█":
            d.rectangle((x0, y + 3, x1, y1 - 3), fill=fg)
        elif c == "░":
            d.rectangle((x0, y + 3, x1, y1 - 3), fill=blend(fg, THEME.background_color, 0.55))
        elif c in "▁▂▃▄▅▆▇":
            h = (ch - 6) * ("▁▂▃▄▅▆▇".index(c) + 1) / 8
            d.rectangle((x0 + 1, y1 - 3 - h, x1 - 1, y1 - 3), fill=fg)
        elif c == "─":
            d.line((x0, ym, x1, ym), fill=fg, width=stroke)
        elif c == "│":
            d.line((xm, y, xm, y1), fill=fg, width=stroke)
        elif c in "╭╮╰╯":
            r = cw / 2
            box = {"╭": (xm, ym, xm + 2 * r, ym + 2 * r, 180, 270), "╮": (xm - 2 * r, ym, xm, ym + 2 * r, 270, 360),
                   "╰": (xm, ym - 2 * r, xm + 2 * r, ym, 90, 180), "╯": (xm - 2 * r, ym - 2 * r, xm, ym, 0, 90)}[c]
            d.arc(box[:4], box[4], box[5], fill=fg, width=stroke)
            d.line((xm, ym + r, xm, y1) if c in "╭╮" else (xm, y, xm, ym - r), fill=fg, width=stroke)
            d.line((xm + r, ym, x1, ym) if c in "╭╰" else (x0, ym, xm - r, ym), fill=fg, width=stroke)
        elif c != " ":
            d.text((x0, y), c, font=font, fill=fg)
        if style and style.underline and c != " ":
            d.line((x0, y1 - 3, x1, y1 - 3), fill=fg, width=stroke)


def lines_for(servers, width, procs, compact=False):
    console = Console(width=width, force_terminal=True, color_system="truecolor", file=open(os.devnull, "w"))
    renderable = ui.dashboard(servers, 3, procs, width, compact)
    return console.render_lines(renderable, console.options.update(height=None), pad=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("--width", type=int, default=236)
    ap.add_argument("--procs", action="store_true")
    ap.add_argument("--gif", action="store_true", help="animate the demo: jobs finish and start")
    ap.add_argument("--servers", default="", help="comma-separated subset, e.g. node01,node03")
    ap.add_argument("--font", default=next((f for f in FONTS if os.path.exists(f)), None))
    ap.add_argument("--size", type=int, default=26)
    args = ap.parse_args()

    servers = demo.servers()
    if args.servers:
        servers = [s for s in servers if s.name in args.servers.split(",")]
    painter = Painter(args.font, args.size)
    if not args.gif:
        for s in servers:  # as if just polled, without the demo's random walk
            s.updated, s.latency = time.time(), 0.4
        img = painter.paint(lines_for(servers, args.width, args.procs), args.width)
        img.save(args.out, optimize=True)
        print("wrote %s (%dx%d)" % (args.out, img.width, img.height))
        return

    frames, rows = [], 0
    for _ in range(16):  # one demo cycle: a job finishes, two start, one finishes
        for s in servers:
            s.probe()
        lines = lines_for(servers, args.width, args.procs)
        rows = max(rows, len(lines))
        frames.append(lines)
    imgs = [painter.paint(f + [[]] * (rows - len(f)), args.width).convert("P", palette=Image.ADAPTIVE, colors=64)
            for f in frames]
    durations = [1200] * 15 + [2500]
    imgs[0].save(args.out, save_all=True, append_images=imgs[1:], duration=durations, loop=0, optimize=True)
    print("wrote %s (%dx%d, %d frames, %d KB)" % (args.out, imgs[0].width, imgs[0].height, len(imgs),
                                                  os.path.getsize(args.out) // 1024))


if __name__ == "__main__":
    main()
