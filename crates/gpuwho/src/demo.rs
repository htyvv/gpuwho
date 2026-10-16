//! `gpuwho demo`: the live dashboard on a made-up cluster, to try gpuwho without servers.
//!
//! The cluster lives in assets/demo.json (also the test fixture and the README images).
//! Each poll nudges the numbers, and a few scripted events play out: a job finishes,
//! another one starts, so the screen looks like a real lab.

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::state::Outcome;

pub const DATA: &str = include_str!("../assets/demo.json");

pub fn entries() -> Vec<Value> {
    let v: Value = serde_json::from_str(DATA).expect("demo.json");
    v["servers"].as_array().cloned().unwrap_or_default()
}

/// (poll number, server, gpu index, event)
enum Event {
    Finish,
    Start(&'static str, f64, &'static str),
}

const EVENTS: [(u64, &str, usize, Event); 4] = [
    (4, "node01", 2, Event::Finish),
    (
        7,
        "node03",
        3,
        Event::Start(
            "frank",
            61200.0,
            "python train_lora.py --model qwen2.5-7b --bf16",
        ),
    ),
    (
        11,
        "node01",
        2,
        Event::Start(
            "alice",
            40900.0,
            "python train.py --config configs/vit_l16.yaml --batch-size 256",
        ),
    ),
    (14, "node03", 3, Event::Finish),
];

pub struct DemoServer {
    pub name: String,
    pub host: String,
    data: Value,
    base: HashMap<u64, i64>,
    polls: u64,
    rng: u64,
}

fn clamp(x: f64, lo: f64, hi: f64) -> f64 {
    x.max(lo).min(hi)
}

impl DemoServer {
    pub fn new(entry: &Value, seed: u64) -> Self {
        let data = entry["data"].clone();
        let base = data["gpus"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|g| {
                (
                    g["index"].as_u64().unwrap_or(0),
                    g["util"].as_i64().unwrap_or(0),
                )
            })
            .collect();
        DemoServer {
            name: entry["name"].as_str().unwrap_or("?").into(),
            host: entry["host"].as_str().unwrap_or("?").into(),
            data,
            base,
            polls: 0,
            rng: 0x2545_F491_4F6C_DD1D ^ (seed + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15),
        }
    }

    fn rand(&mut self) -> f64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng % 1_000_000) as f64 / 1_000_000.0
    }

    fn uniform(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.rand()
    }

    fn event(&mut self, gpu: usize, ev: &Event) {
        let pid = 100_000 + (self.rand() * 899_999.0) as u64;
        let g = &mut self.data["gpus"][gpu];
        let display: Vec<Value> = g["procs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|p| p["type"] == "G")
            .cloned()
            .collect();
        match ev {
            Event::Finish => {
                g["procs"] = Value::Array(display);
                g["mem_used"] = json!(4.0);
                g["util"] = json!(0);
            }
            Event::Start(user, mem, cmd) => {
                let mut procs = display;
                procs.push(json!({"pid": pid, "mem": mem, "type": "C", "user": user, "cpu": 100.0, "container": null,
                                  "cmd": cmd, "rss_kb": 6u64 << 20, "start": null}));
                g["procs"] = Value::Array(procs);
                g["mem_used"] = json!(mem + 200.0);
                g["util"] = json!(90);
                let index = g["index"].as_u64().unwrap_or(0);
                self.base.insert(index, 92);
                let users = self.data["users"].as_array_mut().unwrap();
                if !users.iter().any(|u| u["user"] == *user) {
                    users.push(json!({"user": user, "cpu": 105.0, "rss_kb": 9u64 << 20, "mem_kb": 7u64 << 20, "nproc": 6}));
                }
            }
        }
    }

    pub fn poll(&mut self) -> Outcome {
        self.polls += 1;
        for (n, server, gpu, ev) in EVENTS.iter() {
            if *n == self.polls % 16 && *server == self.name {
                self.event(*gpu, ev);
            }
        }
        let n = self.data["gpus"].as_array().map_or(0, |a| a.len());
        for i in 0..n {
            if self.data["gpus"][i]["invalid"].as_bool().unwrap_or(false) {
                continue;
            }
            let (r1, r2, r3) = (
                self.rand(),
                self.uniform(-1.0, 1.0),
                self.uniform(0.85, 1.0),
            );
            let g = &mut self.data["gpus"][i];
            let busy = g["procs"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|p| p["type"] != "G");
            let base = *self
                .base
                .get(&g["index"].as_u64().unwrap_or(0))
                .unwrap_or(&0) as f64;
            let util = if busy {
                let step = (r1 * 13.0).floor() - 6.0;
                clamp(
                    g["util"].as_f64().unwrap_or(0.0) + step,
                    (base - 15.0).max(0.0),
                    (base + 10.0).min(100.0),
                )
            } else {
                0.0
            };
            g["util"] = json!(util as i64);
            let temp = clamp(
                (0.8 * g["temp"].as_f64().unwrap_or(40.0) + 0.2 * (32.0 + 0.5 * util) + r2).round(),
                28.0,
                87.0,
            );
            g["temp"] = json!(temp as i64);
            if !g["fan"].is_null() {
                g["fan"] = json!(clamp(30.0 + (temp - 45.0) * 2.0, 30.0, 100.0) as i64);
            }
            if let (Some(_), Some(limit)) = (g["power"].as_f64(), g["power_limit"].as_f64()) {
                let idle = 0.08 * limit;
                g["power"] =
                    json!(((idle + (limit - idle) * util / 100.0 * r3) * 10.0).round() / 10.0);
            }
        }
        let (u, l) = (
            self.uniform(-4.0, 4.0),
            [
                self.uniform(-0.6, 0.6),
                self.uniform(-0.6, 0.6),
                self.uniform(-0.6, 0.6),
            ],
        );
        let count = self.data["cpu"]["count"].as_f64().unwrap_or(1.0);
        let cpu = &mut self.data["cpu"];
        cpu["usage"] = json!(
            (clamp(cpu["usage"].as_f64().unwrap_or(0.0) + u, 1.0, 99.0) * 10.0).round() / 10.0
        );
        if let Some(load) = cpu["load"].as_array_mut() {
            for (x, d) in load.iter_mut().zip(l) {
                *x =
                    json!((clamp(x.as_f64().unwrap_or(0.0) + d, 0.0, count) * 10.0).round() / 10.0);
            }
        }
        self.data["time"] = json!(std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64());
        let mut v = self.data.clone();
        v["agent"] = json!({"impl": "demo"});
        Outcome::Data(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cluster_moves() {
        let mut servers: Vec<DemoServer> = entries()
            .iter()
            .enumerate()
            .map(|(i, e)| DemoServer::new(e, i as u64))
            .collect();
        let users = |s: &DemoServer, g: usize| -> Vec<String> {
            s.data["gpus"][g]["procs"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|p| p["type"] == "C")
                .map(|p| p["user"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(users(&servers[0], 2), vec!["bob"]);
        for _ in 0..7 {
            for s in servers.iter_mut() {
                s.poll();
            }
        }
        assert!(users(&servers[0], 2).is_empty()); // bob's job finished at poll 4
        assert_eq!(users(&servers[2], 3), vec!["frank"]);
        for g in servers[0].data["gpus"].as_array().unwrap() {
            let (u, t) = (g["util"].as_i64().unwrap(), g["temp"].as_i64().unwrap());
            assert!((0..=100).contains(&u) && (28..=87).contains(&t));
        }
    }
}
