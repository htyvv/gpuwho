//! Reading /proc: process stats, owners, command lines, containers.
//!
//! Everything is read as bytes and decoded leniently: a command line with CP949 bytes or a
//! `comm` cut in the middle of a UTF-8 character must not break a poll.

use std::collections::HashMap;
use std::ffi::CStr;
use std::fs;
use std::os::unix::fs::MetadataExt;

/// One line of /proc/PID/stat, the fields we use.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stat {
    pub state: u8,
    pub ticks: u64, // utime + stime
    pub rss_pages: u64,
    pub start: u64, // starttime, to notice pid reuse
}

pub fn read(path: &str) -> String {
    fs::read(path)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

/// Parse /proc/PID/stat. `comm` may contain spaces and ')', so split after the last ')'.
pub fn parse_stat(raw: &[u8]) -> Option<Stat> {
    let close = raw.iter().rposition(|&c| c == b')')?;
    let rest = std::str::from_utf8(&raw[close + 1..]).ok()?;
    let f: Vec<&str> = rest.split_ascii_whitespace().collect();
    if f.len() < 22 {
        return None;
    }
    Some(Stat {
        state: f[0].bytes().next()?,
        ticks: f[11].parse::<u64>().ok()? + f[12].parse::<u64>().ok()?,
        rss_pages: f[21].parse().ok()?,
        start: f[19].parse().ok()?,
    })
}

pub fn all_pids() -> Vec<u32> {
    let mut out = Vec::with_capacity(2048);
    if let Ok(dir) = fs::read_dir("/proc") {
        for e in dir.flatten() {
            if let Some(pid) = e.file_name().to_str().and_then(|s| s.parse().ok()) {
                out.push(pid);
            }
        }
    }
    out
}

/// Stats for the given pids (or every process). Exited and hidden processes are skipped.
pub fn stats(pids: &[u32]) -> HashMap<u32, Stat> {
    let mut out = HashMap::with_capacity(pids.len());
    let mut buf = Vec::with_capacity(1024);
    for &pid in pids {
        buf.clear();
        let path = format!("/proc/{}/stat", pid);
        if let Ok(mut f) = fs::File::open(&path) {
            use std::io::Read;
            if f.read_to_end(&mut buf).is_ok() {
                if let Some(s) = parse_stat(&buf) {
                    out.insert(pid, s);
                }
            }
        }
    }
    out
}

/// pid -> user name, cached per uid. None if the process is gone or hidden (hidepid).
pub struct Owners {
    names: HashMap<u32, String>,
}

impl Owners {
    pub fn new() -> Self {
        Owners {
            names: HashMap::new(),
        }
    }

    pub fn of(&mut self, pid: u32) -> Option<String> {
        let uid = fs::metadata(format!("/proc/{}", pid)).ok()?.uid();
        Some(self.name(uid))
    }

    pub fn name(&mut self, uid: u32) -> String {
        self.names
            .entry(uid)
            .or_insert_with(|| user_name(uid))
            .clone()
    }
}

fn user_name(uid: u32) -> String {
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let ret = unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result) };
    if ret == 0 && !result.is_null() && !pwd.pw_name.is_null() {
        unsafe { CStr::from_ptr(pwd.pw_name) }
            .to_string_lossy()
            .into_owned()
    } else {
        uid.to_string()
    }
}

pub fn cmdline(pid: u32) -> String {
    let raw = fs::read(format!("/proc/{}/cmdline", pid)).unwrap_or_default();
    let s = String::from_utf8_lossy(&raw).replace('\0', " ");
    let s = s.trim();
    s.chars().take(300).collect()
}

/// Short container id from /proc/PID/cgroup text (docker, containerd, podman, cri-o).
pub fn container_id(cgroup: &str) -> Option<String> {
    for marker in [
        "docker-",
        "docker/",
        "containerd-",
        "containerd/",
        "libpod-",
        "crio-",
    ] {
        let mut from = 0;
        while let Some(i) = cgroup[from..].find(marker) {
            let start = from + i + marker.len();
            let id: String = cgroup[start..]
                .chars()
                .take_while(|c| c.is_ascii_hexdigit())
                .collect();
            if id.len() >= 12 && id.chars().all(|c| !c.is_ascii_uppercase()) {
                return Some(id[..12].to_string());
            }
            from = start;
        }
    }
    None
}

/// Why a GPU process may show no owner or command: gone, zombie, D state, or hidden.
pub fn state_note(pid: u32, stats: &HashMap<u32, Stat>, has_owner: bool) -> Option<&'static str> {
    match stats.get(&pid) {
        None => Some("gone"), // NVML still lists it: typically workers holding a dead parent's context
        Some(s) if s.state == b'Z' => Some("zombie"),
        Some(s) if s.state == b'D' => Some("D"),
        Some(_) if !has_owner => Some("hidden"),
        Some(_) => None,
    }
}

pub fn cpu_times() -> (u64, u64) {
    let text = read("/proc/stat");
    let vals: Vec<u64> = text
        .lines()
        .next()
        .unwrap_or("")
        .split_ascii_whitespace()
        .skip(1)
        .take(8) // guest and guest_nice are already inside user and nice
        .filter_map(|v| v.parse().ok())
        .collect();
    let idle = vals.get(3).copied().unwrap_or(0) + vals.get(4).copied().unwrap_or(0);
    (vals.iter().sum(), idle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_with_spaces_and_parens_in_comm() {
        let raw =
            b"4242 (we ird) name) S 1 2 3 4 5 6 7 8 9 10 111 222 13 14 15 16 17 18 999 20 3000 22";
        let s = parse_stat(raw).unwrap();
        assert_eq!(
            (s.state, s.ticks, s.start, s.rss_pages),
            (b'S', 333, 999, 3000)
        );
        assert!(parse_stat(b"garbage").is_none());
    }

    #[test]
    fn containers() {
        let cases = [
            (
                "12:pids:/docker/3f9c2a1b4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8",
                Some("3f9c2a1b4d5e"),
            ),
            (
                "0::/system.slice/docker-3f9c2a1b4d5e6f708192a3b4c5d6e7f8091a2b.scope",
                Some("3f9c2a1b4d5e"),
            ),
            (
                "0::/kubepods/pod1/cri-containerd-0123456789abcdef0123456789abcdef.scope",
                Some("0123456789ab"),
            ),
            (
                "0::/machine.slice/libpod-abcdef012345abcdef012345abcdef012345.scope",
                Some("abcdef012345"),
            ),
            (
                "0::/kubepods.slice/crio-fedcba987654fedcba987654fedcba987654.scope",
                Some("fedcba987654"),
            ),
            ("0::/user.slice/user-1000.slice/session-3.scope", None),
            ("", None),
        ];
        for (text, want) in cases {
            assert_eq!(container_id(text).as_deref(), want, "{}", text);
        }
    }

    #[test]
    fn own_process() {
        let me = std::process::id();
        let s = stats(&[me, 999_999_999]);
        assert_eq!(s.len(), 1);
        assert!(s[&me].rss_pages > 0 && s[&me].start > 0);
        assert!(Owners::new().of(me).is_some());
        assert_eq!(state_note(me, &s, true), None);
        assert_eq!(state_note(1, &HashMap::new(), true), Some("gone"));
    }
}
