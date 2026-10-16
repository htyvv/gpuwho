# gpuwho

[![CI](https://github.com/htyvv/gpuwho/actions/workflows/ci.yml/badge.svg)](https://github.com/htyvv/gpuwho/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/htyvv/gpuwho)](https://github.com/htyvv/gpuwho/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-green)](LICENSE)

**nvidia-smi for every server in your lab, over plain SSH.**

One terminal shows every GPU on every machine, who is using it, and how much.
Nothing gets installed on the servers: gpuwho copies a 600 KB agent over SSH (or runs a Python one with `python3`) and reads NVML directly.

English | [简体中文](README.zh-CN.md) | [한국어](README.ko.md)

<p align="center">
  <img src="docs/demo.gif" alt="gpuwho live: two servers, sixteen GPUs, jobs finishing and starting, and who holds what" width="100%">
  <br>
  <sub>Made-up servers and users from <code>gpuwho demo</code>. A job finishes on node01, another starts on node03.</sub>
</p>

```sh
curl -fsSL https://raw.githubusercontent.com/htyvv/gpuwho/main/install.sh | sh
gpuwho demo                   # try it first, no servers needed
gpuwho add gpu1 10.0.0.11     # then add yours
gpuwho
```

## What you see

- Every GPU on every server: utilization, memory, temperature, fan, power, and the encoder/decoder when they're busy.
- Who holds each GPU and how much memory they use. Docker jobs show up as `root@3f9c2a`, the container id.
- Which GPUs are free right now. A GPU counts as idle with no processes, under 1 GiB used and under 5% load. A display server (Xorg, gnome-shell) parked on it doesn't count.
- Who is holding a GPU without using it: a job that has kept ≥ 1 GiB at ~0% for 30 minutes gets an `idle 2h` tag, and the per-user table a *Held idle* column.
- When two jobs share a GPU, which one is computing (SM%) and which one only holds memory.
- One table per user across all servers: GPUs, GPU memory, where (`node01[0,1]`), CPU cores and RAM. RAM is what the kernel charged to the user's cgroups, so forked DataLoader workers aren't counted 30 times.
- GPU memory no process accounts for, processes that look like crypto miners, people over the lab's GPU limit (`--max-gpus 2`), GPUs that fell off the bus, MIG instances.

A wide terminal puts servers side by side. With more servers than fit it switches to one line per server by itself (`c` toggles), and scrolls when even that doesn't fit.

<p align="center">
  <img src="docs/screenshot.png" alt="gpuwho with four servers side by side, including a CPU-only one and a lost GPU" width="100%">
</p>

## Sharing a lab

The part after "who is using it": waiting, reserving, cleaning up.

```sh
gpuwho free                                   # every idle GPU, most free memory first
gpuwho free gpu1 -n 2 --wait -- python train.py
                                              # on gpu1: wait for 2 free GPUs, then run with CUDA_VISIBLE_DEVICES set
gpuwho free -n 4 --wait --notify https://hooks.slack.com/services/...
                                              # or Discord, Feishu, WeCom, DingTalk, ntfy, Telegram
gpuwho claim gpu1:2,3 --for 6h --note "ICLR rebuttal"
gpuwho claims                                 # everyone sees claims; `free` skips GPUs claimed by others
gpuwho unclaim gpu1
gpuwho kill gpu1:3                            # your own processes on that GPU; lists them and asks first
```

Claims are notes, not locks: nothing is enforced, the same way a booking sheet isn't. They live on each server in `/var/tmp/gpuwho-claims` and belong to whoever owns the file, so nobody can claim as someone else.

```sh
gpuwho record usage.jsonl            # one sample a minute; run it in tmux, or `--once` from cron
gpuwho report usage.jsonl --since 7d # GPU-hours and idle-held hours per person
gpuwho once --html > ~/public_html/gpus.html   # a page for the lab, from cron
```

And for coding agents, gpuwho speaks MCP, so Claude Code, Cursor and the like can check for a free GPU before they launch a job:

```sh
claude mcp add gpuwho -- gpuwho mcp   # tools: gpu_status, free_gpus, who_uses
```

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/htyvv/gpuwho/main/install.sh | sh
```

That puts one binary in `~/.local/bin` (Linux x86_64/aarch64, macOS). Windows users can take `gpuwho-x86_64-pc-windows-msvc.zip` from [Releases](https://github.com/htyvv/gpuwho/releases). Or build it: `cargo install --git https://github.com/htyvv/gpuwho gpuwho`.

The servers need Linux and an NVIDIA driver. Nothing else: no root, no daemon, no open port. If your servers share a home directory over NFS, as many lab clusters do, install it once and run `gpuwho` from any of them.

## Usage

Register each server once. gpuwho connects right away to check that it works, then saves it.

```sh
gpuwho add gpu1 10.0.0.11                  # your SSH keys or ssh-agent
gpuwho add gpu2 alice@10.0.0.12 --password # asks for a password and stores it (mode 0600)
gpuwho add dgx dgx.example.org:2222 --key ~/.ssh/id_lab
gpuwho add lab-a100                        # a Host from ~/.ssh/config, ProxyJump included
gpuwho add here local                      # the machine you're on, no SSH
```

Anything you don't type (user, port, key, jump host) comes from `~/.ssh/config`, the way `ssh` would resolve it.

Then run `gpuwho`:

| key | |
|---|---|
| `q` | quit |
| `p` | process table (PID, user, GPU memory, SM%, CPU%, RAM, age, command) |
| `c` | compact view, one line per server |
| `j` `k` `↑` `↓` `PgUp` `PgDn` `g` `G` | scroll |
| `r` | retry failed servers now |
| `space` | pause |
| `?` | help |

```
gpuwho                       live dashboard, refreshes every 3 s
gpuwho -i 1 -p               every second, with the process table
gpuwho once                  one snapshot, then exit (also what you get under watch, cron or a pipe)
gpuwho once --json           the same as JSON (a stable schema)
gpuwho test                  check every connection, exit 1 if one fails
gpuwho list | remove gpu2
```

## How it works

- One SSH connection per server ([russh](https://github.com/Eugeny/russh)), one long-lived agent on it. Each refresh, gpuwho sends a newline and gets one line of JSON back. No new handshake, no new process per poll.
- The agent is a 600 KB Rust binary built against glibc 2.17 (CentOS 7 and up, x86_64 and aarch64). It goes once into your per-user runtime directory (`/run/user/UID`, a tmpfs that's gone when you log out) or `~/.cache/gpuwho`, and is reused while it's there. Where it can't run (noexec, other architecture), gpuwho pipes [`probe.py`](crates/gpuwho/assets/probe.py) through `python3 -c` instead: same JSON, nothing on disk, Python 3.6+.
- GPU data comes from NVML via `dlopen`, the library behind `nvidia-smi`. gpuwho never runs `nvidia-smi` and never parses its output.
- Process owners come from `/proc`, containers from `/proc/PID/cgroup` (Docker, containerd, Podman, CRI-O).
- It runs as your user and exits when the SSH session closes, after 90 s without a request, or after 6 hours.
- If the NVIDIA driver hangs, the NVML call times out after 15 s and you still get CPU and RAM for that server.
- A dead server is retried after 2, 4, 8 ... up to 60 s. A wrong password or a changed host key waits until you press `r`, so nobody's fail2ban gets triggered.

What it costs, measured on our servers:

| | |
|---|---|
| agent, per poll, host with 1,750 processes and 8 GPUs | 31 ms of CPU (1% of a core at 3 s) |
| client, live view of 6 servers | 0.3% of a core, 10 MB RAM, 3 threads |
| drawing a frame of 64 servers | 21 ms |
| `gpuwho test` on 6 servers | 2.3 s wall, 14 ms of CPU |

gpuwho 0.x was Python. The Rust rewrite cut the agent from 47 to 31 ms per poll and the client from 5% to 0.3% of a core; frames of 64 servers went from 412 to 21 ms.

## Why

Our lab has several GPU servers and one shared home directory. Finding a free GPU used to mean `ssh` into each box, run `nvidia-smi`, then `ps` to work out who owns PID 812345, and then asking in the group chat who's done. gpuwho does the first part for every server every three seconds, and gives the group chat part some structure.

## Alternatives

There are good GPU monitors already. This is how the ones I looked at differ, going by their READMEs and source in October 2026.

| | Many servers in one view | Needs on each server | Shows who owns each GPU process | Interface |
|---|---|---|---|---|
| **gpuwho** | yes | nothing (agent copied over SSH, or `python3`) | yes, Docker jobs included | terminal |
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

- You work on one machine and want to act on it: a process tree, history graphs, a Python API. Use **nvitop**.
- You want a file transfer pane next to the monitor. Use **svtop**.
- You have AMD, Intel, Apple or other accelerators. Use **nvtop** or **all-smi**. gpuwho is NVIDIA only.
- You run a large fleet and want NVLink topology or recording and replay. Use **all-smi**.
- You need alerting and long history, or Kubernetes. Use **DCGM exporter** with Prometheus and Grafana, or Netdata.
- People want a web page or a desktop status bar instead of a terminal. Use **gpustat-web**, **gpuview** or [Multiple-smi](https://github.com/ClementPinard/Multiple-smi) (or `gpuwho once --html` from cron).

If gpuwho saves you a round of `ssh`, a star helps other labs find it.

## FAQ

**Does it work with AMD or Intel GPUs?**
No, NVIDIA only (it talks to NVML). Servers without NVIDIA GPUs still show CPU, RAM and users.

**Does it read `~/.ssh/config`? Jump hosts?**
Yes. gpuwho asks `ssh -G` for the resolved settings, so `Include`, `Match`, `ProxyJump` and `ProxyCommand` work. Jump hosts are reached through your `ssh` with keys or the agent; a stored password is only sent to the final server.

**Is storing a password safe?**
It's stored in plain text in `~/.config/gpuwho/servers.json`, which gpuwho forces to mode 0600 on every start. Keys are better when you can use them. See [SECURITY.md](SECURITY.md).

**What does it leave on a server?**
The agent binary in `/run/user/UID` (gone at logout) or `~/.cache/gpuwho`, and your claim file in `/var/tmp/gpuwho-claims` if you `claim`. Add `"agent": "python"` to a server in `servers.json` (or `gpuwho add --agent python`) and nothing is written at all.

**Can I use it on Windows?**
Yes, the Windows build talks SSH directly and uses the OpenSSH agent. The servers still need to be Linux.

## Troubleshooting

**`auth failed: Authentication failed.`**
Wrong password, or your key isn't in the server's `authorized_keys`. Re-add with `gpuwho add NAME HOST --password --force`, then press `r`.

**`python3: not found` (with `rust agent: ...` after it)**
The Rust agent couldn't run there (the reason follows) and there's no `python3` to fall back on. A noexec `/run/user` is the usual cause; installing `python3` fixes it.

**`host key changed for ...`**
The server's host key changed since it was first seen. If you know why (reinstall), run `ssh-keygen -R HOST` and delete its line in `~/.config/gpuwho/known_hosts`.

**The user column shows `? (hidden)` or `? (gone)`**
`hidden`: `/proc` on that server is mounted with `hidepid`, so other users' processes are hidden from you. `gone`: the process has exited but the driver still holds its memory, usually DataLoader workers of a killed job.

**A user's RAM differs from what `ps` or `htop` add up to**
gpuwho reports the memory the kernel charged to the user's cgroup, which includes their files in `/dev/shm` and leaves out page cache. The process table still shows plain RSS per process.

## Development

```sh
git clone https://github.com/htyvv/gpuwho && cd gpuwho
cargo test --workspace            # client and agent
python -m pytest                  # the Python fallback agent
cargo run -p gpuwho -- demo
```

To embed the Rust agent in a local build, build it first and point the client at it: `cargo build --release -p gpuwho-agent && GPUWHO_AGENT_X86_64=target/release/gpuwho-agent cargo build --release -p gpuwho`. Without it the client still works and uses `python3` on every server. `python scripts/screenshot.py target/release/gpuwho docs/screenshot.png` redraws the README images from the demo data (needs Pillow).

## Acknowledgements

The NVML handling follows [gpustat](https://github.com/wookayin/gpustat) and [nvitop](https://github.com/XuehaiPan/nvitop): process API fallbacks, memory v2, lost GPUs, per-process utilization. The terminal UI is [ratatui](https://github.com/ratatui/ratatui).

## License

[MIT](LICENSE)
