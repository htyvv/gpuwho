//! gpuwho: every GPU on every server, in one terminal, over SSH.

mod agents;
mod config;
mod demo;
mod remote;
mod render;
mod sshcfg;
mod state;
mod tui;

use std::io::IsTerminal;
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
        /// exit 1 if any server failed, not only all of them
        #[arg(long)]
        strict: bool,
        /// give up on a server after this many seconds
        #[arg(long, default_value_t = 30.0)]
        timeout: f64,
    },
    /// list free GPUs (exit 1 if fewer than -n)
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
        /// print only GPU indices, comma-separated (needs one server)
        #[arg(long)]
        ids: bool,
        #[arg(long)]
        json: bool,
        #[arg(long, default_value_t = 30.0)]
        timeout: f64,
    },
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

fn runtime() -> tokio::runtime::Runtime {
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
fn probe_all(rt: &tokio::runtime::Runtime, cfgs: &[ServerCfg], timeout: f64) -> Vec<View> {
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

fn ctx(live: bool, interval: f64, procs: bool) -> Ctx {
    Ctx {
        live,
        interval,
        procs,
        me: config::me(),
        now: Instant::now(),
    }
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

fn snapshot(views: &[View]) -> Value {
    let servers: Vec<Value> = views
        .iter()
        .map(|v| {
            let mut e = json!({"name": v.name, "host": v.host, "ok": v.ok(), "error": v.error,
                               "latency": v.latency.map(|l| (l * 1000.0).round() / 1000.0), "agent": v.agent});
            if let Some(d) = &v.data {
                for k in ["hostname", "uptime", "cpu", "mem", "users", "gpus", "gpu_error", "nvml", "driver", "cuda"] {
                    e[k] = d.get(k).cloned().unwrap_or(Value::Null);
                }
            }
            e
        })
        .collect();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    json!({"schema": SCHEMA, "version": env!("CARGO_PKG_VERSION"), "time": now, "servers": servers})
}

fn print_dashboard(views: &[View], cli: &Cli) {
    let buf = render::dashboard(
        views,
        &ctx(false, 0.0, cli.procs),
        term_width(),
        cli.compact,
    );
    print!("{}", render::to_ansi(&buf, use_color(cli.no_color)));
}

fn cmd_once(cli: &Cli, names: &[String], as_json: bool, strict: bool, timeout: f64) -> Result<i32> {
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

/// (server, gpu index, free GiB, gpu name), most free memory first.
fn free_gpus(
    views: &[View],
    min_free: Option<f64>,
    max_util: f64,
) -> Vec<(String, u64, f64, String)> {
    let mut out = Vec::new();
    for v in views {
        let Some(d) = &v.data else { continue };
        for g in d["gpus"].as_array().into_iter().flatten() {
            let (Some(used), Some(total)) =
                (render::num(g, "mem_used"), render::num(g, "mem_total"))
            else {
                continue;
            };
            if g["invalid"].as_bool().unwrap_or(false) || total <= 0.0 {
                continue;
            }
            let free = (total - used) / 1024.0;
            let ok = match min_free {
                None => render::gpu_is_idle(g),
                Some(m) => free >= m && render::num(g, "util").unwrap_or(0.0) <= max_util,
            };
            if ok {
                let name = render::short_gpu_name(render::text(g, "name").unwrap_or("?"));
                out.push((
                    v.name.clone(),
                    g["index"].as_u64().unwrap_or(0),
                    (free * 10.0).round() / 10.0,
                    name,
                ));
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

#[allow(clippy::too_many_arguments)]
fn cmd_free(
    names: &[String],
    n: usize,
    min_free: Option<f64>,
    max_util: f64,
    ids: bool,
    as_json: bool,
    timeout: f64,
) -> Result<i32> {
    let cfgs = select(&config::load()?, names)?;
    let rt = runtime();
    let views = probe_all(&rt, &cfgs, timeout);
    rt.shutdown_background();
    let found = free_gpus(&views, min_free, max_util);
    if ids {
        let mut hosts: Vec<&String> = found.iter().map(|r| &r.0).collect();
        hosts.dedup();
        if hosts.len() > 1 {
            bail!(
                "--ids needs one server, e.g. `gpuwho free {} --ids`",
                hosts[0]
            );
        }
        println!(
            "{}",
            found
                .iter()
                .take(n)
                .map(|r| r.1.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
    } else if as_json {
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
    Ok(if found.len() < n { 1 } else { 0 })
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
    use std::io::Write;
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
        "  {:<12} {:<22} {:>5}  {:<12} {:<16} Via",
        "Name", "Host", "Port", "User", "Auth"
    );
    for c in &conf.servers {
        if c.is_local() {
            println!(
                "  {:<12} {:<22} {:>5}  {:<12} {:<16}",
                c.name, c.host, "", "", "local"
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
            "  {:<12} {:<22} {:>5}  {:<12} {:<16} {}",
            c.name,
            s.hostname.clone().unwrap_or(c.host.clone()),
            c.port.or(s.port).unwrap_or(22),
            c.user.clone().or(s.user.clone()).unwrap_or_else(config::me),
            auth,
            via
        );
    }
    println!("{}", config::servers_path().display());
    Ok(0)
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
    tui::run(
        rt,
        pollers,
        tui::App::new(
            cli.interval,
            cli.procs,
            if cli.compact { Some(true) } else { None },
        ),
        config::me(),
    )?;
    Ok(0)
}

fn main() {
    let cli = Cli::parse();
    let code = match &cli.cmd {
        None if !std::io::stdout().is_terminal() => {
            // under `watch`, cron or a pipe a live screen would print nothing and never end
            eprintln!("gpuwho: not a terminal, printing one snapshot (`gpuwho once` does this explicitly)");
            cmd_once(&cli, &[], false, false, 30.0)
        }
        None => cmd_watch(&cli, false),
        Some(Cmd::Demo) if !std::io::stdout().is_terminal() => {
            let views: Vec<View> = demo::entries()
                .iter()
                .enumerate()
                .map(|(i, e)| {
                    let mut d = demo::DemoServer::new(e, i as u64);
                    let mut v = View::new(&d.name, &d.host);
                    v.apply(d.poll(), Instant::now(), 1.0);
                    v
                })
                .collect();
            print_dashboard(&views, &cli);
            Ok(0)
        }
        Some(Cmd::Demo) => cmd_watch(&cli, true),
        Some(Cmd::Once {
            names,
            json,
            strict,
            timeout,
        }) => cmd_once(&cli, names, *json, *strict, *timeout),
        Some(Cmd::Free {
            names,
            n,
            min_free,
            max_util,
            ids,
            json,
            timeout,
        }) => cmd_free(names, *n, *min_free, *max_util, *ids, *json, *timeout),
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

    #[test]
    fn free_selection() {
        let views: Vec<View> = demo::entries()
            .iter()
            .map(|e| {
                let mut v = View::new(e["name"].as_str().unwrap(), "h");
                v.data = Some(e["data"].clone());
                v
            })
            .collect();
        let idle = free_gpus(&views, None, 50.0);
        assert_eq!(idle.len(), 7);
        assert!(idle.iter().all(|r| r.0 == "node01" || r.0 == "node03"));
        assert!(idle.windows(2).all(|w| w[0].2 >= w[1].2));
        let roomy = free_gpus(&views, Some(30.0), 50.0);
        assert!(roomy.iter().any(|r| r.0 == "node01" && r.1 == 5)); // triton at 12% load, 32 GiB free
        assert!(!roomy.iter().any(|r| r.0 == "node02")); // dave's GPUs are full
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
