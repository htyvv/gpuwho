//! Talking to one server: an SSH connection (russh), one long-lived agent on it, one JSON
//! line per request. Also the `local` case (agent as a child process) and error wording.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client::{self, Handle};
use russh::keys::{self, PrivateKeyWithHashAlg};
use russh::ChannelMsg;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Semaphore;

use crate::agents;
use crate::config::{self, ServerCfg};
use crate::sshcfg;
use crate::state::Outcome;

pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(40); // the agent gives up on NVML after 15 s
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const START_TIMEOUT: Duration = Duration::from_secs(20);

/// Handshakes in flight at once.
static CONNECTING: Semaphore = Semaphore::const_new(8);
/// New host keys are appended under this lock, so concurrent connects don't interleave lines.
static KNOWN_HOSTS: Mutex<()> = Mutex::new(());

#[derive(Debug)]
pub struct Failure {
    pub msg: String,
    pub needs_user: bool,
}

impl Failure {
    fn new(msg: impl Into<String>) -> Self {
        Failure {
            msg: msg.into(),
            needs_user: false,
        }
    }
    fn user(msg: impl Into<String>) -> Self {
        Failure {
            msg: msg.into(),
            needs_user: true,
        }
    }
}

fn io_failure(e: &std::io::Error) -> Failure {
    use std::io::ErrorKind::*;
    match e.kind() {
        ConnectionRefused => Failure::new("connection refused"),
        TimedOut => Failure::new("unreachable (no answer in time)"),
        HostUnreachable | NetworkUnreachable => Failure::new(format!("unreachable: {}", e)),
        _ => Failure::new(e.to_string()),
    }
}

// ---------------------------------------------------------------- host keys

struct Client {
    host: String,
    port: u16,
    changed: Arc<Mutex<Option<String>>>,
}

fn user_known_hosts() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".ssh")
        .join("known_hosts")
}

impl client::Handler for Client {
    type Error = russh::Error;

    /// Like `StrictHostKeyChecking=accept-new`: known keys from ~/.ssh/known_hosts or
    /// gpuwho's own file are checked, unknown ones remembered, changed ones rejected.
    async fn check_server_key(
        &mut self,
        key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let pk = match key {
            russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
            russh::keys::PublicKeyOrCertificate::Certificate(c) => c.public_key().clone().into(),
        };
        let _g = KNOWN_HOSTS.lock().unwrap();
        for path in [user_known_hosts(), config::known_hosts_path()] {
            match keys::check_known_hosts_path(&self.host, self.port, &pk, &path) {
                Ok(true) => return Ok(true),
                Err(keys::Error::KeyChanged { .. }) => {
                    *self.changed.lock().unwrap() = Some(path.display().to_string());
                    return Ok(false);
                }
                _ => {}
            }
        }
        let _ = keys::known_hosts::learn_known_hosts_path(
            &self.host,
            self.port,
            &pk,
            config::known_hosts_path(),
        );
        Ok(true)
    }
}

// ---------------------------------------------------------------- the agent's pipes

pub struct AgentIo {
    reader: Box<dyn AsyncBufRead + Unpin + Send>,
    writer: Box<dyn AsyncWrite + Unpin + Send>,
    #[allow(dead_code)] // kept for debugging output
    pub kind: &'static str,
    _keep: Vec<Box<dyn std::any::Any + Send>>,
}

impl AgentIo {
    /// The next JSON document, skipping rc-file noise and the agent's {"ready": true}.
    async fn next_json(&mut self) -> Result<Value, Failure> {
        let mut noise = String::new();
        loop {
            let mut line = String::new();
            let n = self
                .reader
                .read_line(&mut line)
                .await
                .map_err(|e| io_failure(&e))?;
            if n == 0 {
                return Err(Failure::new(if noise.is_empty() {
                    "agent exited".to_string()
                } else {
                    noise
                }));
            }
            let t = line.trim();
            if t.starts_with('{') {
                if let Ok(v) = serde_json::from_str::<Value>(t) {
                    return Ok(v);
                }
            }
            if !t.is_empty() {
                noise = t.chars().take(200).collect();
            }
        }
    }

    async fn ready(&mut self) -> Result<(), Failure> {
        match tokio::time::timeout(START_TIMEOUT, self.next_json()).await {
            Ok(Ok(v)) if v.get("ready").is_some() => Ok(()),
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(Failure::new("agent didn't start in time")),
        }
    }

    pub async fn request(&mut self) -> Result<Value, Failure> {
        self.writer
            .write_all(b"\n")
            .await
            .map_err(|e| io_failure(&e))?;
        self.writer.flush().await.map_err(|e| io_failure(&e))?;
        loop {
            let v = self.next_json().await?;
            if v.get("ready").is_none() {
                return Ok(v);
            }
        }
    }
}

// ---------------------------------------------------------------- SSH

struct Conn {
    handle: Handle<Client>,
    agent: AgentIo,
    _proxy: Option<tokio::process::Child>,
}

async fn spawn_proxy(
    argv: Vec<String>,
) -> Result<
    (
        tokio::process::Child,
        impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
    ),
    Failure,
> {
    let mut child = tokio::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| Failure::new(format!("can't start proxy `{}`: {}", argv.join(" "), e)))?;
    let stream = tokio::io::join(child.stdout.take().unwrap(), child.stdin.take().unwrap());
    Ok((child, stream))
}

/// Exec `cmd`, optionally feeding `input`; (exit status, stdout).
async fn run(
    handle: &Handle<Client>,
    cmd: &str,
    input: Option<&[u8]>,
) -> Result<(Option<u32>, String), Failure> {
    let mut ch = handle
        .channel_open_session()
        .await
        .map_err(|e| Failure::new(format!("SSH: {}", e)))?;
    ch.exec(true, cmd)
        .await
        .map_err(|e| Failure::new(format!("SSH: {}", e)))?;
    if let Some(data) = input {
        ch.data(data)
            .await
            .map_err(|e| Failure::new(format!("SSH: {}", e)))?;
    }
    let _ = ch.eof().await;
    let (mut code, mut out) = (None, Vec::new());
    while let Some(msg) = ch.wait().await {
        match msg {
            ChannelMsg::Data { data } => out.extend_from_slice(&data),
            ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
            _ => {}
        }
    }
    Ok((code, String::from_utf8_lossy(&out).into_owned()))
}

async fn authenticate(
    handle: &mut Handle<Client>,
    user: &str,
    cfg: &ServerCfg,
    s: &sshcfg::Settings,
) -> Result<(), Failure> {
    let ssh_err = |e: russh::Error| Failure::new(format!("SSH: {}", e));
    // servers with a stored password: go straight to it, sshd drops you after MaxAuthTries keys
    if let (Some(pw), None) = (&cfg.password, &cfg.key) {
        let r = handle
            .authenticate_password(user, pw.clone())
            .await
            .map_err(ssh_err)?;
        return if r.success() {
            Ok(())
        } else {
            Err(Failure::user("auth failed: Authentication failed."))
        };
    }
    let only = s.identities_only;
    if !only && cfg.key.is_none() {
        if let Ok(mut agent) = keys::agent::client::AgentClient::connect_env().await {
            if let Ok(ids) = agent.request_identities().await {
                for id in ids {
                    let keys::agent::AgentIdentity::PublicKey { key, .. } = id else {
                        continue;
                    };
                    let hash = handle
                        .best_supported_rsa_hash()
                        .await
                        .ok()
                        .flatten()
                        .flatten();
                    if let Ok(r) = handle
                        .authenticate_publickey_with(user, key, hash, &mut agent)
                        .await
                    {
                        if r.success() {
                            return Ok(());
                        }
                    }
                }
            }
        }
    }
    let mut files: Vec<PathBuf> = match &cfg.key {
        Some(k) => vec![sshcfg::expand_home(k)],
        None => s
            .identity_files
            .iter()
            .map(|f| sshcfg::expand_home(f))
            .collect(),
    };
    if !only && cfg.key.is_none() {
        for name in ["id_ed25519", "id_ecdsa", "id_rsa"] {
            let p = dirs::home_dir().unwrap_or_default().join(".ssh").join(name);
            if !files.contains(&p) {
                files.push(p);
            }
        }
    }
    for f in files.iter().filter(|f| f.exists()) {
        let Ok(k) = keys::load_secret_key(f, None) else {
            continue;
        }; // encrypted keys go through the agent
        let hash = handle
            .best_supported_rsa_hash()
            .await
            .ok()
            .flatten()
            .flatten();
        let r = handle
            .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(k), hash))
            .await
            .map_err(ssh_err)?;
        if r.success() {
            return Ok(());
        }
    }
    if let Some(pw) = &cfg.password {
        let r = handle
            .authenticate_password(user, pw.clone())
            .await
            .map_err(ssh_err)?;
        if r.success() {
            return Ok(());
        }
    }
    Err(Failure::user(
        "auth failed: no key or password was accepted",
    ))
}

async fn connect(
    cfg: &ServerCfg,
) -> Result<(Handle<Client>, Option<tokio::process::Child>), Failure> {
    let s = sshcfg::settings(&cfg.host);
    let host = s.hostname.clone().unwrap_or_else(|| cfg.host.clone());
    let port = cfg.port.or(s.port).unwrap_or(22);
    let user = cfg
        .user
        .clone()
        .or(s.user.clone())
        .unwrap_or_else(config::me);
    let config = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        nodelay: true,
        ..Default::default()
    });
    let changed = Arc::new(Mutex::new(None));
    let handler = Client {
        host: host.clone(),
        port,
        changed: changed.clone(),
    };
    let _slot = CONNECTING.acquire().await.unwrap();

    let mut proxy = None;
    let connected = if let Some(jump) = &s.proxy_jump {
        let (child, stream) = spawn_proxy(sshcfg::jump_command(jump, &host, port)).await?;
        proxy = Some(child);
        tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            client::connect_stream(config, stream, handler),
        )
        .await
    } else if let Some(pc) = &s.proxy_command {
        let cmd = pc
            .replace("%h", &host)
            .replace("%p", &port.to_string())
            .replace("%r", &user);
        let (child, stream) = spawn_proxy(vec!["sh".into(), "-c".into(), cmd]).await?;
        proxy = Some(child);
        tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            client::connect_stream(config, stream, handler),
        )
        .await
    } else {
        let addrs: Vec<_> = match tokio::net::lookup_host((host.as_str(), port)).await {
            Ok(a) => a.collect(),
            Err(e) => return Err(Failure::new(format!("unknown host: {}", e))),
        };
        let mut last = Failure::new("unknown host: no addresses");
        let mut sock = None;
        for addr in addrs {
            match tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(addr)).await
            {
                Ok(Ok(s)) => {
                    sock = Some(s);
                    break;
                }
                Ok(Err(e)) => last = io_failure(&e),
                Err(_) => last = Failure::new("unreachable (no answer in time)"),
            }
        }
        let Some(sock) = sock else { return Err(last) };
        let _ = sock.set_nodelay(true);
        tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            client::connect_stream(config, sock, handler),
        )
        .await
    };
    let mut handle = match connected {
        Err(_) => return Err(Failure::new("not answering SSH (handshake timed out)")),
        Ok(Err(e)) => {
            if let Some(file) = changed.lock().unwrap().take() {
                let target = if port == 22 {
                    host.clone()
                } else {
                    format!("[{}]:{}", host, port)
                };
                return Err(Failure::user(format!(
                    "host key changed for {}: if expected, run `ssh-keygen -R {}` and remove it from {}",
                    target, target, file
                )));
            }
            return Err(Failure::new(format!("not answering SSH ({})", e)));
        }
        Ok(Ok(h)) => h,
    };
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, authenticate(&mut handle, &user, cfg, &s)).await {
        Ok(Ok(())) => Ok((handle, proxy)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(Failure::new("auth timed out")),
    }
}

async fn into_agent(
    handle: &Handle<Client>,
    cmd: &str,
    kind: &'static str,
) -> Result<AgentIo, Failure> {
    let ch = handle
        .channel_open_session()
        .await
        .map_err(|e| Failure::new(format!("SSH: {}", e)))?;
    ch.exec(false, cmd)
        .await
        .map_err(|e| Failure::new(format!("SSH: {}", e)))?;
    let (r, w) = tokio::io::split(ch.into_stream());
    let mut io = AgentIo {
        reader: Box::new(BufReader::new(r)),
        writer: Box::new(w),
        kind,
        _keep: vec![],
    };
    if kind == "rust" {
        io.ready().await?; // probe.py only speaks when asked; its errors show on the first request
    }
    Ok(io)
}

/// Prefer the uploaded Rust agent, fall back to python3. Returns whether Rust failed for
/// a reason that will persist (so we stop trying on this server).
async fn start_agent(
    handle: &Handle<Client>,
    cfg: &ServerCfg,
    idle: u64,
    rust_broken: &mut bool,
) -> Result<AgentIo, Failure> {
    let pref = cfg.agent.as_deref().unwrap_or("auto");
    let mut why_not_rust = String::new();
    if pref != "python" && !*rust_broken && !agents::EMBEDDED.is_empty() {
        let (_, out) = run(handle, &agents::sh(&agents::probe_script()), None).await?;
        let kv = |k: &str| {
            out.lines()
                .find_map(|l| l.strip_prefix(&format!("{}=", k)))
                .map(str::to_string)
        };
        let arch = kv("arch").unwrap_or_default();
        let dir = kv("dir").unwrap_or_default();
        match (agents::embedded(&arch), dir.is_empty()) {
            (None, _) => why_not_rust = format!("no agent built for {}", arch),
            (_, true) => why_not_rust = "no writable directory".into(),
            (Some((gz, tag)), false) => {
                let name = agents::file_name(
                    match arch.as_str() {
                        "arm64" => "aarch64",
                        a => a,
                    },
                    tag,
                );
                let cached = out.lines().any(|l| l == format!("cached={}", name));
                let mut uploaded = cached;
                if !cached {
                    let gzip = kv("gzip").is_some();
                    let raw;
                    let body: &[u8] = if gzip {
                        gz
                    } else {
                        raw = {
                            use std::io::Read;
                            let mut v = Vec::new();
                            flate2::read::GzDecoder::new(gz)
                                .read_to_end(&mut v)
                                .map_err(|e| Failure::new(e.to_string()))?;
                            v
                        };
                        &raw
                    };
                    let (code, res) = run(
                        handle,
                        &agents::sh(&agents::upload_script(&dir, &name, gzip)),
                        Some(body),
                    )
                    .await?;
                    uploaded = code == Some(0) && res.trim() == "ok";
                    if !uploaded {
                        why_not_rust = format!("upload failed: {}", res.trim());
                    }
                }
                if uploaded {
                    match into_agent(handle, &agents::rust_command(&dir, &name, idle), "rust").await
                    {
                        Ok(io) => return Ok(io),
                        Err(e) => {
                            *rust_broken = true; // noexec, old glibc, ...: the same next time
                            why_not_rust = e.msg;
                        }
                    }
                }
            }
        }
        if pref == "rust" {
            return Err(Failure::new(format!("rust agent: {}", why_not_rust)));
        }
    }
    into_agent(handle, &agents::python_command(idle), "python")
        .await
        .map_err(|e| {
            if why_not_rust.is_empty() {
                e
            } else {
                Failure::new(format!("{} (rust agent: {})", e.msg, why_not_rust))
            }
        })
}

/// Run a shell script on a server (for claims and kill), feeding `input` on stdin.
pub async fn exec(
    cfg: &ServerCfg,
    script: &str,
    input: Option<Vec<u8>>,
) -> Result<(Option<u32>, String), Failure> {
    if cfg.is_local() {
        use tokio::io::AsyncReadExt;
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| Failure::new(e.to_string()))?;
        let mut stdin = child.stdin.take().unwrap();
        if let Some(data) = input {
            stdin.write_all(&data).await.map_err(|e| io_failure(&e))?;
        }
        drop(stdin);
        let mut out = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut out)
            .await
            .map_err(|e| io_failure(&e))?;
        let mut err = String::new();
        let _ = child.stderr.take().unwrap().read_to_string(&mut err).await;
        let status = child.wait().await.map_err(|e| io_failure(&e))?;
        return Ok((status.code().map(|c| c as u32), out + &err));
    }
    let (handle, _proxy) = connect(cfg).await?;
    run(
        &handle,
        &agents::sh(&format!("exec 2>&1; {}", script)),
        input.as_deref(),
    )
    .await
}

// ---------------------------------------------------------------- sources

pub struct Remote {
    cfg: ServerCfg,
    idle: u64,
    conn: Option<Conn>,
    rust_broken: bool,
}

impl Remote {
    pub fn new(cfg: ServerCfg, idle: u64) -> Self {
        Remote {
            cfg,
            idle,
            conn: None,
            rust_broken: false,
        }
    }

    async fn ensure(&mut self) -> Result<&mut AgentIo, Failure> {
        if self.conn.as_ref().is_some_and(|c| c.handle.is_closed()) {
            self.conn = None;
        }
        if self.conn.is_none() {
            let (handle, proxy) = connect(&self.cfg).await?;
            let agent = start_agent(&handle, &self.cfg, self.idle, &mut self.rust_broken).await?;
            self.conn = Some(Conn {
                handle,
                agent,
                _proxy: proxy,
            });
        }
        Ok(&mut self.conn.as_mut().unwrap().agent)
    }

    pub async fn poll(&mut self) -> Outcome {
        let res = match self.ensure().await {
            Ok(agent) => tokio::time::timeout(REQUEST_TIMEOUT, agent.request())
                .await
                .unwrap_or_else(|_| {
                    Err(Failure::new(format!(
                        "no answer within {}s",
                        REQUEST_TIMEOUT.as_secs()
                    )))
                }),
            Err(e) => Err(e),
        };
        match res {
            Ok(v) if v.get("fatal").is_some() => {
                Outcome::AgentError(v["fatal"].as_str().unwrap_or("?").to_string())
            }
            Ok(v) => Outcome::Data(v),
            Err(f) => {
                self.conn = None; // drop the connection; the agent sees EOF and exits
                Outcome::Failed {
                    msg: f.msg,
                    needs_user: f.needs_user,
                }
            }
        }
    }
}

/// `host: local`: the agent as a child process of ours.
pub struct Local {
    idle: u64,
    python: bool,
    io: Option<AgentIo>,
}

impl Local {
    pub fn new(idle: u64, python: bool) -> Self {
        Local {
            idle,
            python,
            io: None,
        }
    }

    async fn spawn(idle: u64, python: bool) -> Result<AgentIo, Failure> {
        let rust = if python {
            None
        } else {
            agents::local_binary().and_then(|bin| write_local_agent(&bin).ok())
        };
        let (mut cmd, kind) = match rust {
            Some(path) => {
                let mut c = tokio::process::Command::new(path);
                c.arg(idle.to_string());
                (c, "rust")
            }
            None => {
                let mut c = tokio::process::Command::new("python3");
                c.arg("-u")
                    .arg("-c")
                    .arg(agents::PROBE_PY)
                    .arg(idle.to_string());
                (c, "python")
            }
        };
        let mut child = cmd
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| Failure::new(format!("can't start the agent: {}", e)))?;
        let mut io = AgentIo {
            reader: Box::new(BufReader::new(child.stdout.take().unwrap())),
            writer: Box::new(child.stdin.take().unwrap()),
            kind,
            _keep: vec![Box::new(child)],
        };
        if kind == "rust" {
            io.ready().await?;
        }
        Ok(io)
    }

    pub async fn poll(&mut self) -> Outcome {
        if self.io.is_none() {
            match Self::spawn(self.idle, self.python).await {
                Ok(io) => self.io = Some(io),
                Err(f) => {
                    return Outcome::Failed {
                        msg: f.msg,
                        needs_user: false,
                    }
                }
            }
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, self.io.as_mut().unwrap().request()).await {
            Ok(Ok(v)) if v.get("fatal").is_some() => {
                Outcome::AgentError(v["fatal"].as_str().unwrap_or("?").to_string())
            }
            Ok(Ok(v)) => Outcome::Data(v),
            Ok(Err(f)) => {
                self.io = None;
                Outcome::Failed {
                    msg: f.msg,
                    needs_user: false,
                }
            }
            Err(_) => {
                self.io = None;
                Outcome::Failed {
                    msg: "no answer in time".into(),
                    needs_user: false,
                }
            }
        }
    }
}

#[cfg(unix)]
fn write_local_agent(bin: &[u8]) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|d| d.is_dir())
        .unwrap_or_else(|| config::dir().join("cache"));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("gpuwho-agent-{}-local", env!("CARGO_PKG_VERSION")));
    if std::fs::metadata(&path)
        .map(|m| m.len() as usize != bin.len())
        .unwrap_or(true)
    {
        let tmp = path.with_extension(format!("{}", std::process::id()));
        std::fs::write(&tmp, bin)?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o700))?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(path)
}

#[cfg(not(unix))]
fn write_local_agent(_bin: &[u8]) -> std::io::Result<PathBuf> {
    Err(std::io::Error::other("local agent needs Linux"))
}

pub enum Source {
    Remote(Box<Remote>),
    Local(Local),
    Demo(Box<crate::demo::DemoServer>),
}

impl Source {
    pub fn for_cfg(cfg: &ServerCfg, idle: u64) -> Self {
        if cfg.is_local() {
            Source::Local(Local::new(idle, cfg.agent.as_deref() == Some("python")))
        } else {
            Source::Remote(Box::new(Remote::new(cfg.clone(), idle)))
        }
    }

    pub async fn poll(&mut self) -> Outcome {
        match self {
            Source::Remote(r) => r.poll().await,
            Source::Local(l) => l.poll().await,
            Source::Demo(d) => d.poll(),
        }
    }

    pub fn is_demo(&self) -> bool {
        matches!(self, Source::Demo(_))
    }
}
