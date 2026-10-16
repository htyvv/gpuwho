import os
import stat

from gpuwho import config


def mode(path):
    return stat.S_IMODE(os.stat(path).st_mode)


def test_first_load_creates_private_files(conf_dir):
    assert config.load_conf() == {"servers": []}
    assert mode(conf_dir) == 0o700
    assert mode(conf_dir / "servers.json") == 0o600
    assert mode(conf_dir / "known_hosts") == 0o600


def test_loose_permissions_are_fixed(conf_dir):
    config.load_conf()
    os.chmod(conf_dir / "servers.json", 0o644)
    os.chmod(conf_dir, 0o755)
    config.load_conf()
    assert mode(conf_dir / "servers.json") == 0o600
    assert mode(conf_dir) == 0o700


def test_save_roundtrip(conf_dir):
    conf = {"servers": [{"name": "gpu1", "host": "10.0.0.1", "port": 22, "user": "me", "password": "pw"}]}
    config.load_conf()
    config.save_conf(conf)
    assert config.load_conf() == conf
    assert mode(conf_dir / "servers.json") == 0o600
    assert not (conf_dir / "servers.json.tmp").exists()


def test_empty_file_means_no_servers(conf_dir):
    config.load_conf()
    (conf_dir / "servers.json").write_text("")
    assert config.load_conf() == {"servers": []}
