import json
import subprocess
import sys

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
    assert snap["here"]["cpu"]["count"] > 0
    assert "already exists" in gpuwho("add", "here", "local", check=False).stderr
    gpuwho("remove", "here")
    assert "here" not in gpuwho("list").stdout
    assert gpuwho("remove", "here", check=False).returncode != 0
