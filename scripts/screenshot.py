"""Render the README images from `gpuwho demo --frames` (made-up servers and users).

    pip install pillow
    python scripts/screenshot.py PATH/TO/gpuwho docs/screenshot.png
    python scripts/screenshot.py PATH/TO/gpuwho docs/demo.gif --gif --width 140 --only node01,node03 --size 20

The binary lays the dashboard out exactly as in a terminal and prints ANSI; each cell is
then painted with Pillow. Box-drawing and block characters are drawn as shapes so they
join up cleanly.
"""
import argparse
import os
import re
import subprocess

from PIL import Image, ImageDraw, ImageFont

BG, FG = (30, 30, 46), (205, 214, 244)  # Catppuccin Mocha
ANSI16 = [(69, 71, 90), (243, 139, 168), (166, 227, 161), (249, 226, 175), (137, 180, 250), (245, 194, 231),
          (148, 226, 213), (186, 194, 222), (88, 91, 112), (243, 139, 168), (166, 227, 161), (249, 226, 175),
          (137, 180, 250), (245, 194, 231), (148, 226, 213), (166, 173, 200)]
FONTS = ["/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf", "/Library/Fonts/DejaVuSansMono.ttf"]
SGR = re.compile(r"\x1b\[([0-9;]*)m")


def xterm(n):
    if n < 16:
        return ANSI16[n]
    if n < 232:
        n -= 16
        steps = [0, 95, 135, 175, 215, 255]
        return steps[n // 36], steps[n // 6 % 6], steps[n % 6]
    v = 8 + (n - 232) * 10
    return v, v, v


def parse(text):
    """ANSI text -> rows of (char, fg, bg, bold, dim, underline)."""
    rows = []
    for line in text.split("\n"):
        row, style = [], dict(fg=None, bg=None, bold=False, dim=False, ul=False)
        pos = 0
        for m in SGR.finditer(line):
            row += [(c, dict(style)) for c in line[pos:m.start()]]
            pos = m.end()
            codes = [int(c) if c else 0 for c in m.group(1).split(";")]
            i = 0
            while i < len(codes):
                c = codes[i]
                if c == 0:
                    style = dict(fg=None, bg=None, bold=False, dim=False, ul=False)
                elif c == 1:
                    style["bold"] = True
                elif c == 2:
                    style["dim"] = True
                elif c == 4:
                    style["ul"] = True
                elif 30 <= c <= 37 or 90 <= c <= 97:
                    style["fg"] = ANSI16[c - 30 if c < 90 else c - 82]
                elif 40 <= c <= 47 or 100 <= c <= 107:
                    style["bg"] = ANSI16[c - 40 if c < 100 else c - 92]
                elif c in (38, 48) and i + 2 < len(codes) and codes[i + 1] == 5:
                    style["fg" if c == 38 else "bg"] = xterm(codes[i + 2])
                    i += 2
                elif c in (38, 48) and i + 4 < len(codes) and codes[i + 1] == 2:
                    style["fg" if c == 38 else "bg"] = tuple(codes[i + 2:i + 5])
                    i += 4
                i += 1
        row += [(c, dict(style)) for c in line[pos:]]
        rows.append(row)
    while rows and not rows[-1]:
        rows.pop()
    return rows


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

    def paint(self, rows, cols):
        cw, ch, pad, bar = self.cw, self.ch, self.pad, self.bar
        img = Image.new("RGB", (int(cw * cols + 2 * pad), int(ch * len(rows) + 2 * pad + bar)), BG)
        d = ImageDraw.Draw(img)
        dot = int(ch * 0.6)
        for i, color in enumerate([(243, 139, 168), (249, 226, 175), (166, 227, 161)]):
            d.ellipse((pad + i * dot * 2, bar // 3, pad + i * dot * 2 + dot, bar // 3 + dot), fill=color)
        for r, row in enumerate(rows):
            y = pad + bar + r * ch
            for c_i, (c, st) in enumerate(row):
                fg = st["fg"] or FG
                if st["dim"]:
                    fg = blend(fg, BG, 0.45)
                self.cell(d, c, pad + c_i * cw, y, fg, st["bg"], self.bold if st["bold"] else self.regular, st["ul"])
        return img

    def cell(self, d, c, x, y, fg, bg, font, ul):
        cw, ch, stroke = self.cw, self.ch, self.stroke
        x0, x1, y1 = x, x + cw, y + ch
        xm, ym = round(x + cw / 2), round(y + ch / 2)
        if bg:
            d.rectangle((x0, y, x1, y1), fill=bg)
        if c == "█":
            d.rectangle((x0, y + 3, x1, y1 - 3), fill=fg)
        elif c == "░":
            d.rectangle((x0, y + 3, x1, y1 - 3), fill=blend(fg, BG, 0.55))
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
        if ul and c != " ":
            d.line((x0, y1 - 3, x1, y1 - 3), fill=fg, width=stroke)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("binary")
    ap.add_argument("out")
    ap.add_argument("--width", type=int, default=236)
    ap.add_argument("--only", default="")
    ap.add_argument("--gif", action="store_true", help="16 frames: jobs finish and start")
    ap.add_argument("--font", default=next((f for f in FONTS if os.path.exists(f)), None))
    ap.add_argument("--size", type=int, default=26)
    args = ap.parse_args()

    n = 16 if args.gif else 1
    cmd = [args.binary, "demo", "--frames", str(n), "--width", str(args.width)]
    if args.only:
        cmd += ["--only", args.only]
    out = subprocess.run(cmd, check=True, capture_output=True, text=True).stdout
    frames = [parse(f.lstrip("\n")) for f in out.split("\x0c")]
    painter = Painter(args.font, args.size)
    if not args.gif:
        img = painter.paint(frames[0], args.width)
        img.save(args.out, optimize=True)
        print("wrote %s (%dx%d)" % (args.out, img.width, img.height))
        return
    rows = max(len(f) for f in frames)
    imgs = [painter.paint(f + [[]] * (rows - len(f)), args.width).convert("P", palette=Image.ADAPTIVE, colors=64)
            for f in frames]
    imgs[0].save(args.out, save_all=True, append_images=imgs[1:], duration=[1200] * 15 + [2500], loop=0, optimize=True)
    print("wrote %s (%dx%d, %d frames, %d KB)" % (args.out, imgs[0].width, imgs[0].height, len(imgs),
                                                  os.path.getsize(args.out) // 1024))


if __name__ == "__main__":
    main()
