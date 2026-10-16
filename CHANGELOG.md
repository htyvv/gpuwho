# Changelog

Notable changes to gpuwho. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.0] - 2026-10-16

First public release.

### Added

- Live dashboard for every registered server: per-GPU utilization, memory, temperature, fan,
  power, encoder/decoder load, and who is using each GPU.
- Docker jobs show up as `root@<container id>`. Display servers parked on a GPU (Xorg,
  gnome-shell) are not counted as users, so the GPU still shows as idle.
- Lost GPUs ("GPU is lost" from NVML) get a red row instead of breaking the whole server.
- Cross-server summary per user: GPUs held, GPU memory, CPU cores, RAM.
- `gpuwho once` (text or `--json`), `add`, `remove`, `list`, `test`.
- The agent that is piped over SSH: stdlib only, runs on Python 3.6+, talks to NVML through
  ctypes, never calls `nvidia-smi`.
- Password or key authentication, trust-on-first-use host keys, config forced to mode 0600.

[Unreleased]: https://github.com/htyvv/gpuwho/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/htyvv/gpuwho/releases/tag/v0.1.0
