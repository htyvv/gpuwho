"""Tests for the Python fallback agent (crates/gpuwho/assets/probe.py).

It is piped to servers whose python3 may be as old as 3.6, so it stays stdlib-only;
CI also runs it in python:3.6/3.7/3.8 containers.
"""
import importlib.util
import json
import os
import pathlib
import subprocess
import sys

import pytest

PATH = pathlib.Path(__file__).resolve().parent.parent / "crates" / "gpuwho" / "assets" / "probe.py"
PROBE = PATH.read_text()
_spec = importlib.util.spec_from_file_location("probe", str(PATH))
probe = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(probe)

# the agent reads /proc, so anything that actually runs it needs Linux
linux_only = pytest.mark.skipif(not sys.platform.startswith("linux"), reason="agent reads /proc")


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
    assert all(u["mem_kb"] >= 0 for u in snap["users"])
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


@pytest.mark.parametrize("cgroup, expected", [
    ("0::/user.slice/user-1000.slice/session-3.scope", ["/sys/fs/cgroup/user.slice/user-1000.slice/session-3.scope"]),
    ("0::/system.slice/docker-3f9c2a1b4d5e.scope", ["/sys/fs/cgroup/system.slice/docker-3f9c2a1b4d5e.scope"]),
    # hybrid: v2 for systemd tracking, memory controller still on v1
    ("12:memory:/user.slice/user-1000.slice\n1:name=systemd:/user.slice/user-1000.slice/session-3.scope\n"
     "0::/user.slice/user-1000.slice/session-3.scope",
     ["/sys/fs/cgroup/memory/user.slice/user-1000.slice",
      "/sys/fs/cgroup/user.slice/user-1000.slice/session-3.scope"]),
    ("4:cpu,cpuacct:/\n7:blkio,memory:/user.slice", ["/sys/fs/cgroup/memory/user.slice"]),
    ("0::/", []),  # root cgroup counts the whole machine
    ("", []),
])
def test_memcg_dirs(cgroup, expected):
    assert sorted(probe.memcg_dirs(cgroup)) == sorted(expected)


def test_charged_kb_v2_counts_anon_and_shmem_not_cache():
    stat = "anon 1073741824\nfile 53687091200\nkernel 1000\nshmem 2147483648\nfile_mapped 1\n"
    assert probe.charged_kb(stat) == 3 * 1024 * 1024


def test_charged_kb_v1():
    assert probe.charged_kb("cache 999999\nrss 1048576\nshmem 1048576\nmapped_file 1\n") == 2048


def test_charged_kb_unreadable():
    assert probe.charged_kb("") is None


def test_split_by_rss():
    charged = {"/u/alice": 10 * 1024, "/docker/x": 3000, "/gone": None}.get
    groups = {
        "/u/alice": {"alice": 300 * 1024},       # 30 forked workers: RSS says 300G, cgroup says 10G
        "/docker/x": {"root": 2000, "bob": 1000},  # shared cgroup: split by RSS
        "/gone": {"carol": 700},                   # memory.stat unreadable: RSS as is
        None: {"dave": 500},                       # no memory cgroup at all
    }
    assert probe.split_by_rss(groups, charged) == {"alice": 10 * 1024, "root": 2000, "bob": 1000,
                                                    "carol": 700, "dave": 500}


def test_read_survives_non_utf8(tmp_path):
    f = tmp_path / "cmdline"
    f.write_bytes(b"python\x00\xbd\xc7\xc7\xe8.py\x00")  # CP949 bytes in argv
    assert probe.read(str(f)).startswith("python\x00")
    assert probe.read(str(tmp_path / "missing")) == ""


def test_merge_procs_mps_duplicates_stay_compute():
    procs = probe.merge_procs([(7, 10.0), (7, 10.0)], [])
    assert procs == [{"pid": 7, "mem": 10.0, "type": "C"}]


def test_average_sm():
    samples = [(1, 100, 80), (1, 200, 60), (2, 150, 0), (2, 50, 90), (3, 300, 4294967295)]
    sm, newest = probe.average_sm(samples, since=60)
    assert sm == {1: 70, 2: 0}  # pid 2's old sample and pid 3's garbage value are dropped
    assert newest == 200


def test_proc_state():
    stats = {10: ("S", 1, 1, 1), 11: ("Z", 1, 1, 1), 12: ("D", 1, 1, 1)}
    assert probe.proc_state(10, stats, "alice") is None
    assert probe.proc_state(10, stats, None) == "hidden"
    assert probe.proc_state(11, stats, "alice") == "zombie"
    assert probe.proc_state(12, stats, "alice") == "D"
    assert probe.proc_state(99, stats, None) == "gone"


@linux_only
def test_proc_stats_reads_own_process():
    import os
    stats = probe.proc_stats()
    state, ticks, rss, start = stats[os.getpid()]
    assert state in "RSD" and rss > 0 and start > 0
    assert set(probe.proc_stats([str(os.getpid()), "999999999"])) == {os.getpid()}


@linux_only
def test_partial_scans_between_full_ones():
    s = probe.Sampler()
    flags = [s.sample()["agent"]["full_scan"] for _ in range(probe.FULL_SCAN_EVERY + 1)]
    assert flags == [True] + [False] * (probe.FULL_SCAN_EVERY - 1) + [True]


def test_nvml_absent_without_library(monkeypatch):
    def no_lib(name):
        raise OSError("libnvidia-ml.so.1: cannot open shared object file")
    monkeypatch.setattr(probe.ctypes, "CDLL", no_lib)
    n = probe.Nvml()
    assert n.query() == ([], None) and n.status == "absent"
    assert n.retry_at > 0  # tried again later, e.g. after the driver is installed


def test_read_claims_trusts_the_file_owner(tmp_path):
    (tmp_path / "bob.json").write_text(json.dumps({"claims": [
        {"gpus": [0, 1], "until": 2000, "note": "rebuttal"}, {"gpus": [2], "until": 500}]}))
    (tmp_path / "junk.json").write_text("not json")
    got = probe.read_claims(now=1000, directory=str(tmp_path))
    assert len(got) == 1 and got[0]["gpus"] == [0, 1] and got[0]["note"] == "rebuttal"
    assert got[0]["user"] == probe.owner_name(os.getuid())  # not "bob": whoever owns the file


@linux_only
def test_sample_reports_claims_and_start_times():
    snap = probe.Sampler().sample()
    assert isinstance(snap["claims"], list) and snap["clk_tck"] > 0
    assert snap["agent"]["impl"] == "python"
