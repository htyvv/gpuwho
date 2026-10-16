//! gpuwho: every GPU on every server, in one terminal, over SSH.

mod agents;
mod claim;
mod config;
mod demo;
mod history;
mod mcp;
mod notify;
mod remote;
mod render;
mod sshcfg;
mod state;
mod tui;

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};

use crate::config::ServerCfg;
use crate::remote::Source;
use crate::render::Ctx;
use crate::state::{jitter, Shared, View};

const SCHEMA: u32 = 1; // `once --json` layout; bumped only for breaking changes

fn interval(s: &str) -> Result<f64, String> {
    let v: f64 = s.parse().map_err(|_| format!("not a number: {}", s))?;
    if v < 0.5 {
        return Err("must be at least 0.5 seconds".into());
    }
    Ok(v)
}

#[derive(Parser)]
#[command(
    name = "gpuwho",
    version,
    about = "Every GPU on every server, in one terminal, over SSH."
)]
struct Cli {
    /// refresh interval in seconds
    #[arg(short, long, global = true, default_value = "3", value_parser = interval)]
    interval: f64,
    /// show the per-process GPU table
    #[arg(short, long, global = true)]
    procs: bool,
    /// one line per server
    #[arg(short, long, global = true)]
    compact: bool,
    /// no colors (NO_COLOR works too)
    #[arg(long, global = true)]
    no_color: bool,
    /// flag GPU memory held without computing for this many minutes (live view)
    #[arg(long, global = true, default_value_t = 30, value_name = "MIN")]
    idle_after: u64,
    /// lab rule: highlight people using more GPUs than this (or set max_gpus_per_user in servers.json)
    #[arg(long, global = true, value_name = "N")]
    max_gpus: Option<usize>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// print one snapshot
    Once {
        /// only these servers
        names: Vec<String>,
        /// machine-readable output (schema 1)
        #[arg(long)]
        json: bool,
        /// a standalone HTML page (e.g. from cron into a web directory)
        #[arg(long, conflicts_with = "json")]
        html: bool,
        /// with --html: reload the page every N seconds
        #[arg(long, value_name = "SECS")]
        refresh: Option<u64>,
        /// exit 1 if any server failed, not only all of them
        #[arg(long)]
        strict: bool,
        /// give up on a server after this many seconds
        #[arg(long, default_value_t = 30.0)]
        timeout: f64,
    },
    /// list free GPUs; --wait until there are enough, then notify or run a command
    Free {
        names: Vec<String>,
        /// how many you need
        #[arg(short, default_value_t = 1)]
        n: usize,
        /// instead of idle GPUs, any GPU with this much free memory (GiB)
        #[arg(long, value_name = "GIB")]
        min_free: Option<f64>,
        /// with --min-free: skip GPUs busier than this (%)
        #[arg(long, value_name = "PCT", default_value_t = 50.0)]
        max_util: f64,
        /// include GPUs other people have claimed
        #[arg(long)]
        any: bool,
        /// print only GPU indices, comma-separated (needs one server)
        #[arg(long)]
        ids: bool,
        #[arg(long)]
        json: bool,
        /// keep checking until enough GPUs are free
        #[arg(long)]
        wait: bool,
        /// with --wait: give up after this long (e.g. 2h; default: never)
        #[arg(long, value_name = "DURATION")]
        wait_timeout: Option<String>,
        /// post to this webhook when they're free (Slack, Discord, Feishu, WeCom, DingTalk, ntfy, ...)
        #[arg(long, value_name = "URL")]
        notify: Option<String>,
        #[arg(long, default_value_t = 30.0)]
        timeout: f64,
        /// run this here with CUDA_VISIBLE_DEVICES set to the free GPUs (after --)
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// reserve GPUs softly: everyone sees it, `free` skips them, nothing is enforced
    Claim {
        /// SERVER:GPUS, e.g. gpu1:0,1 or gpu1:0-3
        target: String,
        /// how long (e.g. 6h, 90m, 2d)
        #[arg(long = "for", default_value = "4h", value_name = "DURATION")]
        duration: String,
        #[arg(long)]
        note: Option<String>,
    },
    /// drop your claims: SERVER (all of them) or SERVER:GPUS
    Unclaim { target: String },
    /// list everyone's claims
    Claims { names: Vec<String> },
    /// stop your own processes on a GPU: SERVER:GPUS
    Kill {
        target: String,
        /// signal to send
        #[arg(short, long, default_value = "TERM")]
        signal: String,
        /// don't ask
        #[arg(long)]
        yes: bool,
    },
    /// append a usage sample to FILE every --every seconds (run in tmux or from cron)
    Record {
        file: String,
        #[arg(long, default_value_t = 60.0)]
        every: f64,
        /// take one sample and exit (for cron)
        #[arg(long)]
        once: bool,
    },
    /// GPU-hours per person from a `record` file
    Report {
        file: String,
        /// only the last DURATION (e.g. 7d)
        #[arg(long)]
        since: Option<String>,
    },
    /// Model Context Protocol server on stdio, for coding agents
    Mcp,
    /// the dashboard on a made-up cluster, no servers needed
    Demo,
    /// register a server
    Add {
        name: String,
        /// [user@]host[:port], an ~/.ssh/config alias, or 'local' (default: NAME)
        host: Option<String>,
        /// login user (default: from ~/.ssh/config, else you)
        #[arg(short, long)]
        user: Option<String>,
        #[arg(short = 'P', long)]
        port: Option<u16>,
        /// prompt for a password and store it (mode 0600)
        #[arg(long)]
        password: bool,
        /// private key file
        #[arg(long)]
        key: Option<String>,
        /// agent to run there: auto (Rust, else python3), rust or python
        #[arg(long)]
        agent: Option<String>,
        /// overwrite existing / save even if the test fails
        #[arg(long)]
        force: bool,
        /// skip the connection test
        #[arg(long)]
        no_test: bool,
    },
    /// unregister a server
    Remove { name: String },
    /// list registered servers
    List,
    /// check every connection (exit 1 if any fails)
    Test {
        names: Vec<String>,
        #[arg(long, default_value_t = 30.0)]
        timeout: f64,
    },
}

pub(crate) fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime")
}

fn select(conf: &config::Conf, names: &[String]) -> Result<Vec<ServerCfg>> {
    let unknown: Vec<&String> = names
        .iter()
        .filter(|n| !conf.servers.iter().any(|s| &s.name == *n))
        .collect();
    if !unknown.is_empty() {
        bail!(
            "unknown server: {} (`gpuwho list` shows what's registered)",
            unknown
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let picked: Vec<ServerCfg> = conf
        .servers
        .iter()
        .filter(|s| names.is_empty() || names.contains(&s.name))
        .cloned()
        .collect();
    if picked.is_empty() {
        bail!(
            "no servers configured. add one with:  gpuwho add NAME [USER@]HOST[:PORT] [--password]"
        );
    }
    Ok(picked)
}

fn shared(name: &str, host: &str) -> Shared {
    Arc::new(Mutex::new(View::new(name, host)))
}

/// Poll every server once, at the same time; give up on slow ones after `timeout`.
pub(crate) fn probe_all(
    rt: &tokio::runtime::Runtime,
    cfgs: &[ServerCfg],
    timeout: f64,
) -> Vec<View> {
    let views: Vec<Shared> = cfgs.iter().map(|c| shared(&c.name, &c.host)).collect();
    rt.block_on(async {
        let tasks: Vec<_> = cfgs
            .iter()
            .cloned()
            .zip(views.iter().cloned())
            .map(|(cfg, view)| {
                tokio::spawn(async move {
                    let mut src = Source::for_cfg(&cfg, 90);
                    let t0 = Instant::now();
                    let out = src.poll().await;
                    view.lock().unwrap().apply(out, t0, jitter());
                })
            })
            .collect();
        let _ = tokio::time::timeout(
            Duration::from_secs_f64(timeout),
            futures::future::join_all(tasks),
        )
        .await;
    });
    views
        .iter()
        .map(|v| {
            let mut v = v.lock().unwrap().clone();
            if v.version == 0 {
                v.error = Some(format!("no answer within {:.0}s", timeout));
            }
            v
        })
        .collect()
}

/// The login name on each server (claims and processes are owned by it).
pub(crate) fn remote_users(cfgs: &[ServerCfg]) -> HashMap<String, String> {
    cfgs.iter()
        .map(|c| {
            let user = if c.is_local() {
                config::me()
            } else {
                c.user
                    .clone()
                    .or_else(|| sshcfg::settings(&c.host).user)
                    .unwrap_or_else(config::me)
            };
            (c.name.clone(), user)
        })
        .collect()
}

fn max_gpus(cli: &Cli) -> Option<usize> {
    cli.max_gpus.or_else(|| {
        config::load()
            .ok()
            .and_then(|c| c.extra.get("max_gpus_per_user").and_then(Value::as_u64))
            .map(|m| m as usize)
    })
}

fn ctx(cli: &Cli, live: bool) -> Ctx {
    let mut c = Ctx::new(
        live,
        if live { cli.interval } else { 0.0 },
        cli.procs,
        config::me(),
    );
    c.idle_after = Duration::from_secs(cli.idle_after * 60);
    c.max_gpus = max_gpus(cli);
    c
}

fn term_width() -> u16 {
    if let Ok((w, _)) = ratatui::crossterm::terminal::size() {
        if std::io::stdout().is_terminal() {
            return w;
        }
    }
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse().ok())
        .unwrap_or(120)
}

fn use_color(no_color: bool) -> bool {
    !no_color && std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

fn snapshot(views: &[View]) -> Value {
    let servers: Vec<Value> = views
        .iter()
        .map(|v| {
            let mut e = json!({"name": v.name, "host": v.host, "ok": v.ok(), "error": v.error,
                               "latency": v.latency.map(|l| (l * 1000.0).round() / 1000.0), "agent": v.agent});
            if let Some(d) = &v.data {
                for k in ["hostname", "uptime", "cpu", "mem", "users", "gpus", "gpu_error", "nvml", "driver", "cuda", "claims"] {
                    e[k] = d.get(k).cloned().unwrap_or(Value::Null);
                }
            }
            e
        })
        .collect();
    json!({"schema": SCHEMA, "version": env!("CARGO_PKG_VERSION"), "time": now_epoch(), "servers": servers})
}

fn print_dashboard(views: &[View], cli: &Cli) {
    let buf = render::dashboard(views, &ctx(cli, false), term_width(), cli.compact);
    print!("{}", render::to_ansi(&buf, use_color(cli.no_color)));
}

fn cmd_once(
    cli: &Cli,
    names: &[String],
    as_json: bool,
    html: bool,
    refresh: Option<u64>,
    strict: bool,
    timeout: f64,
) -> Result<i32> {
    let cfgs = select(&config::load()?, names)?;
    let rt = runtime();
    let views = probe_all(&rt, &cfgs, timeout);
    if as_json {
        let doc = snapshot(&views);
        println!(
            "{}",
            if std::io::stdout().is_terminal() {
                serde_json::to_string_pretty(&doc)?
            } else {
                doc.to_string()
            }
        );
    } else if html {
        let width = std::env::var("COLUMNS")
            .ok()
            .and_then(|c| c.parse().ok())
            .unwrap_or(240);
        print!(
            "{}",
            render::to_html(
                &render::dashboard(&views, &ctx(cli, false), width, cli.compact),
                refresh
            )
        );
    } else {
        print_dashboard(&views, cli);
    }
    rt.shutdown_background();
    let ok = views.iter().filter(|v| v.ok()).count();
    Ok(if ok == 0 || (strict && ok < views.len()) {
        1
    } else {
        0
    })
}

/// (server, gpu index, free GiB, gpu name), most free memory first. GPUs someone else has
/// claimed are skipped unless `any`.
pub(crate) fn free_gpus(
    views: &[View],
    min_free: Option<f64>,
    max_util: f64,
    me: &HashMap<String, String>,
    any: bool,
) -> Vec<(String, u64, f64, String)> {
    let mut out = Vec::new();
    for v in views {
        let Some(d) = &v.data else { continue };
        let my_name = me.get(&v.name).cloned().unwrap_or_else(config::me);
        for g in d["gpus"].as_array().into_iter().flatten() {
            let (Some(used), Some(total)) =
                (render::num(g, "mem_used"), render::num(g, "mem_total"))
            else {
                continue;
            };
            if g["invalid"].as_bool().unwrap_or(false) || total <= 0.0 {
                continue;
            }
            let index = g["index"].as_u64().unwrap_or(0);
            if !any
                && render::claims_on(d, index)
                    .iter()
                    .any(|c| render::text(c, "user") != Some(my_name.as_str()))
            {
                continue;
            }
            let free = (total - used) / 1024.0;
            let ok = match min_free {
                None => render::gpu_is_idle(g),
                Some(m) => free >= m && render::num(g, "util").unwrap_or(0.0) <= max_util,
            };
            if ok {
                let name = render::short_gpu_name(render::text(g, "name").unwrap_or("?"));
                out.push((v.name.clone(), index, (free * 10.0).round() / 10.0, name));
            }
        }
    }
    out.sort_by(|a, b| {
        b.2.partial_cmp(&a.2)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
            .then(a.1.cmp(&b.1))
    });
    out
}

struct FreeOpts {
    n: usize,
    min_free: Option<f64>,
    max_util: f64,
    any: bool,
    ids: bool,
    json: bool,
    wait: bool,
    wait_timeout: Option<f64>,
    notify: Option<String>,
    timeout: f64,
    command: Vec<String>,
}

fn cmd_free(cli: &Cli, names: &[String], o: FreeOpts) -> Result<i32> {
    let cfgs = select(&config::load()?, names)?;
    if (o.ids || !o.command.is_empty()) && cfgs.len() > 1 {
        bail!(
            "--ids and running a command need one server, e.g. `gpuwho free {} -n {}`",
            cfgs[0].name,
            o.n
        );
    }
    let me = remote_users(&cfgs);
    let rt = runtime();
    let started = Instant::now();
    let every = Duration::from_secs_f64(cli.interval.max(10.0));
    let (views, found) = loop {
        let views = probe_all(&rt, &cfgs, o.timeout);
        let found = free_gpus(&views, o.min_free, o.max_util, &me, o.any);
        if found.len() >= o.n || !o.wait {
            break (views, found);
        }
        if o.wait_timeout
            .is_some_and(|t| started.elapsed().as_secs_f64() >= t)
        {
            eprintln!(
                "\ngave up after {:.0} s: {} of {} GPUs free",
                started.elapsed().as_secs_f64(),
                found.len(),
                o.n
            );
            break (views, found);
        }
        eprint!(
            "\rwaiting for {} free GPU{} ({} now) · {:.0} min so far   ",
            o.n,
            if o.n == 1 { "" } else { "s" },
            found.len(),
            started.elapsed().as_secs_f64() / 60.0
        );
        let _ = std::io::stderr().flush();
        std::thread::sleep(every);
    };
    rt.shutdown_background();
    let enough = found.len() >= o.n;
    if o.wait && started.elapsed() > every {
        eprintln!();
    }
    if enough && o.wait {
        eprint!("\x07"); // terminal bell
        if let Some(url) = &o.notify {
            let list: Vec<String> = found
                .iter()
                .take(o.n)
                .map(|r| format!("{} GPU {} ({:.0} GiB free)", r.0, r.1, r.2))
                .collect();
            if let Err(e) = notify::send(url, &format!("gpuwho: free now: {}", list.join(", "))) {
                eprintln!("notify failed: {}", e);
            }
        }
    }
    if !o.command.is_empty() {
        if !enough {
            return Ok(1);
        }
        let ids: Vec<String> = found.iter().take(o.n).map(|r| r.1.to_string()).collect();
        let server = &cfgs[0];
        let there = views[0]
            .data
            .as_ref()
            .and_then(|d| render::text(d, "hostname"))
            .map(str::to_string);
        let here = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
        if !server.is_local() && there.as_deref() != Some(here.trim()) {
            eprintln!(
                "note: running here, but the GPUs are on {} ({})",
                server.name,
                there.unwrap_or_default()
            );
        }
        eprintln!(
            "CUDA_VISIBLE_DEVICES={} {}",
            ids.join(","),
            o.command.join(" ")
        );
        let mut cmd = std::process::Command::new(&o.command[0]);
        cmd.args(&o.command[1..])
            .env("CUDA_VISIBLE_DEVICES", ids.join(","))
            .env("CUDA_DEVICE_ORDER", "PCI_BUS_ID");
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let e = cmd.exec();
            bail!("can't run {}: {}", o.command[0], e);
        }
        #[cfg(not(unix))]
        return Ok(cmd.status()?.code().unwrap_or(1));
    }
    if o.ids {
        println!(
            "{}",
            found
                .iter()
                .take(o.n)
                .map(|r| r.1.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
    } else if o.json {
        let rows: Vec<Value> = found
            .iter()
            .map(|r| json!({"server": r.0, "gpu": r.1, "free_gib": r.2, "name": r.3}))
            .collect();
        println!("{}", Value::Array(rows));
    } else {
        for (name, idx, free, gpu) in &found {
            println!("{:<12} {:>2}  {:>6.1}G free  {}", name, idx, free, gpu);
        }
        for v in views.iter().filter(|v| v.error.is_some()) {
            eprintln!("{:<12}  ?  {}", v.name, v.error.as_deref().unwrap_or(""));
        }
    }
    Ok(if enough { 0 } else { 1 })
}

fn find_server(name: &str) -> Result<ServerCfg> {
    let conf = config::load()?;
    Ok(select(&conf, &[name.to_string()])?.remove(0))
}

fn cmd_claim(target: &str, duration: &str, note: Option<&str>) -> Result<i32> {
    let (server, gpus) = claim::parse_target(target)?;
    let Some(gpus) = gpus else {
        bail!("say which GPUs, e.g. {}:0,1", server)
    };
    let cfg = find_server(&server)?;
    let secs = claim::parse_duration(duration)?;
    let rt = runtime();
    let res: Result<i32> = rt.block_on(async {
        let (_, out) = remote::exec(&cfg, &claim::read_script(), None)
            .await
            .map_err(|e| anyhow::anyhow!(e.msg))?;
        let now = now_epoch();
        let claims = claim::add(
            claim::own_claims(&out, now),
            &gpus,
            now + secs,
            note.unwrap_or(""),
            now,
        );
        let (code, out) = remote::exec(
            &cfg,
            &claim::write_script(),
            Some(claim::document(&claims).into_bytes()),
        )
        .await
        .map_err(|e| anyhow::anyhow!(e.msg))?;
        if code != Some(0) || !out.trim().ends_with("ok") {
            bail!("claim failed: {}", out.trim());
        }
        println!(
            "claimed {}:{} until {}{}",
            server,
            gpus.iter()
                .map(|g| g.to_string())
                .collect::<Vec<_>>()
                .join(","),
            render::local_time(now + secs),
            note.map(|n| format!(" · {}", n)).unwrap_or_default()
        );
        Ok(0)
    });
    rt.shutdown_background();
    res
}

fn cmd_unclaim(target: &str) -> Result<i32> {
    let (server, gpus) = claim::parse_target(target)?;
    let cfg = find_server(&server)?;
    let rt = runtime();
    let res: Result<i32> = rt.block_on(async {
        let (_, out) = remote::exec(&cfg, &claim::read_script(), None)
            .await
            .map_err(|e| anyhow::anyhow!(e.msg))?;
        let before = claim::own_claims(&out, now_epoch());
        let after = claim::remove(before.clone(), gpus.as_deref());
        let (code, out) = remote::exec(
            &cfg,
            &claim::write_script(),
            Some(claim::document(&after).into_bytes()),
        )
        .await
        .map_err(|e| anyhow::anyhow!(e.msg))?;
        if code != Some(0) {
            bail!("unclaim failed: {}", out.trim());
        }
        println!(
            "{} → {} claim{} left on {}",
            before.len(),
            after.len(),
            if after.len() == 1 { "" } else { "s" },
            server
        );
        Ok(0)
    });
    rt.shutdown_background();
    res
}

fn cmd_claims(names: &[String]) -> Result<i32> {
    let cfgs = select(&config::load()?, names)?;
    let rt = runtime();
    let views = probe_all(&rt, &cfgs, 30.0);
    rt.shutdown_background();
    let mut any = false;
    for v in &views {
        let Some(d) = &v.data else { continue };
        for c in d["claims"].as_array().into_iter().flatten() {
            any = true;
            let gpus: Vec<String> = c["gpus"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|g| g.to_string())
                .collect();
            println!(
                "{:<12} {:<8} {:<12} until {}  {}",
                v.name,
                gpus.join(","),
                render::text(c, "user").unwrap_or("?"),
                render::local_time(render::num(c, "until").unwrap_or(0.0)),
                render::text(c, "note").unwrap_or("")
            );
        }
    }
    if !any {
        println!("no claims");
    }
    Ok(0)
}

fn cmd_kill(target: &str, signal: &str, yes: bool) -> Result<i32> {
    let (server, gpus) = claim::parse_target(target)?;
    let cfg = find_server(&server)?;
    if signal.is_empty() || !signal.chars().all(|c| c.is_ascii_alphanumeric()) {
        bail!("bad signal name {}", signal);
    }
    let me = remote_users(std::slice::from_ref(&cfg))[&cfg.name].clone();
    let rt = runtime();
    let view = probe_all(&rt, std::slice::from_ref(&cfg), 30.0).remove(0);
    let Some(d) = &view.data else {
        bail!("{}: {}", server, view.error.unwrap_or_default())
    };
    let mut mine = Vec::new();
    let mut others = 0;
    for g in d["gpus"].as_array().into_iter().flatten() {
        let index = g["index"].as_u64().unwrap_or(0);
        if gpus.as_ref().is_some_and(|l| !l.contains(&index)) {
            continue;
        }
        for p in render::busy_procs(g) {
            if render::text(p, "user") == Some(me.as_str())
                && render::text(p, "container").is_none()
            {
                mine.push((
                    index,
                    p["pid"].as_u64().unwrap_or(0),
                    render::text(p, "cmd").unwrap_or("?").to_string(),
                    render::num(p, "mem"),
                ));
            } else {
                others += 1;
            }
        }
    }
    if mine.is_empty() {
        println!(
            "no processes of yours there ({} of other people's left alone)",
            others
        );
        rt.shutdown_background();
        return Ok(1);
    }
    for (gpu, pid, cmd, mem) in &mine {
        println!(
            "  GPU {}  pid {:>8}  {:>7}  {}",
            gpu,
            pid,
            render::gib(*mem),
            cmd.chars().take(90).collect::<String>()
        );
    }
    if !yes {
        eprint!(
            "send SIG{} to these {} process{} on {}? [y/N] ",
            signal,
            mine.len(),
            if mine.len() == 1 { "" } else { "es" },
            server
        );
        let _ = std::io::stderr().flush();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            rt.shutdown_background();
            return Ok(1);
        }
    }
    let pids: Vec<String> = mine.iter().map(|m| m.1.to_string()).collect();
    let script = format!("kill -s {} {}", signal, pids.join(" "));
    let res = rt.block_on(remote::exec(&cfg, &script, None));
    rt.shutdown_background();
    match res {
        Ok((Some(0), _)) => {
            println!("sent SIG{} to {}", signal, pids.join(" "));
            Ok(0)
        }
        Ok((_, out)) => bail!("kill: {}", out.trim()),
        Err(e) => bail!("{}", e.msg),
    }
}

fn cmd_record(file: &str, every: f64, once: bool) -> Result<i32> {
    let cfgs = select(&config::load()?, &[])?;
    let rt = runtime();
    loop {
        let t0 = Instant::now();
        let views = probe_all(&rt, &cfgs, 30.0);
        let line = history::sample(&views, now_epoch()).to_string();
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(file)?;
        writeln!(f, "{}", line)?;
        if once {
            break;
        }
        std::thread::sleep(Duration::from_secs_f64(every.max(5.0)).saturating_sub(t0.elapsed()));
    }
    rt.shutdown_background();
    Ok(0)
}

fn cmd_report(file: &str, since: Option<&str>) -> Result<i32> {
    let text = std::fs::read_to_string(file)?;
    let from = since.map(|s| history::since(s, now_epoch())).transpose()?;
    history::print(&history::report(text.lines().map(str::to_string), from));
    Ok(0)
}

fn cmd_test(names: &[String], timeout: f64) -> Result<i32> {
    let cfgs = select(&config::load()?, names)?;
    let rt = runtime();
    let views = probe_all(&rt, &cfgs, timeout);
    rt.shutdown_background();
    let color = use_color(false);
    let (ok, bad) = if color {
        ("\x1b[32m✓\x1b[0m", "\x1b[31m✗\x1b[0m")
    } else {
        ("✓", "✗")
    };
    for v in &views {
        match (&v.error, &v.data) {
            (None, Some(d)) => println!(
                "{} {:<12} {} · {} GPUs · {:.1}s · {} agent",
                ok,
                v.name,
                render::text(d, "hostname").unwrap_or("?"),
                d["gpus"].as_array().map_or(0, |g| g.len()),
                v.latency.unwrap_or(0.0),
                v.agent.as_deref().unwrap_or("?")
            ),
            (e, _) => println!("{} {:<12} {}", bad, v.name, e.as_deref().unwrap_or("?")),
        }
    }
    Ok(if views.iter().any(|v| v.error.is_some()) {
        1
    } else {
        0
    })
}

#[allow(clippy::too_many_arguments)]
fn cmd_add(
    name: &str,
    host: Option<&str>,
    user: Option<String>,
    port: Option<u16>,
    password: bool,
    key: Option<String>,
    agent: Option<String>,
    force: bool,
    no_test: bool,
) -> Result<i32> {
    let mut conf = config::load()?;
    if conf.servers.iter().any(|s| s.name == name) && !force {
        bail!(
            "server '{}' already exists (use --force to overwrite)",
            name
        );
    }
    let (t_user, t_host, t_port) = sshcfg::parse_target(host.unwrap_or(name));
    // store only what was typed, so ~/.ssh/config keeps working for the rest
    let mut entry = ServerCfg {
        name: name.into(),
        host: t_host,
        user: user.or(t_user),
        port: port.or(t_port),
        key,
        agent,
        ..Default::default()
    };
    if let Some(a) = &entry.agent {
        if !["auto", "rust", "python"].contains(&a.as_str()) {
            bail!("--agent must be auto, rust or python");
        }
    }
    if password {
        let who = entry
            .user
            .clone()
            .or_else(|| sshcfg::settings(&entry.host).user)
            .unwrap_or_else(config::me);
        let pw = read_password(&format!("password for {}@{}: ", who, entry.host))?;
        if !pw.is_empty() {
            entry.password = Some(pw);
        }
    }
    if !no_test {
        let rt = runtime();
        let v = probe_all(&rt, std::slice::from_ref(&entry), 60.0).remove(0);
        rt.shutdown_background();
        match (&v.error, &v.data) {
            (None, Some(d)) => println!(
                "ok {} · {} GPUs · {} CPU cores · {} agent",
                render::text(d, "hostname").unwrap_or("?"),
                d["gpus"].as_array().map_or(0, |g| g.len()),
                d["cpu"]["count"],
                v.agent.as_deref().unwrap_or("?")
            ),
            (e, _) => {
                println!("connection test failed: {}", e.as_deref().unwrap_or("?"));
                if !force {
                    bail!("not saved (use --force to save anyway, or --no-test)");
                }
            }
        }
    }
    conf.servers.retain(|s| s.name != name);
    conf.servers.push(entry);
    config::save(&conf)?;
    println!(
        "saved '{}' to {} (mode 600)",
        name,
        config::servers_path().display()
    );
    Ok(0)
}

fn read_password(prompt: &str) -> Result<String> {
    eprint!("{}", prompt);
    std::io::stderr().flush()?;
    let mut s = String::new();
    #[cfg(unix)]
    {
        let fd = 0;
        let mut term: libc::termios = unsafe { std::mem::zeroed() };
        let tty = unsafe { libc::tcgetattr(fd, &mut term) } == 0;
        if tty {
            let mut silent = term;
            silent.c_lflag &= !libc::ECHO;
            unsafe { libc::tcsetattr(fd, libc::TCSANOW, &silent) };
        }
        let r = std::io::stdin().read_line(&mut s);
        if tty {
            unsafe { libc::tcsetattr(fd, libc::TCSANOW, &term) };
            eprintln!();
        }
        r?;
    }
    #[cfg(not(unix))]
    std::io::stdin().read_line(&mut s)?;
    Ok(s.trim_end_matches(['\n', '\r']).to_string())
}

fn cmd_remove(name: &str) -> Result<i32> {
    let mut conf = config::load()?;
    let before = conf.servers.len();
    conf.servers.retain(|s| s.name != name);
    if conf.servers.len() == before {
        bail!("no server named '{}'", name);
    }
    config::save(&conf)?;
    println!("removed '{}'", name);
    Ok(0)
}

fn cmd_list() -> Result<i32> {
    let conf = config::load()?;
    println!(
        "  {:<12} {:<22} {:>5}  {:<12} {:<16} {:<7} Via",
        "Name", "Host", "Port", "User", "Auth", "Agent"
    );
    for c in &conf.servers {
        let agent = c.agent.clone().unwrap_or_else(|| "auto".into());
        if c.is_local() {
            println!(
                "  {:<12} {:<22} {:>5}  {:<12} {:<16} {:<7}",
                c.name, c.host, "", "", "local", agent
            );
            continue;
        }
        let s = sshcfg::settings(&c.host);
        let auth = if c.password.is_some() {
            "password ••••".to_string()
        } else if let Some(k) = &c.key {
            format!("key {}", k)
        } else {
            "ssh key/agent".into()
        };
        let via = s
            .proxy_jump
            .clone()
            .or(s.proxy_command.as_ref().map(|_| "ProxyCommand".into()))
            .unwrap_or_default();
        println!(
            "  {:<12} {:<22} {:>5}  {:<12} {:<16} {:<7} {}",
            c.name,
            s.hostname.clone().unwrap_or(c.host.clone()),
            c.port.or(s.port).unwrap_or(22),
            c.user.clone().or(s.user.clone()).unwrap_or_else(config::me),
            auth,
            agent,
            via
        );
    }
    println!("{}", config::servers_path().display());
    Ok(0)
}

fn demo_views() -> Vec<View> {
    demo::entries()
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let mut d = demo::DemoServer::new(e, i as u64);
            let mut v = View::new(&d.name, &d.host);
            v.apply(d.poll(), Instant::now(), 1.0);
            v
        })
        .collect()
}

fn cmd_watch(cli: &Cli, demo: bool) -> Result<i32> {
    let rt = runtime();
    let interval = Duration::from_secs_f64(cli.interval);
    let sources: Vec<(Source, Shared)> = if demo {
        demo::entries()
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let d = demo::DemoServer::new(e, i as u64);
                let view = shared(&d.name, &d.host);
                (Source::Demo(Box::new(d)), view)
            })
            .collect()
    } else {
        let idle = (3.0 * cli.interval).max(90.0) as u64; // agents exit if they hear nothing this long
        select(&config::load()?, &[])?
            .into_iter()
            .map(|c| (Source::for_cfg(&c, idle), shared(&c.name, &c.host)))
            .collect()
    };
    let pollers = tui::spawn(&rt, sources, interval);
    let mut app = tui::App::new(
        cli.interval,
        cli.procs,
        if cli.compact { Some(true) } else { None },
    );
    app.idle_after = Duration::from_secs(cli.idle_after * 60);
    app.max_gpus = max_gpus(cli);
    tui::run(rt, pollers, app, config::me())?;
    Ok(0)
}

fn main() {
    let cli = Cli::parse();
    let tty = std::io::stdout().is_terminal();
    let code = match &cli.cmd {
        None if !tty => {
            // under `watch`, cron or a pipe a live screen would print nothing and never end
            eprintln!("gpuwho: not a terminal, printing one snapshot (`gpuwho once` does this explicitly)");
            cmd_once(&cli, &[], false, false, None, false, 30.0)
        }
        None => cmd_watch(&cli, false),
        Some(Cmd::Demo) if !tty => {
            print_dashboard(&demo_views(), &cli);
            Ok(0)
        }
        Some(Cmd::Demo) => cmd_watch(&cli, true),
        Some(Cmd::Once {
            names,
            json,
            html,
            refresh,
            strict,
            timeout,
        }) => cmd_once(&cli, names, *json, *html, *refresh, *strict, *timeout),
        Some(Cmd::Free {
            names,
            n,
            min_free,
            max_util,
            any,
            ids,
            json,
            wait,
            wait_timeout,
            notify,
            timeout,
            command,
        }) => {
            match wait_timeout
                .as_deref()
                .map(claim::parse_duration)
                .transpose()
            {
                Ok(wt) => cmd_free(
                    &cli,
                    names,
                    FreeOpts {
                        n: *n,
                        min_free: *min_free,
                        max_util: *max_util,
                        any: *any,
                        ids: *ids,
                        json: *json,
                        wait: *wait || !command.is_empty(),
                        wait_timeout: wt,
                        notify: notify.clone(),
                        timeout: *timeout,
                        command: command.clone(),
                    },
                ),
                Err(e) => Err(e),
            }
        }
        Some(Cmd::Claim {
            target,
            duration,
            note,
        }) => cmd_claim(target, duration, note.as_deref()),
        Some(Cmd::Unclaim { target }) => cmd_unclaim(target),
        Some(Cmd::Claims { names }) => cmd_claims(names),
        Some(Cmd::Kill {
            target,
            signal,
            yes,
        }) => cmd_kill(target, signal, *yes),
        Some(Cmd::Record { file, every, once }) => cmd_record(file, *every, *once),
        Some(Cmd::Report { file, since }) => cmd_report(file, since.as_deref()),
        Some(Cmd::Mcp) => mcp::serve(),
        Some(Cmd::Add {
            name,
            host,
            user,
            port,
            password,
            key,
            agent,
            force,
            no_test,
        }) => cmd_add(
            name,
            host.as_deref(),
            user.clone(),
            *port,
            *password,
            key.clone(),
            agent.clone(),
            *force,
            *no_test,
        ),
        Some(Cmd::Remove { name }) => cmd_remove(name),
        Some(Cmd::List) => cmd_list(),
        Some(Cmd::Test { names, timeout }) => cmd_test(names, *timeout),
    };
    match code {
        Ok(c) => std::process::exit(c),
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo_data() -> Vec<View> {
        demo::entries()
            .iter()
            .map(|e| {
                let mut v = View::new(e["name"].as_str().unwrap(), "h");
                v.data = Some(e["data"].clone());
                v
            })
            .collect()
    }

    #[test]
    fn free_selection() {
        let views = demo_data();
        let me = HashMap::new();
        let idle = free_gpus(&views, None, 50.0, &me, false);
        assert_eq!(idle.len(), 7);
        assert!(idle.iter().all(|r| r.0 == "node01" || r.0 == "node03"));
        assert!(idle.windows(2).all(|w| w[0].2 >= w[1].2));
        let roomy = free_gpus(&views, Some(30.0), 50.0, &me, false);
        assert!(roomy.iter().any(|r| r.0 == "node01" && r.1 == 5)); // triton at 12% load, 32 GiB free
        assert!(!roomy.iter().any(|r| r.0 == "node02")); // dave's GPUs are full
    }

    #[test]
    fn claimed_gpus_are_skipped_unless_mine() {
        let mut views = demo_data();
        let d = views[0].data.as_mut().unwrap();
        d["claims"] = json!([{"user": "bob", "gpus": [3], "until": 9e12, "note": "eval"}]);
        let alice: HashMap<String, String> = [("node01".to_string(), "alice".to_string())].into();
        assert!(!free_gpus(&views, None, 50.0, &alice, false)
            .iter()
            .any(|r| r.0 == "node01" && r.1 == 3));
        assert!(free_gpus(&views, None, 50.0, &alice, true)
            .iter()
            .any(|r| r.0 == "node01" && r.1 == 3));
        let bob: HashMap<String, String> = [("node01".to_string(), "bob".to_string())].into();
        assert!(free_gpus(&views, None, 50.0, &bob, false)
            .iter()
            .any(|r| r.0 == "node01" && r.1 == 3));
    }

    #[test]
    fn json_schema() {
        let mut v = View::new("a", "h");
        v.apply(
            state::Outcome::Data(json!({"hostname": "a", "gpus": [], "agent": {"impl": "rust"}})),
            Instant::now(),
            1.0,
        );
        let doc = snapshot(&[v, View::new("b", "h2")]);
        assert_eq!(doc["schema"], 1);
        assert_eq!(doc["servers"][0]["ok"], true);
        assert_eq!(doc["servers"][0]["agent"], "rust");
        assert_eq!(doc["servers"][1]["ok"], false);
    }

    #[test]
    fn intervals() {
        assert!(interval("0.4").is_err() && interval("x").is_err());
        assert_eq!(interval("1.5"), Ok(1.5));
    }
}
