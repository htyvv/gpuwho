"""Turning agent snapshots into rich renderables: one panel per server plus a per-user summary."""
import os
import time
import zlib

from rich import box
from rich.console import Group
from rich.panel import Panel
from rich.table import Table
from rich.text import Text

from gpuwho.config import ME

USER_COLORS = ["cyan", "magenta", "bright_blue", "bright_green", "orange1", "orchid", "turquoise2",
               "gold1", "salmon1", "spring_green2", "deep_sky_blue1", "hot_pink", "khaki1", "plum1"]


def user_text(user, suffix=""):
    if not user:
        return Text("?" + suffix, style="dim")
    color = USER_COLORS[zlib.crc32(user.encode()) % len(USER_COLORS)]
    style = "bold %s underline" % color if user == ME else color
    return Text(user + suffix, style=style)


def level_color(pct, warn=50, crit=85):
    if pct is None:
        return "dim"
    return "red" if pct >= crit else "yellow" if pct >= warn else "green"


def bar(pct, width=10):
    if pct is None:
        return Text("─" * width, style="dim")
    filled = int(round(max(0.0, min(100.0, pct)) / 100 * width))
    t = Text("█" * filled, style=level_color(pct))
    t.append("░" * (width - filled), style="grey35")
    return t


def gib(mib):
    return "-" if mib is None else "%.1fG" % (mib / 1024)


def fmt_uptime(sec):
    if not sec:
        return "-"
    d, h = int(sec // 86400), int(sec % 86400 // 3600)
    return "%dd%dh" % (d, h) if d else "%dh%dm" % (h, int(sec % 3600 // 60))


def short_gpu_name(name):
    for p in ("NVIDIA ", "GeForce ", "Tesla "):
        name = name.replace(p, "")
    return name


def proc_owner(p):
    """Label for a GPU process: user, or user@container for dockerized jobs."""
    return (p.get("user") or "?") + ("@" + p["container"][:6] if p.get("container") else "")


def is_display_proc(p):
    """A display server (Xorg, gnome-shell) parked on the GPU: graphics-only and a few MiB."""
    return p.get("type") == "G" and (p.get("mem") or 0) < 64


def busy_procs(g):
    return [p for p in g["procs"] if not is_display_proc(p)]


def gpu_is_idle(g):
    if g.get("invalid"):
        return False
    return not busy_procs(g) and (g["mem_used"] or 0) < 1024 and (g["util"] or 0) < 5


def pretty_cmd(cmd):
    """Executable basename highlighted, arguments dimmed (gpustat's prettify_commandline)."""
    if not cmd:
        return Text("?", style="dim")
    exe, _, args = cmd.partition(" ")
    t = Text(os.path.basename(exe), style="bold")
    if args:
        t.append(" " + args, style="dim")
    return t


def gpu_users_text(g):
    agg = {}
    for p in busy_procs(g):
        key = (p.get("user"), p.get("container"))
        agg[key] = agg.get(key, 0) + (p.get("mem") or 0)
    t = Text()
    for (user, ctr), mem in sorted(agg.items(), key=lambda kv: -kv[1]):
        if t:
            t.append("  ")
        t.append_text(user_text(user, "@" + ctr[:6] if ctr else ""))
        t.append(" %s" % gib(mem), style="dim")
    return t


def server_panel(s, interval, show_procs):
    title = Text.assemble((s.name, "bold"), ("  " + s.cfg["host"], "dim"))
    d = s.data
    if d and d.get("driver"):
        title.append("  driver %s · CUDA %s" % (d["driver"], d.get("cuda") or "?"), style="dim")
    if d is None:
        msg = Text(s.error or "connecting...", style="red" if s.error else "dim")
        return Panel(msg, title=title, title_align="left", border_style="red" if s.error else "grey50")

    stale = s.updated and time.time() - s.updated > max(3 * interval, 15)
    cpu, mem = d["cpu"], d["mem"]
    mem_used = (mem["total_kb"] - mem["avail_kb"]) / 1024 / 1024
    mem_total = mem["total_kb"] / 1024 / 1024
    mem_pct = 100 * mem_used / mem_total if mem_total else None

    head = Text()
    head.append("CPU ", style="bold")
    head.append_text(bar(cpu["usage"], 8))
    head.append(" %3.0f%% of %dc" % (cpu["usage"], cpu["count"]))
    head.append("   load ", style="bold")
    head.append(" / ".join("%.1f" % x for x in cpu["load"]))
    head.append("   RAM ", style="bold")
    head.append_text(bar(mem_pct, 8))
    head.append(" %.0f/%.0fG" % (mem_used, mem_total))
    head.append("   up %s" % fmt_uptime(d.get("uptime")), style="dim")

    top_users = [u for u in d["users"] if u["cpu"] >= 50][:6]
    cpu_line = Text("CPU users  ", style="bold")
    for u in top_users:
        cpu_line.append_text(user_text(u["user"]))
        cpu_line.append(" %.1fc %.0fG   " % (u["cpu"] / 100, u["rss_kb"] / 1024 / 1024), style="dim")
    if not top_users:
        cpu_line.append("-", style="dim")

    parts = [head, cpu_line]

    if d.get("nvml") == "absent":
        parts.append(Text("CPU only (no NVIDIA GPU)", style="dim"))
    elif d.get("gpu_error") and not d["gpus"]:
        parts.append(Text("GPU: " + d["gpu_error"], style="yellow"))
    elif d["gpus"]:
        t = Table(box=box.SIMPLE_HEAD, expand=True, pad_edge=False, padding=(0, 1))
        t.add_column("#", justify="right", width=2)
        t.add_column("GPU", no_wrap=True)
        has_fan = any(g.get("fan") is not None for g in d["gpus"])
        t.add_column("Temp Fan" if has_fan else "Temp", justify="right", width=8 if has_fan else 4)
        t.add_column("Util", no_wrap=True, min_width=15)
        t.add_column("Memory", no_wrap=True, width=23)
        t.add_column("Power", justify="right", no_wrap=True, width=9)
        t.add_column("Users (GPU mem)", ratio=1)
        for g in d["gpus"]:
            if g.get("invalid"):  # NVML says the GPU is lost / in an unknown state
                t.add_row(Text(str(g["index"]), style="bold red"), Text(g["name"], style="bold red"),
                          "", "", "", "", Text("needs admin (reset/reboot)", style="red"))
                continue
            mem_pct = 100 * g["mem_used"] / g["mem_total"] if g["mem_total"] else None
            idx = Text(str(g["index"]), style="bold green" if gpu_is_idle(g) else "bold")
            util = bar(g["util"], 10).append(" %3.0f%%" % (g["util"] or 0))
            if g.get("enc") or g.get("dec"):  # video encode/decode engines (gpustat -e)
                util.append(" E%d D%d" % (g.get("enc") or 0, g.get("dec") or 0), style="magenta")
            memc = bar(mem_pct, 10).append(" %s/%s" % (gib(g["mem_used"]), gib(g["mem_total"])))
            temp = Text("-" if g["temp"] is None else "%.0f°" % g["temp"],
                        style=level_color(g["temp"], 70, 83))
            if has_fan:
                temp.append(" %3s" % ("-" if g.get("fan") is None else "%d%%" % g["fan"]), style="cyan")
            power = "-" if g["power"] is None else "%.0f/%.0fW" % (g["power"], g["power_limit"] or 0)
            if busy_procs(g):
                users = gpu_users_text(g)
            elif g.get("procs_supported") is False:
                users = Text("process info N/A", style="dim")
            else:
                users = Text("idle" if gpu_is_idle(g) else "-", style="green" if gpu_is_idle(g) else "dim")
            t.add_row(idx, short_gpu_name(g["name"]), temp, util, memc, power, users)
        parts.append(t)

        if show_procs:
            pt = Table(box=box.SIMPLE_HEAD, expand=True, pad_edge=False, padding=(0, 1), style="dim")
            pt.add_column("GPU", justify="right", width=3)
            pt.add_column("PID", justify="right", width=8)
            pt.add_column("User", no_wrap=True)
            pt.add_column("GPU mem", justify="right", width=7)
            pt.add_column("CPU%", justify="right", width=6)
            pt.add_column("RAM", justify="right", width=6)
            pt.add_column("Command", ratio=1, no_wrap=True, overflow="ellipsis")
            for g in d["gpus"]:
                for p in sorted(busy_procs(g), key=lambda p: -(p.get("mem") or 0)):
                    pt.add_row(str(g["index"]), str(p["pid"]),
                               user_text(p.get("user"), "@" + p["container"][:6] if p.get("container") else ""),
                               gib(p.get("mem")), "-" if p.get("cpu") is None else "%.0f" % p["cpu"],
                               "-" if p.get("rss_kb") is None else gib(p["rss_kb"] / 1024),
                               pretty_cmd(p.get("cmd")))
            if pt.row_count:
                parts.append(pt)
    else:
        parts.append(Text("no GPUs", style="dim"))

    n_idle = sum(gpu_is_idle(g) for g in d["gpus"])
    sub = Text()
    if d["gpus"]:
        sub.append("%d GPUs · %d idle" % (len(d["gpus"]), n_idle), style="green" if n_idle else "dim")
        n_bad = sum(1 for g in d["gpus"] if g.get("invalid"))
        if n_bad:
            sub.append(" · %d lost" % n_bad, style="bold red")
    if d.get("gpu_error") and d["gpus"]:
        sub.append("  ⚠ %s" % d["gpu_error"][:50], style="yellow")
    if s.error:
        sub.append("  ⚠ %s" % s.error[:60], style="red")
    elif stale:
        sub.append("  ⚠ stale", style="yellow")
    elif s.latency is not None:
        sub.append("  %.1fs" % s.latency, style="dim")
    return Panel(Group(*parts), title=title, title_align="left", subtitle=sub, subtitle_align="right",
                 border_style="yellow" if (s.error or stale) else "grey50")


def collect_users(servers):
    """GPU + CPU usage per user across every server: {user: {gpus, gpu_mem, cpu, rss, hosts}}."""
    agg = {}

    def entry(user):
        return agg.setdefault(user, {"gpus": [], "gpu_mem": 0.0, "cpu": 0.0, "rss": 0.0, "hosts": {}})

    for s in servers:
        if not s.data:
            continue
        for g in s.data["gpus"]:
            per_user = {}
            for p in busy_procs(g):
                per_user[proc_owner(p)] = per_user.get(proc_owner(p), 0) + (p.get("mem") or 0)
            for user, mem in per_user.items():
                e = entry(user)
                e["gpus"].append((s.name, g["index"]))
                e["gpu_mem"] += mem
                e["hosts"].setdefault(s.name, []).append(g["index"])
        for u in s.data["users"]:
            if u["cpu"] >= 50 or u["user"] in agg:
                e = entry(u["user"])
                e["cpu"] += u["cpu"]
                e["rss"] += u["rss_kb"]
    return agg


def user_summary(servers):
    agg = collect_users(servers)
    t = Table(box=box.SIMPLE_HEAD, expand=True, pad_edge=False, padding=(0, 1))
    t.add_column("User", no_wrap=True)
    t.add_column("GPUs", justify="right", width=4)
    t.add_column("GPU mem", justify="right", width=8)
    t.add_column(Text("Where  server[gpu idx]"), ratio=1)
    t.add_column("CPU cores", justify="right", width=9)
    t.add_column("RAM", justify="right", width=7)
    rows = sorted(agg.items(), key=lambda kv: (-kv[1]["gpu_mem"], -kv[1]["cpu"]))
    for user, e in rows[:20]:
        where = Text()
        for host, idxs in e["hosts"].items():
            where.append("%s[%s] " % (host, ",".join(str(i) for i in sorted(idxs))))
        if "@" in user:
            u, ctr = user.split("@", 1)
            label = user_text(u, "@" + ctr)
        else:
            label = user_text(user)
        # CPU/RAM of containerised jobs is accounted to the host-side user (usually root)
        in_ctr = "@" in user
        t.add_row(label, str(len(e["gpus"])) if e["gpus"] else "-",
                  gib(e["gpu_mem"]) if e["gpus"] else "-", where,
                  "-" if in_ctr else "%.1f" % (e["cpu"] / 100),
                  "-" if in_ctr else "%.0fG" % (e["rss"] / 1024 / 1024))
    return Panel(t, title=Text("Users across all servers", style="bold"), title_align="left",
                 border_style="grey50")


def header(servers, interval, show_procs):
    gpus = [g for s in servers if s.data for g in s.data["gpus"]]
    idle = sum(gpu_is_idle(g) for g in gpus)
    up = sum(1 for s in servers if s.data and not s.error)
    t = Text()
    t.append(" gpuwho ", style="bold black on cyan")
    t.append("  %d/%d servers up" % (up, len(servers)))
    t.append("  ·  GPUs %d" % len(gpus))
    t.append("  ·  idle %d" % idle, style="bold green" if idle else "dim")
    t.append("  ·  %s" % time.strftime("%Y-%m-%d %H:%M:%S"), style="dim")
    if interval:  # live mode
        t.append("  ·  every %gs  ·  q quit  p procs%s" % (interval, " (on)" if show_procs else ""), style="dim")
    return t


def dashboard(servers, interval, show_procs, width):
    cols = max(1, min(len(servers), width // 115))
    grid = Table.grid(expand=True, padding=(0, 1))
    for _ in range(cols):
        grid.add_column(ratio=1)
    panels = [server_panel(s, interval, show_procs) for s in servers]
    for i in range(0, len(panels), cols):
        row = panels[i:i + cols]
        grid.add_row(*(row + [""] * (cols - len(row))))
    return Group(header(servers, interval, show_procs), grid, user_summary(servers))
