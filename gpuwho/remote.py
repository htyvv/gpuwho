"""SSH connections and the agent protocol.

Each server gets one SSH connection and one long-lived agent: probe.py, compressed and
piped through `python3 -c`, so nothing is installed on the server. The client writes a
newline, the agent answers with one JSON line.

Connection settings come from the server entry first, then from ~/.ssh/config (through
`ssh -G`, so Include and Match work), like `ssh` itself would resolve them.
"""
import base64
import errno
import json
import logging
import os
import random
import select
import shlex
import socket
import subprocess
import threading
import time
import warnings
import zlib

warnings.filterwarnings("ignore", module="paramiko")
warnings.filterwarnings("ignore", message=".*TripleDES.*")
# paramiko's transport thread logs tracebacks to stderr on transient SSH errors, which would
# scribble over the live dashboard; errors are reported per server in the UI instead.
logging.getLogger("paramiko").addHandler(logging.NullHandler())
logging.getLogger("paramiko").propagate = False

import paramiko  # noqa: E402

from gpuwho import config  # noqa: E402
from gpuwho.config import LOCAL_HOSTS, ME  # noqa: E402

with open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "probe.py")) as _f:
    PROBE = _f.read()

SSH_CONFIG = os.path.expanduser("~/.ssh/config")
MAX_BACKOFF = 60
_kh_lock = threading.Lock()
_connecting = threading.BoundedSemaphore(8)  # handshakes in flight at once
_ssh_cache = {}

# retrying these only gets the account locked or fail2ban'd: wait for the user to press r
NEEDS_USER = (paramiko.AuthenticationException, paramiko.BadHostKeyException)


# ---------------------------------------------------------------- where and how to connect

def parse_target(spec):
    """'alice@gpu1:2222' or '[fe80::1]:22' or 'gpu1' -> (user, host, port), None when absent."""
    user, at, rest = spec.rpartition("@")
    user = user if at else None
    host, port = rest, None
    if rest.startswith("[") and "]" in rest:
        host, _, tail = rest[1:].partition("]")
        if tail[1:].isdigit():
            port = int(tail[1:])
    elif rest.count(":") == 1:
        h, p = rest.split(":")
        if p.isdigit():
            host, port = h, int(p)
    return user or None, host, port


def parse_ssh_g(text):
    """`ssh -G` output -> {key: value}, with identityfile as a list."""
    out = {"identityfile": []}
    for line in text.splitlines():
        key, _, value = line.partition(" ")
        if key == "identityfile":
            out["identityfile"].append(value)
        elif key:
            out[key] = value
    return out


def ssh_settings(alias):
    """What `ssh ALIAS` would use (hostname, user, port, identityfile, proxyjump, ...).
    Asks `ssh -G`, which understands Include and Match; falls back to paramiko's parser."""
    if alias in _ssh_cache:
        return _ssh_cache[alias]
    settings = None
    cmd = ["ssh", "-G", alias]
    if SSH_CONFIG != os.path.expanduser("~/.ssh/config"):  # tests point it elsewhere
        cmd[1:1] = ["-F", SSH_CONFIG]
    try:
        out = subprocess.run(cmd, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                             stderr=subprocess.DEVNULL, timeout=5)
        if out.returncode == 0:
            settings = parse_ssh_g(out.stdout.decode("utf-8", "replace"))
    except (OSError, subprocess.SubprocessError):
        pass
    if settings is None:
        try:
            settings = dict(paramiko.SSHConfig.from_path(SSH_CONFIG).lookup(alias))
        except Exception:
            settings = {}
    _ssh_cache[alias] = settings
    return settings


def jump_command(jump, host, port):
    """ProxyJump 'a,b' -> an `ssh -W` ProxyCommand through those hosts (run by OpenSSH,
    which handles their keys, agent and known_hosts)."""
    hops = [h.strip() for h in jump.split(",") if h.strip()]
    args = ["ssh", "-o", "BatchMode=yes"]
    if len(hops) > 1:
        args += ["-J", ",".join(hops[:-1])]
    args += ["-W", "%s:%d" % (host, port), hops[-1]]
    return " ".join(shlex.quote(a) for a in args)


def connect_kwargs(cfg):
    """paramiko connect() arguments for a server entry: the entry wins, then ~/.ssh/config."""
    s = ssh_settings(cfg["host"])
    kw = {
        "hostname": s.get("hostname") or cfg["host"],
        "port": int(cfg.get("port") or s.get("port") or 22),
        "username": cfg.get("user") or s.get("user") or ME,
        "timeout": 8, "banner_timeout": 15, "auth_timeout": 15,
    }
    password = cfg.get("password")
    if password and not cfg.get("key"):
        # go straight to the password: sshd drops you after MaxAuthTries (6) keys
        kw.update(password=password, look_for_keys=False, allow_agent=False)
    else:
        keys = [cfg["key"]] if cfg.get("key") else s.get("identityfile") or []
        if isinstance(keys, str):
            keys = [keys]
        keys = [os.path.expanduser(k) for k in keys if os.path.exists(os.path.expanduser(k))]
        only = str(s.get("identitiesonly", "no")).lower() == "yes"
        kw.update(key_filename=keys or None, password=password or None, look_for_keys=not only, allow_agent=not only)
    jump, proxy = s.get("proxyjump"), s.get("proxycommand")
    if jump and jump.lower() != "none":
        kw["sock"] = paramiko.ProxyCommand(jump_command(jump, kw["hostname"], kw["port"]))
    elif proxy and proxy.lower() != "none":
        proxy = proxy.replace("%h", kw["hostname"]).replace("%p", str(kw["port"])).replace("%r", kw["username"])
        kw["sock"] = paramiko.ProxyCommand(proxy)
    return kw


def describe(e):
    """Short, human error for the dashboard (the exception type alone says little)."""
    msg = str(e).strip() or type(e).__name__
    if isinstance(e, paramiko.AuthenticationException):
        return "auth failed: %s" % msg
    if isinstance(e, paramiko.BadHostKeyException):
        return ("host key changed for %s: if expected, run `ssh-keygen -R %s` and remove it from %s"
                % (e.hostname, e.hostname, config.KNOWN_HOSTS))
    if isinstance(e, socket.gaierror):
        return "unknown host: %s" % msg
    if isinstance(e, paramiko.ssh_exception.NoValidConnectionsError):
        codes = set(getattr(err, "errno", None) for err in e.errors.values())
        return "connection refused" if codes <= {errno.ECONNREFUSED} else "unreachable: %s" % msg
    if isinstance(e, (socket.timeout, TimeoutError)):
        return "unreachable (no answer in time)" if "answer within" not in msg else msg
    if isinstance(e, paramiko.SSHException) and "banner" in msg:
        return "not answering SSH (%s)" % msg
    return "%s: %s" % (type(e).__name__, msg[:200])


# ---------------------------------------------------------------- host keys

class TrustOnFirstUse(paramiko.MissingHostKeyPolicy):
    """Like `StrictHostKeyChecking=accept-new`: remember unknown keys, reject changed ones.
    New keys are appended as one line, so two gpuwho processes sharing an NFS home
    can't overwrite each other's entries."""

    def missing_host_key(self, client, hostname, key):
        line = "%s %s %s\n" % (hostname, key.get_name(), key.get_base64())
        with _kh_lock:
            fd = os.open(config.KNOWN_HOSTS, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
            try:
                os.write(fd, line.encode())
            finally:
                os.close(fd)


def agent_cmd(idle_sec):
    """Shell command that starts probe.py as a long-lived agent (stdin stays free for requests)."""
    b64 = base64.b64encode(zlib.compress(PROBE.encode())).decode()
    return "exec python3 -u -c \"import base64,zlib;exec(zlib.decompress(base64.b64decode('%s')))\" %d" % (
        b64, idle_sec)


# ---------------------------------------------------------------- one server

class Server:
    """One monitored host: an SSH connection plus a remote agent answering one JSON line per request."""

    REQUEST_TIMEOUT = 40  # the agent gives up on a stuck NVML call after 15 s, so this only trips on a dead link

    def __init__(self, cfg, idle_sec=90):
        self.cfg = cfg
        self.name = cfg["name"]
        self.idle_sec = int(idle_sec)
        self.client = None  # paramiko.SSHClient
        self.chan = None    # remote agent channel
        self.proc = None    # local agent process (host == local)
        self.buf = b""
        self.data = None
        self.error = None
        self.updated = None
        self.latency = None
        self.version = 0    # bumped after every poll, lets the UI redraw only on change
        self.fails = 0
        self.retry_at = 0.0
        self.blocked = False  # auth / host key problem: wait for the user

    @property
    def is_local(self):
        return self.cfg["host"] in LOCAL_HOSTS

    def due(self):
        return not self.blocked and time.time() >= self.retry_at

    def retry(self):
        """Forget the backoff and try again on the next poll (the r key)."""
        self.blocked, self.retry_at = False, 0.0

    def _connect(self):
        c = paramiko.SSHClient()
        try:
            c.load_system_host_keys()
            with _kh_lock:
                c.get_host_keys().load(config.KNOWN_HOSTS)
            c.set_missing_host_key_policy(TrustOnFirstUse())
            with _connecting:
                c.connect(**connect_kwargs(self.cfg))
            c.get_transport().set_keepalive(30)
            return c
        except BaseException:
            c.close()  # a failed handshake leaves the transport thread and socket open otherwise
            raise

    # -- agent lifecycle

    def _agent_alive(self):
        if self.is_local:
            return self.proc is not None and self.proc.poll() is None
        return self.chan is not None and not self.chan.closed and not self.chan.exit_status_ready()

    def _start_agent(self):
        self._close_agent()
        if self.is_local:
            self.proc = subprocess.Popen(["python3", "-u", "-c", PROBE, str(self.idle_sec)],
                                         stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            return
        transport = self.client.get_transport() if self.client else None
        if transport is None or not transport.is_active():
            self.close()
            self.client = self._connect()
            transport = self.client.get_transport()
        self.chan = transport.open_session(timeout=10)
        self.chan.exec_command(agent_cmd(self.idle_sec))

    def _agent_stderr(self):
        try:
            if self.is_local:
                err = self.proc.stderr.read() if self.proc.poll() is not None else b""
            else:
                err = b""
                while self.chan.recv_stderr_ready():
                    err += self.chan.recv_stderr(65536)
            lines = err.decode(errors="replace").strip().splitlines()
            return lines[-1][:200] if lines else ""
        except Exception:
            return ""

    def _request(self):
        if self.is_local:
            self.proc.stdin.write(b"\n")
            self.proc.stdin.flush()
        else:
            self.chan.sendall(b"\n")

    def _recv_line(self, deadline):
        while b"\n" not in self.buf:
            left = deadline - time.time()
            if left <= 0:
                raise TimeoutError("no answer within %ds" % self.REQUEST_TIMEOUT)
            if self.is_local:
                ready = select.select([self.proc.stdout], [], [], left)[0]
                chunk = os.read(self.proc.stdout.fileno(), 65536) if ready else None
            else:
                self.chan.settimeout(left)
                try:
                    chunk = self.chan.recv(65536)
                except socket.timeout:
                    chunk = None
            if chunk is None:
                continue
            if not chunk:
                raise RuntimeError(self._agent_stderr() or "agent exited")
            self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        return line.decode(errors="replace")

    def probe(self):
        t0 = time.time()
        try:
            if not self._agent_alive():
                self._start_agent()
            self._request()
            deadline = t0 + self.REQUEST_TIMEOUT
            line = self._recv_line(deadline)
            while not line.startswith("{"):  # rc files may print noise before the agent starts
                line = self._recv_line(deadline)
            data = json.loads(line)
            if "fatal" in data:  # the agent is fine, one sample wasn't: keep the connection
                self.error = "agent: %s" % data["fatal"]
            else:
                self.data, self.error = data, None
                self.updated = time.time()
                self.latency = time.time() - t0
            self.fails, self.retry_at = 0, 0.0
        except Exception as e:  # keep the dashboard alive no matter what one host does
            self.error = describe(e)
            self.fails += 1
            if isinstance(e, NEEDS_USER):
                self.blocked = True
            else:  # 2, 4, 8 ... 60 s, jittered so a rack coming back isn't hit all at once
                self.retry_at = time.time() + min(MAX_BACKOFF, 2 ** self.fails) * random.uniform(0.8, 1.2)
            self.close()
        self.version += 1
        return self

    def _close_agent(self):
        """Closing stdin/the channel makes the agent see EOF and exit."""
        if self.proc is not None:
            try:
                self.proc.stdin.close()
                self.proc.wait(timeout=3)
            except Exception:
                self.proc.kill()
            self.proc = None
        if self.chan is not None:
            try:
                self.chan.close()
            except Exception:
                pass
            self.chan = None
        self.buf = b""

    def close(self):
        self._close_agent()
        if self.client is not None:
            try:
                self.client.close()
            except Exception:
                pass
        self.client = None
