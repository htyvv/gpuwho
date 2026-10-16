# gpuwho

[![CI](https://github.com/htyvv/gpuwho/actions/workflows/ci.yml/badge.svg)](https://github.com/htyvv/gpuwho/actions/workflows/ci.yml)
![Python 3.9+](https://img.shields.io/badge/python-3.9%2B-blue)
[![License: MIT](https://img.shields.io/badge/license-MIT-green)](LICENSE)

**nvidia-smi for every server in your lab, over plain SSH.**

One terminal shows every GPU on every machine, who is using it and how much.
Nothing gets installed on the servers: gpuwho pipes a small Python agent through SSH and reads NVML directly.

English | [한국어](README.ko.md)

<p align="center">
  <img src="docs/screenshot.png" alt="gpuwho showing four servers, twenty GPUs and the users on them" width="100%">
  <br>
  <sub>Four servers, 20 GPUs, one terminal. Made-up data, rendered by <code>scripts/screenshot.py</code>.</sub>
</p>

```sh
pipx install git+https://github.com/htyvv/gpuwho
gpuwho add gpu1 10.0.0.11
gpuwho
```

## What you see

- Every GPU on every server: utilization, memory, temperature, fan, power, and the encoder/decoder when they're busy.
- Who holds each GPU and how much memory they use. Docker jobs show up as `root@3f9c2a`, the container id.
- Which GPUs are free right now. A GPU only counts as idle with no processes, under 1 GiB used and under 5% load. A display server (Xorg, gnome-shell) parked on it doesn't count.
- One table per user across all servers: how many GPUs, how much GPU memory, where (`node01[0,1]`), CPU cores and RAM.
- CPU-only machines too, with load, RAM and the heaviest users.
- A process table with PID, user, GPU memory, CPU%, RAM and command line (`-p`, or press `p`).
- GPUs that fell off the bus ("GPU is lost") as a red row, so you know to call the admin.

## Install

You need Python 3.9+ on the machine you watch from (Linux or macOS), and SSH access to the servers.
The servers need Linux, an NVIDIA driver and any `python3` from 3.6 up. That's it.

```sh
pipx install git+https://github.com/htyvv/gpuwho
# or
uv tool install git+https://github.com/htyvv/gpuwho
```

If your servers share a home directory over NFS, as many lab clusters do, install it once and run `gpuwho` from any of them. The server list lives in your home too.

## Usage

Register each server once. gpuwho connects right away to check that it works, then saves it.

```sh
gpuwho add gpu1 10.0.0.11                      # your SSH keys or ssh-agent
gpuwho add gpu2 10.0.0.12 -u alice --password  # asks for a password and stores it (mode 0600)
gpuwho add dgx dgx.example.org -P 2222 --key ~/.ssh/id_lab
gpuwho add here local                          # the machine you're on, no SSH
```

Then just run `gpuwho`. Press `p` to toggle the process table, `q` to quit.

```
gpuwho                 live dashboard, refreshes every 3 s
gpuwho -i 1 -p         every second, with the process table
gpuwho once            one snapshot, then exit (good for scripts and cron)
gpuwho once --json     the same as JSON
gpuwho once gpu1 gpu2  only some servers
gpuwho test            check every connection
gpuwho list            show registered servers
gpuwho remove gpu2
```

`gpuwho once --json` gives you everything the dashboard knows, so finding a free GPU from a script is short:

```sh
gpuwho once --json | jq -r 'to_entries[] | .key as $s | .value.gpus[]?
  | select(.invalid != true and .mem_used < 1024 and .util < 5) | "\($s) gpu\(.index)"'
```

## How it works

- gpuwho opens one SSH connection per server ([paramiko](https://www.paramiko.org/)) and starts [`probe.py`](gpuwho/probe.py) there with `python3 -c`. The script travels compressed inside the command, so nothing is copied to disk.
- The agent stays up for the session. Each refresh, gpuwho sends a newline and gets one line of JSON back. No new SSH handshake, no new process per poll.
- GPU data comes from NVML through `ctypes`, the library behind `nvidia-smi`. It never runs `nvidia-smi` and never parses its output.
- Process owners come from `/proc`, containers from `/proc/PID/cgroup` (Docker, containerd, Podman, CRI-O).
- The agent is plain stdlib Python and runs on 3.6+. CI checks that on Python 3.6, 3.7 and 3.8 with the exact command gpuwho sends.
- It runs as your user, needs no root, and exits when the SSH session closes, after 90 s without a request, or after 6 hours.
- A poll costs about 70 ms of CPU on a server with 1,700 processes and 8 GPUs, so roughly 2% of one core at the default 3 s interval.
- If the NVIDIA driver hangs, the NVML call times out after 15 s and you still get CPU and RAM for that server.

## Why

Our lab has several GPU servers and one shared home directory. Finding a free GPU used to mean `ssh` into each box, run `nvidia-smi`, then `ps` to work out who owns PID 812345. gpuwho does that loop for every server, every three seconds, in one window.

## Alternatives

There are good GPU monitors already. This is how the ones I looked at differ, going by their READMEs and source in October 2026.

| | Many servers in one view | Needs on each server | Shows who owns each GPU process | Interface |
|---|---|---|---|---|
| **gpuwho** | yes | `python3` (nothing installed) | yes, Docker jobs included | terminal |
| [nvitop](https://github.com/XuehaiPan/nvitop) | no, one host (`ssh -t host nvitop`) | nvitop | yes | terminal |
| [nvtop](https://github.com/Syllo/nvtop) | no, one host | nvtop | yes | terminal |
| [gpustat](https://github.com/wookayin/gpustat) | no, one host | gpustat | yes | terminal |
| [gpustat-web](https://github.com/wookayin/gpustat-web) | yes | gpustat | yes | browser |
| [all-smi](https://github.com/lablup/all-smi) | yes | nothing in SSH mode, `all-smi api` for full data | not in SSH mode | terminal |
| [cluster-smi](https://github.com/PatWie/cluster-smi) | yes | a node daemon | yes | terminal |
| [gpuview](https://github.com/fgaim/gpuview) | yes | the gpuview service | partly | browser |
| [DCGM exporter](https://github.com/NVIDIA/dcgm-exporter) + Grafana | yes | exporter, Prometheus | Kubernetes pods only | browser |

Use something else when:

- You work on one machine and want to act on it: kill processes, see a tree, scroll back through history graphs. Use **nvitop**.
- You have AMD, Intel, Apple or other accelerators. Use **nvtop** or **all-smi**. gpuwho is NVIDIA only.
- You run a large fleet and want NVLink topology, recording or alerts. Use **all-smi**.
- You need history, alerting or Kubernetes. Use **DCGM exporter** with Prometheus and Grafana, or Netdata. gpuwho keeps nothing; it only shows now.
- People want a web page instead of a terminal. Use **gpustat-web** or **gpuview**.

## FAQ

**Does it work with AMD or Intel GPUs?**
No, NVIDIA only (it talks to NVML). Servers without NVIDIA GPUs still show CPU, RAM and users.

**Does it read `~/.ssh/config`?**
Not yet. Pass the real host, `-u`, `-P` and `--key` to `gpuwho add`. A `ProxyJump` bastion isn't supported either.

**Is storing a password safe?**
It's stored in plain text in `~/.config/gpuwho/servers.json`, which gpuwho forces to mode 0600 on every start. Keys are better when you can use them. See [SECURITY.md](SECURITY.md).

**Can I use it on Windows?**
Not natively. It runs fine inside WSL.

## Troubleshooting

**`AuthenticationException: Authentication failed.`**
Wrong password, or your key isn't in the server's `authorized_keys`. Re-add with `gpuwho add NAME HOST --password --force`.

**`RuntimeError: bash: line 1: exec: python3: not found`**
The server has no `python3` on the PATH of a non-interactive shell. Install it, or make sure your `~/.bashrc` sets PATH before it returns early for non-interactive shells.

**`BadHostKeyException: Host key for server ... does not match`**
The server's host key changed since gpuwho first saw it. If you know why (reinstall), delete its line in `~/.config/gpuwho/known_hosts`.

**The user column shows `?`**
`/proc` on that server is mounted with `hidepid`, so other users' processes are hidden from you.

**`process info N/A`**
NVML said it can't list processes for that GPU. Utilization and memory still work.

**A user's RAM is larger than the machine**
RAM per user is the sum of each process's RSS, and memory shared between processes (PyTorch DataLoader workers, for example) is counted once per process. Treat it as an upper bound.

## Development

```sh
git clone https://github.com/htyvv/gpuwho && cd gpuwho
python -m pip install -e . pytest ruff
python -m pytest
ruff check .
```

Tests run against a made-up cluster in [`tests/fixtures/cluster.json`](tests/fixtures/cluster.json), plus a real agent on your own machine. `python scripts/screenshot.py docs/screenshot.png` redraws the screenshot from the same data (needs Pillow).

## Acknowledgements

The NVML handling follows [gpustat](https://github.com/wookayin/gpustat): process API fallbacks, memory v2, dealing with lost GPUs. The terminal UI is [Rich](https://github.com/Textualize/rich).

## License

[MIT](LICENSE)
