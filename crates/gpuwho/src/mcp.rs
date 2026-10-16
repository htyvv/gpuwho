//! `gpuwho mcp`: a Model Context Protocol server over stdio, so coding agents (Claude Code,
//! Cursor, ...) can ask which GPUs are free and who is using what before they launch a job.
//!
//!   claude mcp add gpuwho -- gpuwho mcp

use std::io::{BufRead, Write};

use serde_json::{json, Value};

use crate::config::{self, ServerCfg};
use crate::render::{self, busy_procs, gpu_is_idle, num, proc_owner, text};
use crate::state::View;

const PROTOCOL: &str = "2025-06-18";

fn tools() -> Value {
    json!([
        {"name": "gpu_status",
         "description": "Every NVIDIA GPU on the lab's servers: load, memory, who is using it, idle or not, reservations.",
         "inputSchema": {"type": "object", "properties": {
             "servers": {"type": "array", "items": {"type": "string"}, "description": "only these servers (default: all)"}}}},
        {"name": "free_gpus",
         "description": "Idle (or roomy) GPUs, most free memory first, skipping GPUs reserved by others. Use the result for CUDA_VISIBLE_DEVICES.",
         "inputSchema": {"type": "object", "properties": {
             "n": {"type": "integer", "description": "how many are needed"},
             "min_free_gib": {"type": "number", "description": "accept busy GPUs with at least this much free memory"},
             "server": {"type": "string"}}}},
        {"name": "who_uses",
         "description": "What one user holds across all servers: GPUs, GPU memory, CPU cores and RAM.",
         "inputSchema": {"type": "object", "properties": {"user": {"type": "string"}}, "required": ["user"]}}
    ])
}

fn gpu_summary(v: &View) -> Value {
    let Some(d) = &v.data else {
        return json!({"name": v.name, "ok": false, "error": v.error});
    };
    let gpus: Vec<Value> = d["gpus"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|g| {
            let users: Vec<Value> = busy_procs(g).iter().map(|p| json!({"user": proc_owner(p), "mem_gib": num(p, "mem").map(|m| (m / 102.4).round() / 10.0), "sm": p.get("sm")})).collect();
            json!({"index": g["index"], "name": text(g, "name"), "util": g["util"],
                   "mem_used_gib": num(g, "mem_used").map(|m| (m / 102.4).round() / 10.0),
                   "mem_total_gib": num(g, "mem_total").map(|m| (m / 102.4).round() / 10.0),
                   "idle": gpu_is_idle(g), "lost": g["invalid"].as_bool().unwrap_or(false), "users": users,
                   "claims": render::claims_on(d, g["index"].as_u64().unwrap_or(0))})
        })
        .collect();
    json!({"name": v.name, "ok": v.ok(), "error": v.error, "gpus": gpus})
}

fn call(name: &str, args: &Value, cfgs: &[ServerCfg]) -> Result<Value, String> {
    let rt = crate::runtime();
    let pick: Vec<ServerCfg> = match (name, args["servers"].as_array(), args["server"].as_str()) {
        (_, Some(list), _) => cfgs
            .iter()
            .filter(|c| list.iter().any(|n| n.as_str() == Some(&c.name)))
            .cloned()
            .collect(),
        (_, _, Some(one)) => cfgs.iter().filter(|c| c.name == one).cloned().collect(),
        _ => cfgs.to_vec(),
    };
    if pick.is_empty() {
        return Err("no such server".into());
    }
    let views = crate::probe_all(&rt, &pick, 30.0);
    rt.shutdown_background();
    Ok(match name {
        "gpu_status" => json!({"servers": views.iter().map(gpu_summary).collect::<Vec<_>>()}),
        "free_gpus" => {
            let n = args["n"].as_u64().unwrap_or(1) as usize;
            let found = crate::free_gpus(
                &views,
                args["min_free_gib"].as_f64(),
                50.0,
                &crate::remote_users(&pick),
                false,
            );
            json!({"enough": found.len() >= n, "gpus": found.iter().map(|r| json!({"server": r.0, "gpu": r.1, "free_gib": r.2, "name": r.3})).collect::<Vec<_>>()})
        }
        "who_uses" => {
            let user = args["user"].as_str().ok_or("user is required")?;
            let all = render::collect_users(&views);
            let mine: Vec<Value> = all
                .iter()
                .filter(|(k, _)| k.as_str() == user || k.starts_with(&format!("{}@", user)))
                .map(|(k, e)| json!({"user": k, "gpus": e.gpus, "gpu_mem_gib": (e.gpu_mem / 102.4).round() / 10.0, "where": e.hosts, "cpu_cores": (e.cpu / 10.0).round() / 10.0, "ram_gib": (e.ram_kb / 1048576.0).round()}))
                .collect();
            json!({"user": user, "found": mine})
        }
        _ => return Err(format!("unknown tool {}", name)),
    })
}

fn reply(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error(id: &Value, code: i64, msg: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg}})
}

pub fn handle(msg: &Value, cfgs: &[ServerCfg]) -> Option<Value> {
    let id = msg.get("id")?.clone(); // notifications get no answer
    let method = msg["method"].as_str().unwrap_or("");
    Some(match method {
        "initialize" => reply(
            &id,
            json!({"protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or(PROTOCOL),
            "capabilities": {"tools": {}}, "serverInfo": {"name": "gpuwho", "version": env!("CARGO_PKG_VERSION")}}),
        ),
        "ping" => reply(&id, json!({})),
        "tools/list" => reply(&id, json!({"tools": tools()})),
        "tools/call" => {
            let name = msg["params"]["name"].as_str().unwrap_or("");
            match call(name, &msg["params"]["arguments"], cfgs) {
                Ok(v) => reply(
                    &id,
                    json!({"content": [{"type": "text", "text": v.to_string()}], "structuredContent": v, "isError": false}),
                ),
                Err(e) => reply(
                    &id,
                    json!({"content": [{"type": "text", "text": e}], "isError": true}),
                ),
            }
        }
        _ => error(&id, -32601, &format!("method not found: {}", method)),
    })
}

pub fn serve() -> anyhow::Result<i32> {
    let cfgs = config::load()?.servers;
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let answer = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => handle(&msg, &cfgs),
            Err(e) => Some(error(&Value::Null, -32700, &format!("parse error: {}", e))),
        };
        if let Some(a) = answer {
            writeln!(out, "{}", a)?;
            out.flush()?;
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol() {
        let init = handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}), &[]).unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "gpuwho");
        assert!(handle(
            &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            &[]
        )
        .is_none());
        let list = handle(
            &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
            &[],
        )
        .unwrap();
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 3);
        let bad = handle(&json!({"jsonrpc": "2.0", "id": 3, "method": "nope"}), &[]).unwrap();
        assert_eq!(bad["error"]["code"], -32601);
        let call = handle(&json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "gpu_status", "arguments": {}}}), &[]).unwrap();
        assert_eq!(call["result"]["isError"], true); // no servers configured
    }
}
