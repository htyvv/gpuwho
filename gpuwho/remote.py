"""SSH connections and the agent protocol.

Each server gets one SSH connection and one long-lived agent: probe.py, compressed and
piped through `python3 -c`, so nothing is installed on the server. The client writes a
newline, the agent answers with one JSON line.
"""
import base64
import json
import logging
import os
import select
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

_kh_lock = threading.Lock()


class TrustOnFirstUse(paramiko.MissingHostKeyPolicy):
    """Like `StrictHostKeyChecking=accept-new`: remember unknown keys, reject changed ones."""

    def missing_host_key(self, client, hostname, key):
        with _kh_lock:
            hk = paramiko.HostKeys()
            hk.load(config.KNOWN_HOSTS)
            hk.add(hostname, key.get_name(), key)
            hk.save(config.KNOWN_HOSTS)
            os.chmod(config.KNOWN_HOSTS, 0o600)


def agent_cmd(idle_sec):
    """Shell command that starts probe.py as a long-lived agent (stdin stays free for requests)."""
    b64 = base64.b64encode(zlib.compress(PROBE.encode())).decode()
    return "exec python3 -u -c \"import base64,zlib;exec(zlib.decompress(base64.b64decode('%s')))\" %d" % (
        b64, idle_sec)


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

    @property
    def is_local(self):
        return self.cfg["host"] in LOCAL_HOSTS

    def _connect(self):
        c = paramiko.SSHClient()
        c.load_system_host_keys()
        c.get_host_keys().load(config.KNOWN_HOSTS)
        c.set_missing_host_key_policy(TrustOnFirstUse())
        c.connect(
            self.cfg["host"], port=self.cfg.get("port", 22), username=self.cfg.get("user") or ME,
            password=self.cfg.get("password") or None, key_filename=self.cfg.get("key") or None,
            look_for_keys=True, allow_agent=True, timeout=8, banner_timeout=15, auth_timeout=15,
        )
        c.get_transport().set_keepalive(30)
        return c

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
            if "fatal" in data:
                raise RuntimeError(data["fatal"])
            self.data = data
            self.error = None
            self.updated = time.time()
            self.latency = time.time() - t0
        except Exception as e:  # keep the dashboard alive no matter what one host does
            self.error = "%s: %s" % (type(e).__name__, str(e)[:200] or "error")
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
