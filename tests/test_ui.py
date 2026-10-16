from rich.console import Console

from gpuwho import ui
from tests.conftest import FakeServer


def render(renderable, width=240):
    console = Console(record=True, width=width, color_system=None, file=open("/dev/null", "w"))
    console.print(renderable)
    return console.export_text()


def gpus(cluster, name):
    return next(s for s in cluster if s.name == name).data["gpus"]


def test_idle_gpu(cluster):
    node01 = gpus(cluster, "node01")
    assert ui.gpu_is_idle(node01[3])
    assert not ui.gpu_is_idle(node01[0])


def test_parked_xorg_does_not_make_a_gpu_busy(cluster):
    node03 = gpus(cluster, "node03")
    assert [p["user"] for p in node03[2]["procs"]] == ["gdm"]
    assert ui.gpu_is_idle(node03[2])
    assert [p["user"] for p in ui.busy_procs(node03[0])] == ["erin"]


def test_graphics_process_with_real_memory_counts():
    gpu = {"procs": [{"pid": 1, "mem": 9000.0, "type": "G", "user": "alice"}], "mem_used": 9000.0, "util": 80}
    assert ui.busy_procs(gpu) and not ui.gpu_is_idle(gpu)
    both = {"procs": [{"pid": 2, "mem": 4.0, "type": "C+G", "user": "bob"}], "mem_used": 4.0, "util": 0}
    assert ui.busy_procs(both)


def test_lost_gpu_is_never_idle(cluster):
    assert not ui.gpu_is_idle(gpus(cluster, "node03")[7])


def test_collect_users(cluster):
    users = ui.collect_users(cluster)
    assert len(users["dave"]["gpus"]) == 4
    assert users["dave"]["hosts"] == {"node02": [0, 1, 2, 3]}
    assert users["carol"]["hosts"] == {"node01": [6, 7]}
    assert "root@3f9c2a" in users  # docker job, labelled by short container id
    assert "gdm" not in users  # display server filtered out
    assert users["frank"]["gpus"] == [] and users["frank"]["cpu"] > 5000  # CPU-only user still listed
    assert round(users["bob"]["ram"] / 2**20, 1) == 20.1  # cgroup RAM, summed over node01 and cpu01


def test_ram_falls_back_to_rss_for_old_agents():
    assert ui.user_ram_kb({"rss_kb": 100}) == 100
    assert ui.user_ram_kb({"rss_kb": 100, "mem_kb": 40}) == 40
    assert ui.user_ram_kb({"rss_kb": 100, "mem_kb": None}) == 100


def test_dashboard_renders_every_server(cluster):
    text = render(ui.dashboard(cluster, 3, True, 240))
    for s in cluster:
        assert s.name in text
    assert "4/4 servers up" in text
    assert "GPUs 20" in text and "idle 7" in text
    assert "CPU only" in text
    assert "needs admin" in text  # the lost GPU on node03
    assert "VLLM::Worker_TP0" in text  # process table


def test_dashboard_narrow_terminal(cluster):
    render(ui.dashboard(cluster, 0, False, 80), width=80)


def test_server_down_and_connecting():
    down = FakeServer("gpu9", "10.9.9.9", error="NoValidConnectionsError: unable to connect")
    pending = FakeServer("gpu8", "10.9.9.8")
    text = render(ui.dashboard([down, pending], 3, False, 240))
    assert "unable to connect" in text and "connecting" in text
    assert "0/2 servers up" in text


def test_helpers():
    assert ui.gib(None) == "-"
    assert ui.gib(2048) == "2.0G"
    assert ui.fmt_uptime(90061) == "1d1h"
    assert ui.fmt_uptime(3720) == "1h2m"
    assert ui.short_gpu_name("NVIDIA GeForce RTX 4090") == "RTX 4090"
    assert ui.level_color(None) == "dim"
    assert [ui.level_color(p) for p in (10, 60, 90)] == ["green", "yellow", "red"]
    assert len(ui.bar(42, 10).plain) == 10
    assert ui.pretty_cmd("/usr/bin/python3 train.py").plain == "python3 train.py"
    assert ui.proc_owner({"user": "root", "container": "3f9c2a1b4d5e"}) == "root@3f9c2a"
