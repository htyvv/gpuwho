//! `gpuwho record` / `gpuwho report`: lightweight usage history without a daemon or a
//! database. `record` appends one compact JSON line per sample (run it in tmux or from
//! cron); `report` turns the file into GPU-hours per person, including hours spent holding
//! a GPU without using it.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use crate::render::{busy_procs, num, proc_owner};
use crate::state::View;

pub fn sample(views: &[View], now: f64) -> Value {
    let servers: Vec<Value> = views
        .iter()
        .map(|v| {
            let gpus: Vec<Value> = v
                .data
                .as_ref()
                .and_then(|d| d["gpus"].as_array())
                .into_iter()
                .flatten()
                .filter(|g| !g["invalid"].as_bool().unwrap_or(false))
                .map(|g| {
                    let mut users: BTreeMap<String, f64> = BTreeMap::new();
                    for p in busy_procs(g) {
                        *users.entry(proc_owner(p)).or_default() += num(p, "mem").unwrap_or(0.0);
                    }
                    json!({"i": g["index"], "u": g["util"], "m": g["mem_used"],
                           "users": users.into_iter().map(|(u, m)| json!({"u": u, "m": m.round()})).collect::<Vec<_>>()})
                })
                .collect();
            json!({"name": v.name, "ok": v.ok(), "gpus": gpus})
        })
        .collect();
    json!({"t": now, "servers": servers})
}

#[derive(Default, Debug)]
pub struct UserUse {
    pub gpu_hours: f64,
    pub idle_hours: f64,
    pub servers: BTreeSet<String>,
}

#[derive(Default, Debug)]
pub struct Report {
    pub from: f64,
    pub to: f64,
    pub samples: usize,
    pub users: BTreeMap<String, UserUse>,
    /// server -> (GPU-hours available, GPU-hours in use)
    pub servers: BTreeMap<String, (f64, f64)>,
}

/// Integrate samples. Each sample stands for the time until the next one, capped at three
/// times the usual gap, so a pause in recording isn't counted as use.
pub fn report(lines: impl Iterator<Item = String>, since: Option<f64>) -> Report {
    let mut samples: Vec<Value> = lines
        .filter_map(|l| serde_json::from_str::<Value>(&l).ok())
        .filter(|s| {
            s["t"]
                .as_f64()
                .is_some_and(|t| since.is_none_or(|from| t >= from))
        })
        .collect();
    samples.sort_by(|a, b| {
        a["t"]
            .as_f64()
            .partial_cmp(&b["t"].as_f64())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut r = Report {
        samples: samples.len(),
        ..Default::default()
    };
    if samples.is_empty() {
        return r;
    }
    let ts: Vec<f64> = samples.iter().map(|s| s["t"].as_f64().unwrap()).collect();
    let mut gaps: Vec<f64> = ts
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|g| *g > 0.0)
        .collect();
    gaps.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let usual = gaps.get(gaps.len() / 2).copied().unwrap_or(60.0);
    let (from, to) = (ts[0], *ts.last().unwrap());
    r.from = from;
    r.to = to + usual;
    for (i, s) in samples.iter().enumerate() {
        let dt = ts
            .get(i + 1)
            .map_or(usual, |next| (next - ts[i]).min(3.0 * usual))
            / 3600.0;
        for srv in s["servers"].as_array().into_iter().flatten() {
            let name = srv["name"].as_str().unwrap_or("?").to_string();
            for g in srv["gpus"].as_array().into_iter().flatten() {
                let users = g["users"].as_array().cloned().unwrap_or_default();
                let e = r.servers.entry(name.clone()).or_default();
                e.0 += dt;
                if !users.is_empty() {
                    e.1 += dt;
                }
                let idle = g["u"].as_f64().is_some_and(|u| u < 5.0);
                for u in &users {
                    let who = u["u"].as_str().unwrap_or("?").to_string();
                    let e = r.users.entry(who).or_default();
                    e.gpu_hours += dt;
                    if idle && u["m"].as_f64().unwrap_or(0.0) >= 1024.0 {
                        e.idle_hours += dt;
                    }
                    e.servers.insert(name.clone());
                }
            }
        }
    }
    r
}

/// "7d" / "24h" before now, as an epoch.
pub fn since(spec: &str, now: f64) -> anyhow::Result<f64> {
    Ok(now - crate::claim::parse_duration(spec)?)
}

pub fn print(r: &Report) {
    if r.samples == 0 {
        println!("no samples in that range");
        return;
    }
    let hours = (r.to - r.from) / 3600.0;
    println!("{} samples over {:.1} h\n", r.samples, hours);
    println!(
        "  {:<16} {:>10} {:>12} {:>9}  Servers",
        "User", "GPU-hours", "idle-held h", "avg GPUs"
    );
    let mut rows: Vec<(&String, &UserUse)> = r.users.iter().collect();
    rows.sort_by(|a, b| {
        b.1.gpu_hours
            .partial_cmp(&a.1.gpu_hours)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for (u, e) in rows {
        println!(
            "  {:<16} {:>10.1} {:>12.1} {:>9.2}  {}",
            u,
            e.gpu_hours,
            e.idle_hours,
            if hours > 0.0 {
                e.gpu_hours / hours
            } else {
                0.0
            },
            e.servers.iter().cloned().collect::<Vec<_>>().join(",")
        );
    }
    println!("\n  {:<16} {:>10} {:>12}", "Server", "in use", "of GPU-h");
    for (s, (avail, used)) in &r.servers {
        println!(
            "  {:<16} {:>9.0}% {:>12.1}",
            s,
            if *avail > 0.0 {
                100.0 * used / avail
            } else {
                0.0
            },
            avail
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_hours() {
        let s = |t: f64, util: u64, users: Value| {
            json!({"t": t, "servers": [{"name": "g1", "ok": true, "gpus": [{"i": 0, "u": util, "m": 20000, "users": users}]}]})
                .to_string()
        };
        let alice = json!([{"u": "alice", "m": 20000}]);
        let lines = vec![
            s(0.0, 90, alice.clone()),
            s(3600.0, 0, alice.clone()),
            s(7200.0, 0, alice.clone()),
            s(36000.0, 90, json!([])), // recording paused for hours: capped
        ];
        let r = report(lines.into_iter(), None);
        let a = &r.users["alice"];
        // 1 h + 1 h + the 8 h pause counted as only 3 h (3x the usual gap)
        assert!((a.gpu_hours - 5.0).abs() < 1e-9, "{:?}", a);
        assert!((a.idle_hours - 4.0).abs() < 1e-9);
        assert_eq!(r.servers["g1"], (6.0, 5.0));
    }
}
