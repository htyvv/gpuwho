import json
import os
import select
import signal
import subprocess
import sys
import time

from gpuwho import __version__
from tests.conftest import linux_only


def gpuwho(*args, check=True):
    """Run the real CLI in a subprocess (conf_dir sets GPUWHO_CONFIG_DIR for it)."""
    return subprocess.run([sys.executable, "-m", "gpuwho", *args], capture_output=True, text=True,
                          check=check, timeout=60)


def test_version(conf_dir):
    assert gpuwho("--version").stdout.strip() == "gpuwho " + __version__


def test_no_servers_yet(conf_dir):
    out = gpuwho("once", check=False)
    assert out.returncode != 0 and "gpuwho add" in out.stderr
    assert "Name" in gpuwho("list").stdout


@linux_only
def test_add_once_remove(conf_dir):
    assert "ok" in gpuwho("add", "here", "local").stdout
    assert "here" in gpuwho("list").stdout
    snap = json.loads(gpuwho("once", "--json").stdout)
    assert snap["schema"] == 1 and snap["version"] == __version__
    [here] = snap["servers"]
    assert here["name"] == "here" and here["ok"] and here["cpu"]["count"] > 0
    assert "already exists" in gpuwho("add", "here", "local", check=False).stderr
    gpuwho("remove", "here")
    assert "here" not in gpuwho("list").stdout
    assert gpuwho("remove", "here", check=False).returncode != 0


def test_flags():
    from gpuwho.cli import build_parser
    args = build_parser().parse_args(["-p", "-c", "once"])
    assert args.procs and args.compact  # the subcommand doesn't reset global flags
    assert gpuwho("-i", "0", "once", check=False).returncode == 2


def test_unknown_server_name(conf_dir):
    gpuwho("add", "a", "10.0.0.1", "--no-test")
    out = gpuwho("once", "a", "typo", check=False)
    assert out.returncode == 1 and "unknown server: typo" in out.stderr


def test_add_stores_only_what_was_typed(conf_dir):
    gpuwho("add", "g1", "alice@10.0.0.1:2222", "--no-test")
    gpuwho("add", "g2", "--no-test")
    conf = json.loads((conf_dir / "servers.json").read_text())
    assert conf["servers"] == [{"name": "g1", "host": "10.0.0.1", "user": "alice", "port": 2222},
                               {"name": "g2", "host": "g2"}]


@linux_only
def test_exit_codes_and_free(conf_dir):
    gpuwho("add", "here", "local")
    gpuwho("add", "down", "127.0.0.2:1", "--no-test")
    assert gpuwho("test", check=False).returncode == 1
    assert gpuwho("once", check=False).returncode == 0  # one server answered
    assert gpuwho("once", "--strict", check=False).returncode == 1
    free = gpuwho("free", "here", "--json", check=False)  # CI machines have no GPUs
    assert json.loads(free.stdout) == [] and free.returncode == 1


@linux_only
def test_not_a_terminal_prints_once(conf_dir):
    gpuwho("add", "here", "local")
    out = gpuwho()
    assert "here" in out.stdout and "not a terminal" in out.stderr


@linux_only
def test_live_demo_starts_and_quits():
    """The real thing in a pseudo-terminal: draws, quits on q, gives the terminal back."""
    import pty
    pid, fd = pty.fork()
    if pid == 0:
        os.execvp(sys.executable, [sys.executable, "-m", "gpuwho", "demo"])
    out, deadline = b"", time.time() + 20
    while b"node01" not in out and time.time() < deadline:
        if select.select([fd], [], [], 0.2)[0]:
            out += os.read(fd, 65536)
    assert b"node01" in out
    os.write(fd, b"q")
    deadline = time.time() + 10
    while time.time() < deadline:
        done, status = os.waitpid(pid, os.WNOHANG)
        if done:
            break
        if select.select([fd], [], [], 0.2)[0]:
            try:
                out += os.read(fd, 65536)
            except OSError:
                pass
    else:
        os.kill(pid, signal.SIGKILL)
        raise AssertionError("did not quit")
    assert os.waitstatus_to_exitcode(status) == 0 if hasattr(os, "waitstatus_to_exitcode") else status == 0
    assert b"\x1b[?1049l" in out  # left the alternate screen


def test_demo_without_a_terminal():
    out = subprocess.run([sys.executable, "-m", "gpuwho", "demo"], capture_output=True, text=True, timeout=60)
    assert out.returncode == 0 and "node01" in out.stdout and "Users across all servers" in out.stdout
