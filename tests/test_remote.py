import base64
import json
import socket
import subprocess
import threading
import zlib

from gpuwho import remote
from tests.conftest import linux_only


def test_agent_cmd_carries_the_probe():
    cmd = remote.agent_cmd(90)
    assert cmd.startswith("exec python3 -u -c ") and cmd.endswith(" 90")
    b64 = cmd.split("b64decode('")[1].split("'")[0]
    assert zlib.decompress(base64.b64decode(b64)).decode() == remote.PROBE


@linux_only
def test_agent_cmd_runs_in_a_shell():
    """The exact command string sent over SSH, run by a local shell."""
    agent = subprocess.Popen(["sh", "-c", remote.agent_cmd(30)], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    try:
        agent.stdin.write(b"\n")
        agent.stdin.flush()
        assert json.loads(agent.stdout.readline())["cpu"]["count"] > 0
        agent.stdin.close()
        assert agent.wait(timeout=10) == 0
    finally:
        agent.kill()


@linux_only
def test_local_server_reuses_its_agent():
    s = remote.Server({"name": "here", "host": "local"}, idle_sec=30)
    try:
        assert s.probe().error is None
        pid = s.data["agent"]["pid"]
        assert s.probe().error is None
        assert s.data["agent"]["pid"] == pid
        assert s.version == 2
    finally:
        s.close()
    assert s.proc is None


def test_broken_host_reports_an_error(monkeypatch, conf_dir):
    """Something that accepts TCP but never speaks SSH: the error lands on the server, not in a traceback."""
    monkeypatch.setattr(remote, "LOCAL_HOSTS", set())
    conf_dir.mkdir(mode=0o700)
    (conf_dir / "known_hosts").touch(mode=0o600)
    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)

    def hang_up():
        conn, _ = listener.accept()
        conn.sendall(b"not ssh\r\n")
        conn.close()

    threading.Thread(target=hang_up, daemon=True).start()
    s = remote.Server({"name": "bad", "host": "127.0.0.1", "port": listener.getsockname()[1], "user": "nobody"})
    s.probe()
    listener.close()
    assert s.data is None and s.error
    assert s.version == 1 and s.client is None


import paramiko  # noqa: E402
import pytest  # noqa: E402


@pytest.mark.parametrize("spec, expected", [
    ("gpu1", (None, "gpu1", None)),
    ("alice@gpu1", ("alice", "gpu1", None)),
    ("alice@gpu1:2222", ("alice", "gpu1", 2222)),
    ("10.0.0.11:22", (None, "10.0.0.11", 22)),
    ("[fe80::1]:2200", (None, "fe80::1", 2200)),
    ("fe80::1", (None, "fe80::1", None)),
    ("me@host:notaport", ("me", "host:notaport", None)),
])
def test_parse_target(spec, expected):
    assert remote.parse_target(spec) == expected


def test_parse_ssh_g():
    out = remote.parse_ssh_g("user alice\nhostname 10.0.0.5\nport 2222\nidentityfile ~/.ssh/a\nidentityfile ~/.ssh/b\n")
    assert out == {"user": "alice", "hostname": "10.0.0.5", "port": "2222", "identityfile": ["~/.ssh/a", "~/.ssh/b"]}


@pytest.fixture
def ssh_config(tmp_path, monkeypatch):
    cfg = tmp_path / "ssh_config"
    cfg.write_text("Host lab-*\n  User alice\n  Port 2222\n  ProxyJump bastion.example.org\n"
                   "Host pw\n  HostName 10.9.9.9\n")
    monkeypatch.setattr(remote, "SSH_CONFIG", str(cfg))
    monkeypatch.setattr(remote, "_ssh_cache", {})
    spawned = []

    class FakeProxy:
        def __init__(self, cmd):
            spawned.append(cmd)

    monkeypatch.setattr(remote.paramiko, "ProxyCommand", FakeProxy)
    return spawned


def test_ssh_config_fills_in_what_the_entry_leaves_out(ssh_config):
    kw = remote.connect_kwargs({"name": "g1", "host": "lab-g1"})
    assert (kw["hostname"], kw["port"], kw["username"]) == ("lab-g1", 2222, "alice")
    assert ssh_config == ["ssh -o BatchMode=yes -W lab-g1:2222 bastion.example.org"]
    kw = remote.connect_kwargs({"name": "g1", "host": "lab-g1", "user": "bob", "port": 22})
    assert (kw["port"], kw["username"]) == (22, "bob")  # typed values win


def test_password_goes_first(ssh_config):
    kw = remote.connect_kwargs({"name": "p", "host": "pw", "password": "secret"})
    assert kw["hostname"] == "10.9.9.9" and kw["password"] == "secret"
    assert kw["look_for_keys"] is False and kw["allow_agent"] is False


def test_jump_chain():
    assert remote.jump_command("a,b", "10.0.0.1", 22) == "ssh -o BatchMode=yes -J a -W 10.0.0.1:22 b"


def test_describe():
    assert remote.describe(paramiko.AuthenticationException("Authentication failed.")).startswith("auth failed")
    assert remote.describe(socket.gaierror(-2, "Name or service not known")).startswith("unknown host")
    refused = paramiko.ssh_exception.NoValidConnectionsError({("10.0.0.1", 22): ConnectionRefusedError(111, "x")})
    assert remote.describe(refused) == "connection refused"
    assert remote.describe(TimeoutError()) == "unreachable (no answer in time)"


def test_backoff_and_blocking(monkeypatch, conf_dir):
    s = remote.Server({"name": "x", "host": "nowhere"})

    def refused():
        raise paramiko.ssh_exception.NoValidConnectionsError({("x", 22): ConnectionRefusedError(111, "x")})

    monkeypatch.setattr(s, "_connect", refused)
    s.probe()
    assert s.error == "connection refused" and not s.due() and not s.blocked
    s.retry()
    assert s.due()

    def bad_password():
        raise paramiko.AuthenticationException("Authentication failed.")

    monkeypatch.setattr(s, "_connect", bad_password)
    s.probe()
    assert s.blocked and not s.due()  # never hammer sshd with a wrong password
    s.retry()
    assert s.due()


def test_failed_handshake_closes_the_client(monkeypatch, conf_dir):
    conf_dir.mkdir(mode=0o700)
    (conf_dir / "known_hosts").touch(mode=0o600)
    closed = []
    monkeypatch.setattr(remote.paramiko.SSHClient, "connect",
                        lambda self, **kw: (_ for _ in ()).throw(paramiko.SSHException("boom")))
    monkeypatch.setattr(remote.paramiko.SSHClient, "close", lambda self: closed.append(self))
    monkeypatch.setattr(remote, "connect_kwargs", lambda cfg: {})
    with pytest.raises(paramiko.SSHException):
        remote.Server({"name": "x", "host": "h"})._connect()
    assert len(closed) == 1


def test_new_host_keys_are_appended(conf_dir):
    conf_dir.mkdir(mode=0o700)
    kh = conf_dir / "known_hosts"
    kh.write_text("")
    key = paramiko.RSAKey.generate(1024)
    remote.TrustOnFirstUse().missing_host_key(None, "[10.0.0.1]:2222", key)
    remote.TrustOnFirstUse().missing_host_key(None, "10.0.0.2", key)
    keys = paramiko.HostKeys(str(kh))
    assert keys.lookup("[10.0.0.1]:2222") and keys.lookup("10.0.0.2")
