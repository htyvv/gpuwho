# Changelog

Notable changes to gpuwho. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [1.0.1] - 2026-10-16

### Fixed

- Upgrading left the previous agent binary in `/run/user/UID` (or `~/.cache/gpuwho`) on
  every server; after uploading a new one gpuwho now removes your older ones.
- The key hints were cut off in an 80-column live view on Windows.

## [1.0.0] - 2026-10-16

gpuwho is now written in Rust, client and server agent, and grew the features labs asked
for once they could see who uses which GPU: waiting for one, reserving one, and noticing
who holds one without using it.

### Added

- `gpuwho free --wait [--wait-timeout 2h] [--notify URL] [-- CMD]`: wait for GPUs, ring the
  bell, post to Slack/Discord/Feishu/WeCom/DingTalk/ntfy/Telegram, or run CMD with
  `CUDA_VISIBLE_DEVICES` set.
- `gpuwho claim SERVER:GPUS --for 6h --note ...`, `unclaim`, `claims`: soft reservations
  everyone sees; `free` skips GPUs claimed by others.
- Held-idle detection: a job keeping >= 1 GiB at ~0% for `--idle-after` minutes (30) is
  tagged, and the per-user table gets a "Held idle" column.
- `gpuwho kill SERVER:GPUS`: your own processes on those GPUs, after asking.
- `gpuwho record` / `gpuwho report`: GPU-hours per person from a JSONL file, no daemon.
- `gpuwho once --html`, `gpuwho mcp` (Model Context Protocol server for coding agents).
- Unaccounted GPU memory, miner-looking processes, `--max-gpus` limit, process age.
- Prebuilt binaries for Linux (x86_64, aarch64; glibc 2.17+), macOS and Windows, an
  install script, and wheels so `pipx install gpuwho` works without Rust.

### Changed

- The server agent is a 600 KB Rust binary, copied once to `/run/user/UID` (or
  `~/.cache/gpuwho`); `probe.py` over `python3 -c` remains the fallback and can be forced
  per server with `"agent": "python"`. Servers no longer need python3.
- Agent: 47 -> 31 ms of CPU per poll (host with 1,750 processes and 8 GPUs).
- Client: 5% -> 0.3% of a core for a live view of 6 servers; a frame of 64 servers
  412 -> 21 ms; startup in milliseconds.
- `servers.json` and `known_hosts` are read as before; `once --json` keeps schema 1 and
  adds `claims`.


## [0.2.0] - 2026-10-16

Benchmarked against nvitop, gpustat, nvtop, all-smi, gpustat-web, btop and psutil, and
fixed what that turned up.

### Added

- `gpuwho demo`: the live dashboard on a made-up cluster, no servers needed.
- `gpuwho free [-n N] [--min-free GIB] [--ids]` lists free GPUs and exits 1 when there aren't
  enough: `CUDA_VISIBLE_DEVICES=$(gpuwho free gpu1 -n 2 --ids) python train.py`.
- `~/.ssh/config` support through `ssh -G` (Include, Match, ProxyJump, ProxyCommand), and
  `gpuwho add NAME [user@]host[:port]` or just an ssh config alias.
- Live view keys: scroll (`j`/`k`, arrows, PgUp/PgDn, `g`/`G`), `c` compact view, `r` retry,
  space to pause, `?` help. It switches to the compact view by itself when the panels don't
  fit, and nothing is cut off silently any more.
- Per-process SM% when two or more jobs share a GPU; MIG instances; MPS clients.
- GPU processes say why their owner is missing: gone, zombie, hidden.
- `once --json` has a schema (`{"schema": 1, "servers": [...]}`) with `ok`/`error` per server.

### Changed

- `once` exits 1 when no server answered (`--strict`: when any failed); `test` exits 1 when
  any failed. Under `watch`, cron or a pipe, plain `gpuwho` prints one snapshot.
- Failing servers are retried after 2, 4, 8 ... 60 s; wrong passwords and changed host keys
  wait for `r` instead of being retried every poll.
- Servers with a stored password use it first instead of trying every key.
- A poll costs about 45 ms of CPU instead of 68 ms on a host with 1,700 processes.
- Narrow terminals drop Power, Fan and the bars before the Users column.

### Fixed

- One GPU job with non-UTF-8 bytes in its command line took the whole server offline.
- Failed SSH handshakes leaked a connection every retry.
- Readings of exactly 0 (fan 0%, 0 W) showed as missing.
- A GPU server whose NVIDIA module failed to load was shown as "CPU only"; NVML errors are
  named now and nvmlInit is retried.
- Keys typed together were handled late, Ctrl-Z and SIGTERM left the terminal broken, and
  two gpuwho processes could overwrite each other's `known_hosts` entries.

## [0.1.0] - 2026-10-16

First public release.

### Added

- Live dashboard for every registered server: per-GPU utilization, memory, temperature, fan,
  power, encoder/decoder load, and who is using each GPU.
- Docker jobs show up as `root@<container id>`. Display servers parked on a GPU (Xorg,
  gnome-shell) are not counted as users, so the GPU still shows as idle.
- Lost GPUs ("GPU is lost" from NVML) get a red row instead of breaking the whole server.
- Cross-server summary per user: GPUs held, GPU memory, CPU cores, RAM.
- RAM per user is read from the user's cgroups (anonymous + shared memory), so pages shared
  by forked workers or `/dev/shm` count once instead of once per process. Falls back to summed
  RSS where cgroup memory accounting isn't available.
- `gpuwho once` (text or `--json`), `add`, `remove`, `list`, `test`.
- The agent that is piped over SSH: stdlib only, runs on Python 3.6+, talks to NVML through
  ctypes, never calls `nvidia-smi`.
- Password or key authentication, trust-on-first-use host keys, config forced to mode 0600.

[Unreleased]: https://github.com/htyvv/gpuwho/compare/v1.0.1...HEAD
[1.0.1]: https://github.com/htyvv/gpuwho/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/htyvv/gpuwho/compare/v0.2.0...v1.0.0
[0.2.0]: https://github.com/htyvv/gpuwho/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/htyvv/gpuwho/releases/tag/v0.1.0
