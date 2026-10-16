//! RAM per user from memory cgroups.
//!
//! Summing RSS over a user's processes overcounts badly: forked DataLoader workers share
//! their parent's pages copy-on-write and /dev/shm tensors show up in every process that
//! maps them. The kernel charges each page to exactly one cgroup, so per-user RAM is read
//! from cgroups (user-UID.slice for SSH sessions, docker-*.scope for containers) and shared
//! out by RSS when one cgroup holds several users. Without a readable cgroup, RSS is used.

use std::collections::HashMap;
use std::path::Path;

/// Memory cgroup directories named in /proc/PID/cgroup text: cgroup v2 first, then v1.
/// The root cgroup is skipped: its counters cover the whole machine.
pub fn dirs(cgroup: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in cgroup.lines() {
        let mut parts = line.splitn(3, ':');
        let (Some(id), Some(ctrls), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let path = path.trim_end_matches('/');
        if path.is_empty() {
            continue;
        }
        if id == "0" && ctrls.is_empty() {
            out.push(format!("/sys/fs/cgroup{}", path));
        } else if ctrls.split(',').any(|c| c == "memory") {
            out.push(format!("/sys/fs/cgroup/memory{}", path));
        }
    }
    out
}

pub fn of_pid(pid: u32) -> Option<String> {
    let text = crate::procfs::read(&format!("/proc/{}/cgroup", pid));
    dirs(&text)
        .into_iter()
        .find(|d| Path::new(d).join("memory.stat").exists())
}

/// Anonymous + shared memory charged to a cgroup, in kB, from its memory.stat text.
/// Page cache is left out: it is reclaimable and not what people mean by "using RAM".
pub fn charged_kb(memory_stat: &str) -> Option<u64> {
    let mut anon = None;
    let mut rss = None;
    let mut shmem = 0;
    for line in memory_stat.lines() {
        let mut it = line.split_ascii_whitespace();
        let (Some(k), Some(v), None) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        let v: u64 = v.parse().unwrap_or(0);
        match k {
            "anon" => anon = Some(v), // cgroup v2
            "rss" => rss = Some(v),   // cgroup v1
            "shmem" => shmem = v,
            _ => {}
        }
    }
    anon.or(rss).map(|a| (a + shmem) / 1024)
}

/// {user: kB}. `groups` is {cgroup dir or None: {user: RSS kB}}; `charged(dir)` gives the
/// cgroup's memory in kB, or None if unreadable, in which case RSS is used as is.
pub fn split_by_rss(
    groups: &HashMap<Option<String>, HashMap<String, u64>>,
    charged: impl Fn(&str) -> Option<u64>,
) -> HashMap<String, u64> {
    let mut out: HashMap<String, u64> = HashMap::new();
    for (dir, shares) in groups {
        let used = dir.as_deref().and_then(&charged);
        let total: u64 = shares.values().sum();
        for (user, &rss) in shares {
            let kb = match used {
                Some(u) if total > 0 => (u as f64 * rss as f64 / total as f64) as u64,
                _ => rss,
            };
            *out.entry(user.clone()).or_default() += kb;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgroup_dirs() {
        assert_eq!(
            dirs("0::/user.slice/user-1000.slice/session-3.scope"),
            vec!["/sys/fs/cgroup/user.slice/user-1000.slice/session-3.scope"]
        );
        let hybrid = "12:memory:/user.slice/user-1000.slice\n1:name=systemd:/user.slice\n0::/user.slice/x.scope";
        assert_eq!(
            dirs(hybrid),
            vec![
                "/sys/fs/cgroup/memory/user.slice/user-1000.slice",
                "/sys/fs/cgroup/user.slice/x.scope"
            ]
        );
        assert_eq!(
            dirs("7:blkio,memory:/user.slice"),
            vec!["/sys/fs/cgroup/memory/user.slice"]
        );
        assert!(dirs("0::/").is_empty());
    }

    #[test]
    fn charged() {
        let v2 = "anon 1073741824\nfile 53687091200\nkernel 1000\nshmem 2147483648\n";
        assert_eq!(charged_kb(v2), Some(3 * 1024 * 1024));
        assert_eq!(
            charged_kb("cache 999999\nrss 1048576\nshmem 1048576\n"),
            Some(2048)
        );
        assert_eq!(charged_kb(""), None);
    }

    #[test]
    fn split() {
        let mut groups: HashMap<Option<String>, HashMap<String, u64>> = HashMap::new();
        groups.insert(
            Some("/u/alice".into()),
            [("alice".to_string(), 300 * 1024)].into(),
        );
        groups.insert(
            Some("/docker/x".into()),
            [("root".to_string(), 2000), ("bob".to_string(), 1000)].into(),
        );
        groups.insert(Some("/gone".into()), [("carol".to_string(), 700)].into());
        groups.insert(None, [("dave".to_string(), 500)].into());
        let charged = |d: &str| match d {
            "/u/alice" => Some(10 * 1024),
            "/docker/x" => Some(3000),
            _ => None,
        };
        let out = split_by_rss(&groups, charged);
        assert_eq!(out["alice"], 10 * 1024);
        assert_eq!(
            (out["root"], out["bob"], out["carol"], out["dave"]),
            (2000, 1000, 700, 500)
        );
    }
}
