import io

from rich.console import Console

from gpuwho import demo, tui


def test_parse_keys():
    assert tui.parse_keys(b"jk\x1b[A\x1b[6~q") == ["j", "k", "up", "pgdn", "q"]
    assert tui.parse_keys(b"\x1b[15~x") == ["x"]  # unknown sequence skipped whole
    assert tui.parse_keys(b"\x1b") == ["esc"]


def view(width=120, height=30):
    console = Console(width=width, height=height, file=io.StringIO(), color_system=None)
    servers = demo.servers()
    for s in servers:
        s.probe()
    return tui.LiveView(servers, 3, console=console), console


def text(renderable, console):
    console.print(renderable)
    return console.file.getvalue()


def test_too_tall_switches_to_compact():
    v, con = view(120, 30)
    out = text(v.frame(120, 30), con)
    assert v.showing_compact and "c full" in out and len(out.splitlines()) <= 30


def test_tall_terminal_shows_panels():
    v, con = view(240, 200)
    out = text(v.frame(240, 200), con)
    assert not v.showing_compact and "Users (GPU mem)" in out


def test_keys():
    v, con = view(120, 20)
    v.frame(120, 20)
    v.compact = False
    v.frame(120, 20)
    v.handle("G")
    v.frame(120, 20)
    assert v.offset > 0
    v.handle("g")
    assert v.offset == 0
    v.handle("c")
    assert v.compact is True
    v.handle(" ")
    assert v.paused
    v.handle("?")
    assert "this help" in text(v.frame(120, 20), con)
    assert v.handle("x") is None and not v.help  # any key closes the help
    assert v.handle("q") == "quit"
