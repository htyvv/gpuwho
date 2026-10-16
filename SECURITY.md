# Security

gpuwho logs in to your servers, and `gpuwho add --password` stores a password, so please report
anything that looks like a hole.

## Reporting

Use GitHub's private reporting (**Security → Report a vulnerability** on this repo) instead of a
public issue.

## What gpuwho keeps, and where

- `~/.config/gpuwho/servers.json`: hosts, user names and, if you used `--password`, the password
  in plain text. Every time gpuwho starts it forces this file to mode 0600 and the directory to
  0700, and it refuses to read them if another user owns them. Use SSH keys when you can.
- `~/.config/gpuwho/known_hosts`: host keys accepted on first connection. A key that changes
  later is rejected, like `StrictHostKeyChecking=accept-new`.
- On the servers: the agent binary in `/run/user/UID` (a per-user tmpfs, gone at logout) or
  `~/.cache/gpuwho`, created with mode 0700 and named by its SHA-256, and your claim file in
  `/var/tmp/gpuwho-claims` if you use `gpuwho claim`. With `"agent": "python"` for a server
  nothing is written: `probe.py` runs from memory via `python3 -c`. Either agent runs as your
  user, reads `/proc`, cgroups and NVML, and exits when the SSH session closes, after 90 s
  without a request, or after 6 hours.
- Claims are attributed by file owner, not file name, and written under fresh random names,
  so another user can neither claim as you nor block your claims.
- `gpuwho kill` only signals processes owned by your login on that server, and asks first.
