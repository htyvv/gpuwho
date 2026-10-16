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
