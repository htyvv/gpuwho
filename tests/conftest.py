import json
import os
import sys
import time

import pytest

DEMO = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "gpuwho", "demo.json")

# the agent reads /proc, so anything that actually runs it needs Linux
linux_only = pytest.mark.skipif(not sys.platform.startswith("linux"), reason="agent reads /proc")


class FakeServer:
    """Stands in for gpuwho.remote.Server: same attributes the UI reads, no SSH."""

    def __init__(self, name, host, data=None, error=None, latency=0.4):
        self.name = name
        self.cfg = {"name": name, "host": host}
        self.data = data
        self.error = error
        self.updated = time.time() if data else None
        self.latency = latency
        self.version = 0


def load_cluster():
    with open(DEMO) as f:
        return [FakeServer(s["name"], s["host"], s["data"]) for s in json.load(f)["servers"]]


@pytest.fixture
def cluster():
    return load_cluster()


@pytest.fixture
def conf_dir(tmp_path, monkeypatch):
    """Point gpuwho's config at an empty temp dir (in-process and for subprocesses)."""
    from gpuwho import config

    d = tmp_path / "gpuwho"
    monkeypatch.setattr(config, "CONF_DIR", str(d))
    monkeypatch.setattr(config, "CONF_PATH", str(d / "servers.json"))
    monkeypatch.setattr(config, "KNOWN_HOSTS", str(d / "known_hosts"))
    monkeypatch.setenv("GPUWHO_CONFIG_DIR", str(d))
    return d
