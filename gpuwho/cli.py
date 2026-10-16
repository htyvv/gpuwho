"""gpuwho - every GPU on every server, in one terminal, over SSH.

  gpuwho                       live dashboard (? for keys)
  gpuwho once [--json]         print one snapshot
  gpuwho free [-n N] [--ids]   list free GPUs, exit 1 if there aren't enough
  gpuwho demo                  the dashboard on a made-up cluster
  gpuwho add NAME [[USER@]HOST[:PORT]] [--password]
  gpuwho list | remove NAME | test [NAME...]
"""
import argparse
import getpass
import json
import os
import sys
import threading
import time

from rich import box
from rich.console import Console
from rich.table import Table

from gpuwho import __version__, config, ui
from gpuwho.config import LOCAL_HOSTS, ME, load_conf, save_conf

console = Console()
SCHEMA = 1  # `once --json` layout; bumped only for breaking changes


def build_servers(conf, names=None, idle_sec=90):
    from gpuwho.remote import Server

    known = [c["name"] for c in conf["servers"]]
    unknown = [n for n in names or [] if n not in known]
    if unknown:
        sys.exit("unknown server: %s (`gpuwho list` shows what's registered)" % ", ".join(unknown))
    servers = [Server(c, idle_sec) for c in conf["servers"] if not names or c["name"] in names]
    if not servers:
        sys.exit("no servers configured. add one with:  gpuwho add NAME [USER@]HOST[:PORT] [--password]")
    return servers


def probe_all(servers, timeout=30):
    """Probe every server at once; give up on the slow ones after `timeout` seconds.
    Daemon threads, so a hung connect can't keep the process alive after we print."""
    threads = [threading.Thread(target=s.probe, daemon=True) for s in servers]
    for t in threads:
        t.start()
    deadline = time.time() + timeout
    for s, t in zip(servers, threads):
        t.join(max(0.0, deadline - time.time()))
        if t.is_alive():
            s.error = "no answer within %ds" % timeout
    return [s for s, t in zip(servers, threads) if not t.is_alive()]


def snapshot(servers):
    """The `once --json` document: stable keys, one entry per server, errors included."""
    out = []
    for s in servers:
        entry = {"name": s.name, "host": s.cfg["host"], "ok": s.data is not None and not s.error,
                 "error": s.error, "latency": round(s.latency, 3) if s.latency else None}
        if s.data:
            for key in ("hostname", "uptime", "cpu", "mem", "users", "gpus", "gpu_error", "nvml", "driver", "cuda"):
                entry[key] = s.data.get(key)
        out.append(entry)
    return {"schema": SCHEMA, "version": __version__, "time": time.time(), "servers": out}


def close_all(servers):
    for s in servers:
        s.close()


def cmd_watch(args):
    from gpuwho.tui import LiveView

    # agents exit on their own if they hear nothing for this long (e.g. the network dropped)
    servers = build_servers(load_conf(), idle_sec=max(90, 3 * args.interval))
    LiveView(servers, args.interval, args.procs, True if args.compact else None, console).run()


def cmd_demo(args):
    from gpuwho import demo
    from gpuwho.tui import LiveView

    servers = demo.servers()
    if not sys.stdout.isatty():
        for s in servers:
            s.probe()
        console.print(ui.dashboard(servers, 0, args.procs, console.width, args.compact))
        return
    LiveView(servers, args.interval, args.procs, True if args.compact else None, console).run()


def cmd_once(args):
    servers = build_servers(load_conf(), args.names)
    done = probe_all(servers, args.timeout)
    if args.json:
        print(json.dumps(snapshot(servers), indent=2 if sys.stdout.isatty() else None))
    else:
        console.print(ui.dashboard(servers, 0, args.procs, console.width, args.compact))
    close_all(done)
    ok = sum(1 for s in servers if s.data is not None and not s.error)
    if ok == 0 or (args.strict and ok < len(servers)):
        sys.exit(1)


def free_gpus(servers, min_free_gib=None, max_util=50):
    """[(server, gpu index, free GiB, gpu name)], most free memory first. Without
    min_free_gib a GPU must be idle; with it, it needs that much free memory and load
    at most max_util %."""
    out = []
    for s in servers:
        for g in (s.data or {}).get("gpus", []):
            if g.get("invalid") or g.get("mem_used") is None or not g.get("mem_total"):
                continue
            free = (g["mem_total"] - g["mem_used"]) / 1024
            if min_free_gib is None:
                ok = ui.gpu_is_idle(g)
            else:
                ok = free >= min_free_gib and (g.get("util") or 0) <= max_util
            if ok:
                out.append((s.name, g["index"], round(free, 1), ui.short_gpu_name(g["name"])))
    return sorted(out, key=lambda r: (-r[2], r[0], r[1]))


def cmd_free(args):
    servers = build_servers(load_conf(), args.names)
    done = probe_all(servers, args.timeout)
    found = free_gpus(servers, args.min_free, args.max_util)
    close_all(done)
    if args.ids:
        hosts = sorted(set(r[0] for r in found))
        if len(hosts) > 1:
            sys.exit("--ids needs one server, e.g. `gpuwho free %s --ids`" % hosts[0])
        print(",".join(str(r[1]) for r in found[:args.n]))
    elif args.json:
        print(json.dumps([{"server": r[0], "gpu": r[1], "free_gib": r[2], "name": r[3]} for r in found]))
    else:
        for name, idx, free, gpu in found:
            print("%-12s %2d  %6.1fG free  %s" % (name, idx, free, gpu))
        for s in servers:
            if s.error:
                print("%-12s  ?  %s" % (s.name, s.error), file=sys.stderr)
    if len(found) < args.n:
        sys.exit(1)


def cmd_add(args):
    from gpuwho.remote import Server, parse_target, ssh_settings

    conf = load_conf()
    existing = next((c for c in conf["servers"] if c["name"] == args.name), None)
    if existing and not args.force:
        sys.exit("server '%s' already exists (use --force to overwrite)" % args.name)
    user, host, port = parse_target(args.host or args.name)
    entry = {"name": args.name, "host": host}
    # store only what was typed, so ~/.ssh/config keeps working for the rest
    if args.user or user:
        entry["user"] = args.user or user
    if args.port or port:
        entry["port"] = args.port or port
    if args.key:
        entry["key"] = os.path.expanduser(args.key)
    if args.password:
        who = entry.get("user") or ssh_settings(host).get("user") or ME
        pw = getpass.getpass("password for %s@%s: " % (who, host))
        if pw:
            entry["password"] = pw
    if not args.no_test:
        s = Server(entry).probe()
        if s.error:
            console.print("[red]connection test failed:[/] %s" % s.error)
            if not args.force:
                s.close()
                sys.exit("not saved (use --force to save anyway, or --no-test)")
        else:
            console.print("[green]ok[/] %s · %d GPUs · %d CPU cores"
                          % (s.data["hostname"], len(s.data["gpus"]), s.data["cpu"]["count"]))
        s.close()
    conf["servers"] = [c for c in conf["servers"] if c["name"] != args.name] + [entry]
    save_conf(conf)
    console.print("saved '%s' to %s (mode 600)" % (args.name, config.CONF_PATH))


def cmd_remove(args):
    conf = load_conf()
    before = len(conf["servers"])
    conf["servers"] = [c for c in conf["servers"] if c["name"] != args.name]
    if len(conf["servers"]) == before:
        sys.exit("no server named '%s'" % args.name)
    save_conf(conf)
    console.print("removed '%s'" % args.name)


def cmd_list(args):
    from gpuwho.remote import ssh_settings

    conf = load_conf()
    t = Table(box=box.SIMPLE_HEAD)
    for col in ("Name", "Host", "Port", "User", "Auth", "Via"):
        t.add_column(col)
    for c in conf["servers"]:
        if c["host"] in LOCAL_HOSTS:
            t.add_row(c["name"], c["host"], "", "", "local", "")
            continue
        s = ssh_settings(c["host"])
        auth = "password ••••" if c.get("password") else ("key " + c["key"]) if c.get("key") else "ssh key/agent"
        via = s.get("proxyjump") or ("ProxyCommand" if s.get("proxycommand") else "")
        t.add_row(c["name"], s.get("hostname") or c["host"], str(c.get("port") or s.get("port") or 22),
                  c.get("user") or s.get("user") or ME, auth, "" if via == "none" else via)
    console.print(t)
    console.print("[dim]%s[/]" % config.CONF_PATH)


def cmd_test(args):
    servers = build_servers(load_conf(), args.names)
    done = probe_all(servers, args.timeout)
    for s in servers:
        if s.error:
            console.print("[red]✗[/] %-12s %s" % (s.name, s.error))
        else:
            console.print("[green]✓[/] %-12s %s · %d GPUs · %.1fs"
                          % (s.name, s.data["hostname"], len(s.data["gpus"]), s.latency))
    close_all(done)
    if any(s.error for s in servers):
        sys.exit(1)


def interval_arg(text):
    value = float(text)
    if value < 0.5:
        raise argparse.ArgumentTypeError("must be at least 0.5 seconds")
    return value


def build_parser():
    ap = argparse.ArgumentParser(prog="gpuwho", description="Every GPU on every server, in one terminal, over SSH.")
    ap.add_argument("-V", "--version", action="version", version="%(prog)s " + __version__)
    ap.add_argument("-i", "--interval", type=interval_arg, default=3.0, help="refresh interval in seconds (default 3)")
    ap.add_argument("-p", "--procs", action="store_true", help="show per-process GPU table")
    ap.add_argument("-c", "--compact", action="store_true", help="one line per server")
    ap.add_argument("--no-color", action="store_true", help="no colors (NO_COLOR works too)")
    sub = ap.add_subparsers(dest="cmd")
    keep = argparse.SUPPRESS  # subcommand flags must not reset the global ones to False

    def snapshot_flags(p):
        p.add_argument("names", nargs="*", help="only these servers")
        p.add_argument("--timeout", type=float, default=30, help="give up on a server after this long (default 30 s)")

    p = sub.add_parser("once", help="print a single snapshot")
    snapshot_flags(p)
    p.add_argument("--json", action="store_true", help="machine-readable output (schema %d)" % SCHEMA)
    p.add_argument("--strict", action="store_true", help="exit 1 if any server failed, not only all of them")
    p.add_argument("-p", "--procs", action="store_true", default=keep)
    p.add_argument("-c", "--compact", action="store_true", default=keep)
    p.set_defaults(func=cmd_once)

    p = sub.add_parser("free", help="list free GPUs (exit 1 if fewer than -n)")
    snapshot_flags(p)
    p.add_argument("-n", type=int, default=1, help="how many you need (default 1)")
    p.add_argument("--min-free", type=float, metavar="GIB",
                   help="instead of idle GPUs, any GPU with this much free memory")
    p.add_argument("--max-util", type=float, default=50, metavar="PCT",
                   help="with --min-free: skip GPUs busier than this (default 50)")
    p.add_argument("--ids", action="store_true", help="print only GPU indices, comma-separated (one server)")
    p.add_argument("--json", action="store_true")
    p.set_defaults(func=cmd_free)

    p = sub.add_parser("demo", help="the dashboard on a made-up cluster, no servers needed")
    p.add_argument("-i", "--interval", type=interval_arg, default=keep)
    p.add_argument("-p", "--procs", action="store_true", default=keep)
    p.add_argument("-c", "--compact", action="store_true", default=keep)
    p.set_defaults(func=cmd_demo)

    p = sub.add_parser("add", help="register a server")
    p.add_argument("name")
    p.add_argument("host", nargs="?",
                   help="[user@]host[:port], an ~/.ssh/config alias, or 'local' (default: NAME)")
    p.add_argument("-u", "--user", help="login user (default: from ~/.ssh/config, else %s)" % ME)
    p.add_argument("-P", "--port", type=int)
    p.add_argument("--password", action="store_true", help="prompt for a password and store it")
    p.add_argument("--key", help="private key file")
    p.add_argument("--force", action="store_true", help="overwrite existing / save even if test fails")
    p.add_argument("--no-test", action="store_true", help="skip the connection test")
    p.set_defaults(func=cmd_add)

    p = sub.add_parser("remove", help="unregister a server")
    p.add_argument("name")
    p.set_defaults(func=cmd_remove)

    sub.add_parser("list", help="list registered servers").set_defaults(func=cmd_list)

    p = sub.add_parser("test", help="check every connection (exit 1 if any fails)")
    snapshot_flags(p)
    p.set_defaults(func=cmd_test)
    return ap


def main(argv=None):
    global console
    args = build_parser().parse_args(argv)
    if args.no_color:
        console = Console(no_color=True)
    func = getattr(args, "func", None)
    if func is None:
        if sys.stdout.isatty():
            func = cmd_watch
        else:  # under `watch`, cron or a pipe a live screen would print nothing and never end
            print("gpuwho: not a terminal, printing one snapshot (`gpuwho once` does this explicitly)",
                  file=sys.stderr)
            args.names, args.json, args.strict, args.timeout = [], False, False, 30
            func = cmd_once
    func(args)


if __name__ == "__main__":
    main()
