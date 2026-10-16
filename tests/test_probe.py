import json
import subprocess
import sys

import pytest

from gpuwho import probe
from gpuwho.remote import PROBE
from tests.conftest import linux_only


@pytest.mark.parametrize("cgroup, expected", [
    ("12:pids:/docker/3f9c2a1b4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8", "3f9c2a1b4d5e"),
    ("0::/system.slice/docker-3f9c2a1b4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8.scope",
     "3f9c2a1b4d5e"),
    ("0::/kubepods/besteffort/pod1/cri-containerd-0123456789abcdef0123456789abcdef.scope", "0123456789ab"),
    ("0::/machine.slice/libpod-abcdef012345abcdef012345abcdef012345.scope", "abcdef012345"),
    ("0::/kubepods.slice/crio-fedcba987654fedcba987654fedcba987654.scope", "fedcba987654"),
    ("0::/user.slice/user-1000.slice/session-3.scope", None),
    ("", None),
])
def test_container_id(cgroup, expected):
    assert probe.container_id(cgroup) == expected


def test_merge_procs_types_like_nvidia_smi():
    procs = probe.merge_procs([(10, 100.0), (11, 50.0)], [(11, 1.0), (12, 4.0)])
    assert [(p["pid"], p["type"], p["mem"]) for p in procs] == [(10, "C", 100.0), (11, "C+G", 50.0), (12, "G", 4.0)]


def test_merge_procs_unsupported_lists():
    assert probe.merge_procs(None, None) == []
    assert probe.merge_procs(None, [(5, None)]) == [{"pid": 5, "mem": None, "type": "G"}]


def test_num():
    assert probe.num("1.5") == 1.5
    assert probe.num("[N/A]") is None
    assert probe.num(None) is None


@linux_only
def test_sample_shape():
    snap = probe.Sampler().sample()
    assert snap["cpu"]["count"] > 0
    assert 0 <= snap["cpu"]["usage"] <= 100
    assert snap["mem"]["total_kb"] > 0
    assert snap["nvml"] in ("ok", "absent", "error")
    assert isinstance(snap["gpus"], list)
    json.dumps(snap)


@linux_only
def test_agent_protocol():
    """One JSON line per newline, exit on EOF."""
    agent = subprocess.Popen([sys.executable, "-u", "-c", PROBE, "30"],
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    try:
        for _ in range(2):
            agent.stdin.write(b"\n")
            agent.stdin.flush()
            snap = json.loads(agent.stdout.readline())
            assert "fatal" not in snap and snap["hostname"]
        agent.stdin.close()
        assert agent.wait(timeout=10) == 0
    finally:
        agent.kill()
