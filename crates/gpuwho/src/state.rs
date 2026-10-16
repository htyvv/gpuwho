//! What the UI knows about each server, updated by its poller.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

pub const MAX_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Default)]
pub struct View {
    pub name: String,
    pub host: String,
    pub data: Option<Value>,
    pub error: Option<String>,
    pub updated: Option<Instant>,
    pub latency: Option<f64>,
    pub version: u64,
    pub fails: u32,
    pub retry_at: Option<Instant>,
    /// auth / host key problem: retrying only gets the account locked, wait for the user
    pub blocked: bool,
    /// which agent answered: "rust" or "python"
    pub agent: Option<String>,
    /// (gpu, pid) -> since when that process has held >= 1 GiB without computing,
    /// as seen by this client (the servers keep no history)
    pub held_idle: HashMap<(u64, u64), Instant>,
}

pub type Shared = Arc<Mutex<View>>;

/// What one poll produced.
pub enum Outcome {
    Data(Value),
    /// the agent is fine but one sample failed: keep the connection
    AgentError(String),
    Failed {
        msg: String,
        needs_user: bool,
    },
}

impl View {
    pub fn new(name: &str, host: &str) -> Self {
        View {
            name: name.into(),
            host: host.into(),
            ..Default::default()
        }
    }

    pub fn due(&self, now: Instant) -> bool {
        !self.blocked && self.retry_at.is_none_or(|t| now >= t)
    }

    pub fn retry(&mut self) {
        self.blocked = false;
        self.retry_at = None;
    }

    pub fn ok(&self) -> bool {
        self.data.is_some() && self.error.is_none()
    }

    /// A process "holds idle" when it has >= 1 GiB on a GPU and isn't computing: its own SM%
    /// under 5 when the agent measured it, else the GPU's load under 5% with it as the only job.
    fn track_idle(&mut self, d: &Value, now: Instant) {
        let mut seen = HashMap::new();
        for g in d["gpus"].as_array().into_iter().flatten() {
            let index = g["index"].as_u64().unwrap_or(0);
            let busy = crate::render::busy_procs(g);
            let gpu_util = g["util"].as_f64();
            for p in &busy {
                let pid = p["pid"].as_u64().unwrap_or(0);
                if p["mem"].as_f64().unwrap_or(0.0) < 1024.0 {
                    continue;
                }
                let idle = match p["sm"].as_f64() {
                    Some(sm) => sm < 5.0,
                    None => busy.len() == 1 && gpu_util.is_some_and(|u| u < 5.0),
                };
                if idle {
                    let since = self.held_idle.get(&(index, pid)).copied().unwrap_or(now);
                    seen.insert((index, pid), since);
                }
            }
        }
        self.held_idle = seen;
    }

    pub fn apply(&mut self, out: Outcome, started: Instant, jitter: f64) {
        let now = Instant::now();
        match out {
            Outcome::Data(v) => {
                self.agent = v["agent"]["impl"]
                    .as_str()
                    .map(str::to_string)
                    .or(Some("python".into()));
                self.track_idle(&v, now);
                self.data = Some(v);
                self.error = None;
                self.updated = Some(now);
                self.latency = Some((now - started).as_secs_f64());
                self.fails = 0;
                self.retry_at = None;
                self.blocked = false;
            }
            Outcome::AgentError(e) => {
                self.error = Some(format!("agent: {}", e));
                self.fails = 0;
            }
            Outcome::Failed { msg, needs_user } => {
                self.error = Some(msg);
                self.fails += 1;
                if needs_user {
                    self.blocked = true;
                } else {
                    // 2, 4, 8 ... 60 s, jittered so a rack coming back isn't hit all at once
                    let base = Duration::from_secs(1u64 << self.fails.min(6)).min(MAX_BACKOFF);
                    self.retry_at = Some(now + base.mul_f64(jitter));
                }
            }
        }
        self.version += 1;
    }
}

/// 0.8 .. 1.2, without pulling in a random crate.
pub fn jitter() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let mut x = nanos as u64 ^ 0x9E37_79B9_7F4A_7C15;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    0.8 + (x % 4001) as f64 / 10000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_idle_tracking() {
        let mut v = View::new("x", "h");
        let gpu = |util: u64, mem: f64| {
            serde_json::json!({"gpus": [{"index": 0, "util": util, "mem_used": mem,
            "procs": [{"pid": 7, "mem": mem, "type": "C"}]}]})
        };
        let t0 = Instant::now();
        v.apply(Outcome::Data(gpu(0, 20000.0)), t0, 1.0);
        let since = v.held_idle[&(0, 7)];
        v.apply(Outcome::Data(gpu(0, 20000.0)), t0, 1.0);
        assert_eq!(v.held_idle[&(0, 7)], since); // still counting from the first time
        v.apply(Outcome::Data(gpu(90, 20000.0)), t0, 1.0);
        assert!(v.held_idle.is_empty()); // computing again
        v.apply(Outcome::Data(gpu(0, 500.0)), t0, 1.0);
        assert!(v.held_idle.is_empty()); // too small to matter
    }

    #[test]
    fn backoff_and_blocking() {
        let mut v = View::new("x", "h");
        let t = Instant::now();
        v.apply(
            Outcome::Failed {
                msg: "connection refused".into(),
                needs_user: false,
            },
            t,
            1.0,
        );
        assert!(!v.due(Instant::now()) && !v.blocked && v.fails == 1);
        v.retry();
        assert!(v.due(Instant::now()));
        v.apply(
            Outcome::Failed {
                msg: "auth failed".into(),
                needs_user: true,
            },
            t,
            1.0,
        );
        assert!(v.blocked && !v.due(Instant::now() + MAX_BACKOFF * 2));
        v.apply(
            Outcome::Data(serde_json::json!({"agent": {"impl": "rust"}})),
            t,
            1.0,
        );
        assert!(v.ok() && v.fails == 0 && v.agent.as_deref() == Some("rust"));
        assert!((0.8..=1.2).contains(&jitter()));
    }
}
