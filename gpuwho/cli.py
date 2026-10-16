"""gpuwho - every GPU on every server, in one terminal, over SSH.

  gpuwho                      live dashboard (q: quit, p: toggle process list)
  gpuwho once [--json]        print one snapshot
  gpuwho add NAME HOST [...]  register a server (use --password to store a password)
  gpuwho list | remove NAME | test [NAME...]
"""
import argparse
import getpass
import json
import os
import select
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor

from rich import box
from rich.console import Console
from rich.live import Live
from rich.table import Table

from gpuwho import __version__, config
from gpuwho.config import LOCAL_HOSTS, ME, load_conf, save_conf
from gpuwho.remote import Server
from gpuwho.ui import dashboard

console = Console()


def build_servers(conf, names=None, idle_sec=90):
    servers = [Server(c, idle_sec) for c in conf["servers"] if not names or c["name"] in names]
    if not servers:
        sys.exit("no servers configured. add one with:  gpuwho add NAME HOST [-u USER] [--password]")
    return servers


def probe_all(servers):
    with ThreadPoolExecutor(max_workers=min(32, len(servers))) as ex:
        list(ex.map(Server.probe, servers))


def cmd_watch(args):
    # agents exit on their own if they hear nothing for this long (e.g. the network dropped)
    servers = build_servers(load_conf(), idle_sec=max(90, 3 * args.interval))
    stop = threading.Event()
    state = {"procs": args.procs}

    def poll(s):
        while not stop.is_set():
            s.probe()
            stop.wait(args.interval)

    for s in servers:
        threading.Thread(target=poll, args=(s,), daemon=True).start()

    old_tty = None
    if sys.stdin.isatty():
        import termios
        import tty
        old_tty = termios.tcgetattr(sys.stdin)
        tty.setcbreak(sys.stdin.fileno())

    def render():
        return dashboard(servers, args.interval, state["procs"], console.width)

    # Redraw only when something changed (new data, key press, resize), at most once a second,
    # plus every 10 s so stale markers appear even if a host goes silent.
    try:
        with Live(render(), console=console, screen=True, auto_refresh=False) as live:
            drawn_sig, drawn_at = None, 0.0
            while not stop.is_set():
                forced = False
                if old_tty is not None and select.select([sys.stdin], [], [], 0.2)[0]:
                    key = sys.stdin.read(1).lower()
                    if key == "q":
                        break
                    if key == "p":
                        state["procs"] = not state["procs"]
                        forced = True
                elif old_tty is None:
                    time.sleep(0.2)
                sig = (sum(s.version for s in servers), console.width, console.height)
                since = time.time() - drawn_at
                if forced or (sig != drawn_sig and since >= 1.0) or since >= 10:
                    live.update(render(), refresh=True)
                    drawn_sig, drawn_at = sig, time.time()
    except KeyboardInterrupt:
        pass
    finally:
        stop.set()
        if old_tty is not None:
            import termios
            termios.tcsetattr(sys.stdin, termios.TCSADRAIN, old_tty)
        for s in servers:
            s.close()


def cmd_once(args):
    servers = build_servers(load_conf(), args.names)
    probe_all(servers)
    if args.json:
        print(json.dumps({s.name: s.data if s.data else {"error": s.error} for s in servers}, indent=2))
    else:
        console.print(dashboard(servers, 0, args.procs, console.width))
    for s in servers:
        s.close()


def cmd_add(args):
    conf = load_conf()
    existing = next((c for c in conf["servers"] if c["name"] == args.name), None)
    if existing and not args.force:
        sys.exit("server '%s' already exists (use --force to overwrite)" % args.name)
    entry = {"name": args.name, "host": args.host, "port": args.port, "user": args.user or ME}
    if args.key:
        entry["key"] = os.path.expanduser(args.key)
    if args.password:
        pw = getpass.getpass("password for %s@%s: " % (entry["user"], args.host))
        if pw:
            entry["password"] = pw
    if not args.no_test:
        s = Server(entry).probe()
        if s.error:
            console.print("[red]connection test failed:[/] %s" % s.error)
            if not args.force:
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
    conf = load_conf()
    t = Table(box=box.SIMPLE_HEAD)
    for col in ("Name", "Host", "Port", "User", "Auth"):
        t.add_column(col)
    for c in conf["servers"]:
        auth = "password ••••" if c.get("password") else ("key " + c["key"]) if c.get("key") else "ssh key/agent"
        if c["host"] in LOCAL_HOSTS:
            auth = "local"
        t.add_row(c["name"], c["host"], str(c.get("port", 22)), c.get("user") or ME, auth)
    console.print(t)
    console.print("[dim]%s[/]" % config.CONF_PATH)


def cmd_test(args):
    servers = build_servers(load_conf(), args.names)
    probe_all(servers)
    for s in servers:
        if s.error:
            console.print("[red]✗[/] %-12s %s" % (s.name, s.error))
        else:
            console.print("[green]✓[/] %-12s %s · %d GPUs · %.1fs"
                          % (s.name, s.data["hostname"], len(s.data["gpus"]), s.latency))
        s.close()


def build_parser():
    ap = argparse.ArgumentParser(prog="gpuwho", description="Every GPU on every server, in one terminal, over SSH.")
    ap.add_argument("-V", "--version", action="version", version="%(prog)s " + __version__)
    ap.add_argument("-i", "--interval", type=float, default=3.0, help="refresh interval in seconds (default 3)")
    ap.add_argument("-p", "--procs", action="store_true", help="show per-process GPU table")
    sub = ap.add_subparsers(dest="cmd")

    p = sub.add_parser("once", help="print a single snapshot")
    p.add_argument("names", nargs="*")
    p.add_argument("--json", action="store_true")
    p.add_argument("-p", "--procs", action="store_true")
    p.set_defaults(func=cmd_once)

    p = sub.add_parser("add", help="register a server")
    p.add_argument("name")
    p.add_argument("host", help="hostname/IP, or 'local' for the machine gpuwho runs on")
    p.add_argument("-u", "--user", help="login user (default: %s)" % ME)
    p.add_argument("-P", "--port", type=int, default=22)
    p.add_argument("--password", action="store_true", help="prompt for a password and store it")
    p.add_argument("--key", help="private key file")
    p.add_argument("--force", action="store_true", help="overwrite existing / save even if test fails")
    p.add_argument("--no-test", action="store_true", help="skip the connection test")
    p.set_defaults(func=cmd_add)

    p = sub.add_parser("remove", help="unregister a server")
    p.add_argument("name")
    p.set_defaults(func=cmd_remove)

    sub.add_parser("list", help="list registered servers").set_defaults(func=cmd_list)

    p = sub.add_parser("test", help="test connectivity")
    p.add_argument("names", nargs="*")
    p.set_defaults(func=cmd_test)
    return ap


def main(argv=None):
    args = build_parser().parse_args(argv)
    (getattr(args, "func", None) or cmd_watch)(args)


if __name__ == "__main__":
    main()
