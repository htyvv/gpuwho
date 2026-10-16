"""Server list and host keys, kept private.

servers.json may hold passwords, so the directory is forced to 0700 and every file in it
to 0600 each time it is loaded, and files owned by somebody else are refused.
Set GPUWHO_CONFIG_DIR to keep them somewhere other than ~/.config/gpuwho.
"""
import getpass
import json
import os
import stat
import sys

from rich.console import Console

CONF_DIR = os.environ.get("GPUWHO_CONFIG_DIR") or os.path.expanduser("~/.config/gpuwho")
CONF_PATH = os.path.join(CONF_DIR, "servers.json")
KNOWN_HOSTS = os.path.join(CONF_DIR, "known_hosts")
LOCAL_HOSTS = {"local", "localhost", "127.0.0.1"}
ME = getpass.getuser()

_err = Console(stderr=True)


def ensure_private(path, mode):
    """Make sure `path` is owned by us and not readable by group/others."""
    st = os.stat(path)
    if st.st_uid != os.getuid():
        sys.exit("refusing to use %s: owned by uid %d, not you" % (path, st.st_uid))
    if stat.S_IMODE(st.st_mode) != mode:
        os.chmod(path, mode)
        _err.print("[yellow]fixed permissions on %s -> %o[/]" % (path, mode))


def write_private(path, content):
    tmp = path + ".tmp"
    fd = os.open(tmp, os.O_CREAT | os.O_WRONLY | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as f:
        f.write(content)
    os.chmod(tmp, 0o600)
    os.replace(tmp, path)


def init_conf_dir():
    os.makedirs(CONF_DIR, mode=0o700, exist_ok=True)
    ensure_private(CONF_DIR, 0o700)
    for path in (CONF_PATH, KNOWN_HOSTS):
        if not os.path.exists(path):
            os.close(os.open(path, os.O_CREAT | os.O_WRONLY, 0o600))
            if path == CONF_PATH:
                write_private(CONF_PATH, json.dumps({"servers": []}, indent=2))
        ensure_private(path, 0o600)


def load_conf():
    init_conf_dir()
    with open(CONF_PATH) as f:
        text = f.read().strip()
    return json.loads(text) if text else {"servers": []}


def save_conf(conf):
    write_private(CONF_PATH, json.dumps(conf, indent=2, ensure_ascii=False) + "\n")
