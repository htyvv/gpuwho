# gpuwho

[![CI](https://github.com/htyvv/gpuwho/actions/workflows/ci.yml/badge.svg)](https://github.com/htyvv/gpuwho/actions/workflows/ci.yml)
![Python 3.9+](https://img.shields.io/badge/python-3.9%2B-blue)
[![License: MIT](https://img.shields.io/badge/license-MIT-green)](LICENSE)

**nvidia-smi for every server in your lab, over plain SSH.**

One terminal shows every GPU on every machine, who is using it and how much.
Nothing gets installed on the servers: gpuwho pipes a small Python agent through SSH and reads NVML directly.

English | [简体中文](README.zh-CN.md) | [한국어](README.ko.md)

<p align="center">
  <img src="docs/demo.gif" alt="gpuwho live: two servers, sixteen GPUs, jobs finishing and starting, and who holds what" width="100%">
  <br>
  <sub>Made-up servers and users from <code>gpuwho demo</code>. A job finishes on node01, another starts on node03.</sub>
</p>

```sh
pipx install git+https://github.com/htyvv/gpuwho
gpuwho demo                   # try it first, no servers needed
gpuwho add gpu1 10.0.0.11     # then add yours
gpuwho
```

## What you see

- Every GPU on every server: utilization, memory, temperature, fan, power, and the encoder/decoder when they're busy.
- Who holds each GPU and how much memory they use. Docker jobs show up as `root@3f9c2a`, the container id.
- Which GPUs are free right now. A GPU only counts as idle with no processes, under 1 GiB used and under 5% load. A display server (Xorg, gnome-shell) parked on it doesn't count.
- One table per user across all servers: how many GPUs, how much GPU memory, where (`node01[0,1]`), CPU cores and RAM.
- When two jobs share a GPU, which one is computing (SM%) and which one only holds memory.
- CPU-only machines too, with load, RAM and the heaviest users.
- GPUs that fell off the bus ("GPU is lost") as a red row, so you know to call the admin. MIG GPUs are read per instance.

A wide terminal puts servers side by side. With more servers than fit, it switches to one line per server by itself (`c` toggles), and scrolls when even that doesn't fit.

<p align="center">
  <img src="docs/screenshot.png" alt="gpuwho with four servers side by side, including a CPU-only one and a lost GPU" width="100%">
</p>

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
gpuwho add gpu1 10.0.0.11                 # your SSH keys or ssh-agent
gpuwho add gpu2 alice@10.0.0.12 --password # asks for a password and stores it (mode 0600)
gpuwho add dgx dgx.example.org:2222 --key ~/.ssh/id_lab
gpuwho add lab-a100                       # a Host from ~/.ssh/config, ProxyJump included
gpuwho add here local                     # the machine you're on, no SSH
```

Anything you don't type (user, port, key, jump host) comes from `~/.ssh/config`, the way `ssh` would resolve it.

Then run `gpuwho`:

| key | |
|---|---|
| `q` | quit |
| `p` | process table (PID, user, GPU memory, SM%, CPU%, RAM, command) |
| `c` | compact view, one line per server |
| `j` `k` `↑` `↓` `PgUp` `PgDn` `g` `G` | scroll |
| `r` | retry failed servers now |
| `space` | pause |
| `?` | help |

```
gpuwho                       live dashboard, refreshes every 3 s
gpuwho -i 1 -p               every second, with the process table
gpuwho once                  one snapshot, then exit (also what you get under watch, cron or a pipe)
gpuwho once --json           the same as JSON
gpuwho test                  check every connection, exit 1 if one fails
gpuwho list | remove gpu2
```

### Grab a free GPU

```sh
gpuwho free                          # every idle GPU, most free memory first
gpuwho free --min-free 40            # GPUs with at least 40 GiB free and under 50% load
CUDA_VISIBLE_DEVICES=$(gpuwho free gpu1 -n 2 --ids) python train.py
```

`gpuwho free` exits with 1 when there aren't enough, so it works in scripts and job launchers.

## How it works

- gpuwho opens one SSH connection per server ([paramiko](https://www.paramiko.org/)) and starts [`probe.py`](gpuwho/probe.py) there with `python3 -c`. The script travels compressed inside the command, so nothing is copied to disk.
- The agent stays up for the session. Each refresh, gpuwho sends a newline and gets one line of JSON back. No new SSH handshake, no new process per poll.
- GPU data comes from NVML through `ctypes`, the library behind `nvidia-smi`. It never runs `nvidia-smi` and never parses its output.
- Process owners come from `/proc`, containers from `/proc/PID/cgroup` (Docker, containerd, Podman, CRI-O).
- RAM per user is what the kernel charged to that user's cgroups (anonymous plus shared memory), not a sum of RSS. Summing RSS counts the pages that forked DataLoader workers share with their parent once per worker; on one of our servers it said 1.4 TB for a user who held 220 GB.
- The agent is plain stdlib Python and runs on 3.6+. CI checks that on Python 3.6, 3.7 and 3.8 with the exact command gpuwho sends.
- It runs as your user, needs no root, and exits when the SSH session closes, after 90 s without a request, or after 6 hours.
- A poll costs about 45 ms of CPU on a server with 1,700 processes and 8 GPUs, about 1.5% of one core at the default 3 s interval. All of `/proc` is read every fifth poll; in between only the processes that mattered.
- A dead server is retried after 2, 4, 8 ... up to 60 s. A wrong password or a changed host key is not retried until you press `r`, so nobody's fail2ban gets triggered.

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
| [svtop](https://pypi.org/project/svtop/) | yes | nothing (runs `nvidia-smi`) | yes | terminal |
| [gpustat-web](https://github.com/wookayin/gpustat-web) | yes | gpustat | yes | browser |
| [all-smi](https://github.com/lablup/all-smi) | yes | nothing in SSH mode, `all-smi api` for full data | not in SSH mode | terminal |
| [cluster-smi](https://github.com/PatWie/cluster-smi) | yes | a node daemon | yes | terminal |
| [gpuview](https://github.com/fgaim/gpuview) | yes | the gpuview service | partly | browser |
| [DCGM exporter](https://github.com/NVIDIA/dcgm-exporter) + Grafana | yes | exporter, Prometheus | Kubernetes pods only | browser |

Use something else when:

- You work on one machine and want to act on it: kill processes, see a tree, scroll back through history graphs. Use **nvitop**.
- Your servers have no `python3`, or you want a file transfer pane next to the monitor. Use **svtop**.
- You have AMD, Intel, Apple or other accelerators. Use **nvtop** or **all-smi**. gpuwho is NVIDIA only.
- You run a large fleet and want NVLink topology, recording or alerts. Use **all-smi**.
- You need history, alerting or Kubernetes. Use **DCGM exporter** with Prometheus and Grafana, or Netdata. gpuwho keeps nothing; it only shows now.
- People want a web page or a desktop status bar instead of a terminal. Use **gpustat-web**, **gpuview** or [Multiple-smi](https://github.com/ClementPinard/Multiple-smi).

If gpuwho saves you a round of `ssh`, a star helps other labs find it.

## FAQ

**Does it work with AMD or Intel GPUs?**
No, NVIDIA only (it talks to NVML). Servers without NVIDIA GPUs still show CPU, RAM and users.

**Does it read `~/.ssh/config`? Jump hosts?**
Yes. gpuwho asks `ssh -G` for the resolved settings, so `Include`, `Match`, `ProxyJump` and `ProxyCommand` work. Jump hosts are reached through your `ssh` with keys or the agent; a stored password is only sent to the final server.

**Is storing a password safe?**
It's stored in plain text in `~/.config/gpuwho/servers.json`, which gpuwho forces to mode 0600 on every start. Keys are better when you can use them. See [SECURITY.md](SECURITY.md).

**How much does it load the servers?**
About 1.5% of one core per server at the default interval, and about 30 MB of RAM for the agent. See [How it works](#how-it-works).

**Can I use it on Windows?**
Not natively. It runs fine inside WSL.

## Troubleshooting

**`auth failed: Authentication failed.`**
Wrong password, or your key isn't in the server's `authorized_keys`. Re-add with `gpuwho add NAME HOST --password --force`, then press `r`.

**`RuntimeError: bash: line 1: exec: python3: not found`**
The server has no `python3` on the PATH of a non-interactive shell. Install it, or make sure your `~/.bashrc` sets PATH before it returns early for non-interactive shells.

**`host key changed for ...`**
The server's host key changed since it was first seen. If you know why (reinstall), run `ssh-keygen -R HOST` and delete its line in `~/.config/gpuwho/known_hosts`.

**The user column shows `? (hidden)` or `? (gone)`**
`hidden`: `/proc` on that server is mounted with `hidepid`, so other users' processes are hidden from you. `gone`: the process has exited but the driver still holds its memory, usually DataLoader workers of a killed job.

**`process info N/A`**
NVML said it can't list processes for that GPU. Utilization and memory still work.

**A user's RAM differs from what `ps` or `htop` add up to**
gpuwho reports the memory the kernel charged to the user's cgroup, which includes their files in `/dev/shm` and leaves out page cache. The process table still shows plain RSS per process.

## Development

```sh
git clone https://github.com/htyvv/gpuwho && cd gpuwho
python -m pip install -e . pytest ruff
python -m pytest
ruff check .
```

Tests run against the made-up cluster in [`gpuwho/demo.json`](gpuwho/demo.json), plus a real agent on your own machine. `python scripts/screenshot.py docs/screenshot.png` and `python scripts/screenshot.py docs/demo.gif --gif --width 140 --servers node01,node03 --size 20` redraw the images from the same data (needs Pillow).

## Acknowledgements

The NVML handling follows [gpustat](https://github.com/wookayin/gpustat) and [nvitop](https://github.com/XuehaiPan/nvitop): process API fallbacks, memory v2, lost GPUs, per-process utilization. The terminal UI is [Rich](https://github.com/Textualize/rich).

## License

[MIT](LICENSE)
