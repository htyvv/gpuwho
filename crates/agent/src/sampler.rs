//! One poll: CPU and per-user usage from /proc and cgroups, GPUs from NVML.
//!
//! All of /proc is scanned every FULL_SCAN_EVERY polls; in between only the processes that
//! mattered last time are re-read (reading every /proc/PID/stat is ~45 ms of kernel time on
//! a host with 1,700 processes). Processes that weren't watched are measured against the
//! last full scan, so a busy newcomer is still noticed.

use std::collections::{HashMap, HashSet};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::nvml::Worker;
use crate::procfs::{self, Owners, Stat};
use crate::{claims, memcg};

pub const FULL_SCAN_EVERY: u64 = 5;
const FIRST_SAMPLE: std::time::Duration = std::time::Duration::from_millis(500);

fn sysconf(name: libc::c_int, default: i64) -> i64 {
    let v = unsafe { libc::sysconf(name) };
    if v > 0 {
        v
    } else {
        default
    }
}

/// Worth listing in the per-user summary: busy, or holding at least 512 MiB.
fn significant(pct: f64, rss_kb: u64) -> bool {
    pct >= 1.0 || rss_kb >= 512 * 1024
}

pub struct Sampler {
    prev: Option<(Instant, u64, u64, HashMap<u32, Stat>)>,
    last_full: Option<(Instant, HashMap<u32, Stat>)>,
    polls: u64,
    watch: Vec<u32>,
    owners: Owners,
    nvml: Worker,
    model: String,
    clk_tck: f64,
    page_kb: u64,
    ncpu: i64,
}

impl Sampler {
    pub fn new() -> Self {
        let cpuinfo = procfs::read("/proc/cpuinfo");
        let model = cpuinfo
            .lines()
            .find(|l| l.starts_with("model name"))
            .and_then(|l| l.split_once(':'))
            .map(|(_, v)| v.trim().to_string())
            .unwrap_or_default();
        Sampler {
            prev: None,
            last_full: None,
            polls: 0,
            watch: Vec::new(),
            owners: Owners::new(),
            nvml: Worker::new(),
            model,
            clk_tck: sysconf(libc::_SC_CLK_TCK, 100) as f64,
            page_kb: (sysconf(libc::_SC_PAGESIZE, 4096) / 1024) as u64,
            ncpu: sysconf(libc::_SC_NPROCESSORS_ONLN, 1),
        }
    }

    pub fn sample(&mut self) -> Value {
        if self.prev.is_none() {
            let (t, i) = procfs::cpu_times();
            let stats = procfs::stats(&procfs::all_pids());
            let now = Instant::now();
            self.last_full = Some((now, stats.clone()));
            self.prev = Some((now, t, i, stats));
            std::thread::sleep(FIRST_SAMPLE);
        }
        let full = self.polls.is_multiple_of(FULL_SCAN_EVERY);
        self.polls += 1;
        let now = Instant::now();
        let (t1, i1) = procfs::cpu_times();
        let mut p1 = procfs::stats(&if full {
            procfs::all_pids()
        } else {
            self.watch.clone()
        });
        let (t_prev, t0, i0, p0) = self.prev.take().unwrap();
        let (full_at, full_stats) = self.last_full.take().unwrap();

        let cpu_usage =
            100.0 * (1.0 - (i1.saturating_sub(i0)) as f64 / (t1.saturating_sub(t0)).max(1) as f64);
        let wall_ticks = (now - t_prev).as_secs_f64().max(0.1) * self.clk_tck;
        let full_ticks = (now - full_at).as_secs_f64().max(0.1) * self.clk_tck;

        // per-process CPU% where 100% == one full core; a reused pid starts over
        let mut per_pid_cpu: HashMap<u32, f64> = HashMap::with_capacity(p1.len());
        let mut users: HashMap<String, Map<String, Value>> = HashMap::new();
        let mut groups: HashMap<Option<String>, HashMap<String, u64>> = HashMap::new();
        let mut keep = Vec::new();
        for (&pid, s) in &p1 {
            let (prev, span) = match p0.get(&pid) {
                Some(p) if p.start == s.start => (Some(p), wall_ticks),
                _ => (
                    full_stats.get(&pid).filter(|p| p.start == s.start),
                    full_ticks,
                ),
            };
            let pct = prev.map_or(0.0, |p| {
                (100.0 * s.ticks.saturating_sub(p.ticks) as f64 / span).max(0.0)
            });
            per_pid_cpu.insert(pid, (pct * 10.0).round() / 10.0);
            let rss_kb = s.rss_pages * self.page_kb;
            if !significant(pct, rss_kb) {
                continue; // ignore idle, small processes in the user summary
            }
            let Some(user) = self.owners.of(pid) else {
                continue;
            };
            keep.push(pid);
            let agg = users.entry(user.clone()).or_insert_with(|| {
                let mut m = Map::new();
                m.insert("user".into(), json!(user));
                m.insert("cpu".into(), json!(0.0));
                m.insert("rss_kb".into(), json!(0));
                m.insert("nproc".into(), json!(0));
                m
            });
            agg["cpu"] = json!(agg["cpu"].as_f64().unwrap_or(0.0) + pct);
            agg["rss_kb"] = json!(agg["rss_kb"].as_u64().unwrap_or(0) + rss_kb);
            agg["nproc"] = json!(agg["nproc"].as_u64().unwrap_or(0) + 1);
            *groups
                .entry(memcg::of_pid(pid))
                .or_default()
                .entry(user)
                .or_default() += rss_kb;
        }
        let charged = memcg::split_by_rss(&groups, |d| {
            memcg::charged_kb(&procfs::read(&format!("{}/memory.stat", d)))
        });
        for (user, kb) in charged {
            if let Some(u) = users.get_mut(&user) {
                u.insert("mem_kb".into(), json!(kb));
            }
        }

        let reading = self.nvml.query();
        let mut gpus = reading.gpus;
        let gpu_pids: Vec<u32> = gpus
            .iter()
            .flat_map(|g| g["procs"].as_array().cloned().unwrap_or_default())
            .filter_map(|p| p["pid"].as_u64().map(|x| x as u32))
            .collect();
        let missing: Vec<u32> = gpu_pids
            .iter()
            .copied()
            .filter(|p| !p1.contains_key(p))
            .collect();
        if !missing.is_empty() {
            p1.extend(procfs::stats(&missing)); // GPU jobs that started since the last full scan
        }
        for g in gpus.iter_mut() {
            if let Some(procs) = g["procs"].as_array_mut() {
                for p in procs.iter_mut() {
                    let pid = p["pid"].as_u64().unwrap_or(0) as u32;
                    let user = self.owners.of(pid);
                    let cgroup = procfs::read(&format!("/proc/{}/cgroup", pid));
                    p["user"] = json!(user);
                    p["cpu"] = json!(per_pid_cpu.get(&pid));
                    p["container"] = json!(procfs::container_id(&cgroup));
                    p["cmd"] = json!(procfs::cmdline(pid));
                    p["rss_kb"] = json!(p1.get(&pid).map(|s| s.rss_pages * self.page_kb));
                    p["state"] = json!(procfs::state_note(pid, &p1, user.is_some()));
                    p["start"] = json!(p1.get(&pid).map(|s| s.start));
                }
            }
        }

        let mut watch: HashSet<u32> = if full {
            keep.into_iter().collect()
        } else {
            self.watch.iter().copied().collect()
        };
        watch.extend(gpu_pids);
        self.watch = watch.into_iter().collect();
        self.last_full = Some(if full {
            (now, p1.clone())
        } else {
            (full_at, full_stats)
        });
        self.prev = Some((now, t1, i1, p1));

        let meminfo = procfs::read("/proc/meminfo");
        let mem = |key: &str| -> u64 {
            meminfo
                .lines()
                .find(|l| l.starts_with(key) && l.as_bytes().get(key.len()) == Some(&b':'))
                .and_then(|l| l.split_ascii_whitespace().nth(1))
                .and_then(|v| v.parse().ok())
                .unwrap_or(0)
        };
        let load: Vec<Value> = procfs::read("/proc/loadavg")
            .split_ascii_whitespace()
            .take(3)
            .map(|x| json!(x.parse::<f64>().ok()))
            .collect();
        let uptime = procfs::read("/proc/uptime")
            .split_ascii_whitespace()
            .next()
            .and_then(|x| x.parse::<f64>().ok());
        let now_wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let claims = claims::read(claims::DIR, now_wall, &mut |uid| self.owners.name(uid));
        let mut users: Vec<Value> = users.into_values().map(Value::Object).collect();
        users.sort_by(|a, b| {
            b["cpu"]
                .as_f64()
                .partial_cmp(&a["cpu"].as_f64())
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        json!({
            "hostname": procfs::read("/proc/sys/kernel/hostname").trim(),
            "time": now_wall,
            "uptime": uptime,
            "cpu": {"count": self.ncpu, "usage": (cpu_usage * 10.0).round() / 10.0, "load": load, "model": self.model},
            "mem": {"total_kb": mem("MemTotal"), "avail_kb": mem("MemAvailable")},
            "users": users,
            "gpus": gpus,
            "gpu_error": reading.error,
            "nvml": reading.status,
            "driver": reading.driver,
            "cuda": reading.cuda,
            "clk_tck": self.clk_tck,
            "claims": claims,
            "agent": {"pid": std::process::id(), "full_scan": full, "impl": "rust", "version": env!("CARGO_PKG_VERSION")},
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_scan_every_fifth_poll() {
        let mut s = Sampler::new();
        let flags: Vec<bool> = (0..=FULL_SCAN_EVERY)
            .map(|_| s.sample()["agent"]["full_scan"].as_bool().unwrap())
            .collect();
        assert_eq!(
            flags,
            [
                vec![true],
                vec![false; FULL_SCAN_EVERY as usize - 1],
                vec![true]
            ]
            .concat()
        );
    }

    #[test]
    fn sample_shape() {
        let v = Sampler::new().sample();
        assert!(v["cpu"]["count"].as_i64().unwrap() > 0);
        assert!(v["mem"]["total_kb"].as_u64().unwrap() > 0);
        assert!(matches!(
            v["nvml"].as_str().unwrap(),
            "ok" | "absent" | "error"
        ));
        assert!(v["gpus"].is_array() && v["users"].is_array());
    }
}
