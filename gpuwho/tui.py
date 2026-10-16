"""The live view: polls every server on its own thread, redraws only when something changed,
and handles the keyboard (scrolling, compact view, retry, pause, help) and job control.

The body is rendered to lines once per frame and only the slice that fits is drawn, so
nothing is silently cut off: with too many servers it switches to the compact view by
itself, and whatever still doesn't fit can be scrolled.
"""
import os
import select
import signal
import sys
import threading
import time

from rich import box
from rich.console import Group
from rich.live import Live
from rich.panel import Panel
from rich.segment import Segment
from rich.table import Table

from gpuwho import ui

KEYS = {
    b"\x1b[A": "up", b"\x1bOA": "up", b"\x1b[B": "down", b"\x1bOB": "down",
    b"\x1b[5~": "pgup", b"\x1b[6~": "pgdn",
    b"\x1b[H": "home", b"\x1bOH": "home", b"\x1b[1~": "home",
    b"\x1b[F": "end", b"\x1bOF": "end", b"\x1b[4~": "end",
}

HELP = [
    ("q", "quit"),
    ("p", "show / hide the process table"),
    ("c", "compact (one line per server) / full view"),
    ("r", "retry failed servers now"),
    ("space", "pause / resume"),
    ("j k ↓ ↑", "scroll"),
    ("PgDn PgUp", "scroll a page"),
    ("g G", "top / bottom"),
    ("? h", "this help"),
]


def parse_keys(data):
    """Raw bytes from the terminal -> key names. Everything that arrived is handled, and
    escape sequences we don't know are skipped whole instead of leaking letters."""
    keys, i = [], 0
    while i < len(data):
        if data[i:i + 1] != b"\x1b":
            keys.append(chr(data[i]) if data[i] < 128 else "?")
            i += 1
            continue
        for seq, name in KEYS.items():
            if data.startswith(seq, i):
                keys.append(name)
                i += len(seq)
                break
        else:
            j = i + 1
            if data[j:j + 1] in (b"[", b"O"):
                j += 1
                while j < len(data) and not 0x40 <= data[j] <= 0x7E:
                    j += 1
                j += 1
            else:
                keys.append("esc")
            i = j
    return keys


class Lines:
    """Pre-rendered lines as a renderable."""

    def __init__(self, lines):
        self.lines = lines

    def __rich_console__(self, console, options):
        for line in self.lines:
            yield from line
            yield Segment.line()


class LiveView:
    def __init__(self, servers, interval, procs=False, compact=None, console=None):
        self.servers = servers
        self.interval = interval
        self.procs = procs
        self.compact = compact      # None: automatic, compact only when the full view doesn't fit
        self.showing_compact = False
        self.offset = 0
        self.page = 10
        self.paused = False
        self.help = False
        self.stop = threading.Event()
        self.kicks = {id(s): threading.Event() for s in servers}
        self.want_suspend = False
        self.console = console
        self.live = None

    # -- polling

    def poll(self, s, delay):
        if self.stop.wait(delay):  # stagger the first connections
            return
        kick = self.kicks[id(s)]
        while not self.stop.is_set():
            if not self.paused and s.due():
                s.probe()
            kick.wait(self.interval)
            kick.clear()

    def retry_all(self):
        for s in self.servers:
            s.retry()
        for kick in self.kicks.values():
            kick.set()

    # -- drawing

    def _lines(self, renderable, width):
        options = self.console.options.update(width=width, height=None)
        return self.console.render_lines(renderable, options, pad=False)

    def frame(self, width, height):
        rows = max(1, height - 1)
        if self.help:
            t = Table(box=box.SIMPLE, show_header=False, pad_edge=False)
            t.add_column(style="bold cyan", no_wrap=True)
            t.add_column()
            for k, what in HELP:
                t.add_row(k, what)
            body = [Panel(t, title="keys", title_align="left", border_style="cyan", width=min(width, 60))]
            return Group(self._head(width, 0, 0), *body)
        lines = None
        if self.compact is not True:
            lines = self._lines(ui.body(self.servers, self.interval, self.procs, width, False), width)
            if self.compact is None and len(lines) > rows:
                lines = None
        self.showing_compact = lines is None
        if lines is None:
            lines = self._lines(ui.body(self.servers, self.interval, self.procs, width, True), width)
        self.page = max(1, rows - 1)
        self.offset = max(0, min(self.offset, len(lines) - rows))
        shown = lines[self.offset:self.offset + rows]
        return Group(self._head(width, self.offset, len(lines) - self.offset - len(shown)), Lines(shown))

    def _head(self, width, above, below):
        t = ui.header(self.servers, self.interval, short=width < 120)
        if self.paused:
            t.append("  PAUSED", style="bold yellow")
        hint = "  q quit  p procs  c %s  ? help" % ("full" if self.showing_compact else "compact")
        if above or below:
            hint = "  %s%s" % ("↑%d " % above if above else "", "↓%d" % below if below else "") + hint
        t.append(hint, style="dim")
        t.truncate(width, overflow="ellipsis")
        return t

    # -- keys

    def handle(self, key):
        if self.help:  # any key closes the help, q still quits
            self.help = False
            return "quit" if key in ("q", "Q") else None
        if key in ("q", "Q"):
            return "quit"
        if key == "p":
            self.procs = not self.procs
        elif key == "c":
            self.compact = not self.showing_compact
            self.offset = 0
        elif key == "r":
            self.retry_all()
        elif key == " ":
            self.paused = not self.paused
        elif key in ("?", "h"):
            self.help = True
        elif key in ("j", "down"):
            self.offset += 1
        elif key in ("k", "up"):
            self.offset = max(0, self.offset - 1)
        elif key == "pgdn":
            self.offset += self.page
        elif key == "pgup":
            self.offset = max(0, self.offset - self.page)
        elif key in ("g", "home"):
            self.offset = 0
        elif key in ("G", "end"):
            self.offset = 10 ** 9
        return None

    # -- job control

    def _on_tstp(self, signum, frame):
        self.want_suspend = True

    def _suspend(self, fd, saved):
        """Ctrl-Z: leave the alternate screen and give the terminal back, stop, then resume."""
        import termios
        import tty
        self.want_suspend = False
        self.live.stop()
        if saved is not None:
            termios.tcsetattr(fd, termios.TCSADRAIN, saved)
        signal.signal(signal.SIGTSTP, signal.SIG_DFL)
        os.kill(os.getpid(), signal.SIGTSTP)  # stopped here until `fg`
        signal.signal(signal.SIGTSTP, self._on_tstp)
        if saved is not None:
            tty.setcbreak(fd)
        self.live.start()

    def run(self):
        for i, s in enumerate(self.servers):
            threading.Thread(target=self.poll, args=(s, i * 0.05), daemon=True).start()

        fd = sys.stdin.fileno() if sys.stdin.isatty() else None
        saved = None
        if fd is not None:
            import termios
            import tty
            saved = termios.tcgetattr(fd)
            tty.setcbreak(fd)

        def on_term(signum, frame):
            raise KeyboardInterrupt

        old_handlers = {sig: signal.signal(sig, handler) for sig, handler in
                        ((signal.SIGTERM, on_term), (signal.SIGTSTP, self._on_tstp))}
        con = self.console
        try:
            with Live(console=con, screen=True, auto_refresh=False) as live:
                self.live = live
                drawn, drawn_at, size = None, 0.0, None
                while not self.stop.is_set():
                    forced = False
                    if self.want_suspend:
                        self._suspend(fd, saved)
                        forced = True
                    if fd is not None and select.select([fd], [], [], 0.2)[0]:
                        for key in parse_keys(os.read(fd, 64)):
                            if self.handle(key) == "quit":
                                return
                        forced = True
                    elif fd is None:
                        time.sleep(0.2)
                    now_size = (con.width, con.height)
                    state = sum(s.version for s in self.servers)
                    since = time.time() - drawn_at
                    if forced or now_size != size or (not self.paused and (
                            (state != drawn and since >= 1.0) or since >= 10)):
                        live.update(self.frame(*now_size), refresh=True)
                        drawn, drawn_at, size = state, time.time(), now_size
        except KeyboardInterrupt:
            pass
        finally:
            self.stop.set()
            for kick in self.kicks.values():
                kick.set()
            for sig, handler in old_handlers.items():
                signal.signal(sig, handler)
            if saved is not None:
                import termios
                termios.tcsetattr(fd, termios.TCSADRAIN, saved)
            for s in self.servers:
                s.close()
