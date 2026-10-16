//! What the UI knows about each server, updated by its poller.

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

    pub fn apply(&mut self, out: Outcome, started: Instant, jitter: f64) {
        let now = Instant::now();
        match out {
            Outcome::Data(v) => {
                self.agent = v["agent"]["impl"]
                    .as_str()
                    .map(str::to_string)
                    .or(Some("python".into()));
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
