//! Where and how to connect: the server entry first, then ~/.ssh/config, the way `ssh`
//! resolves it. `ssh -G` is asked first (it understands Include and Match); without an
//! OpenSSH client a small parser handles Host blocks.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_files: Vec<String>,
    pub identities_only: bool,
    pub proxy_jump: Option<String>,
    pub proxy_command: Option<String>,
}

/// 'alice@gpu1:2222', '[fe80::1]:22' or 'gpu1' -> (user, host, port).
pub fn parse_target(spec: &str) -> (Option<String>, String, Option<u16>) {
    let (user, rest) = match spec.rfind('@') {
        Some(i) => (
            Some(spec[..i].to_string()).filter(|u| !u.is_empty()),
            &spec[i + 1..],
        ),
        None => (None, spec),
    };
    if let Some(inner) = rest.strip_prefix('[') {
        if let Some((host, tail)) = inner.split_once(']') {
            let port = tail.strip_prefix(':').and_then(|p| p.parse().ok());
            return (user, host.to_string(), port);
        }
    }
    if rest.matches(':').count() == 1 {
        let (h, p) = rest.split_once(':').unwrap();
        if let Ok(port) = p.parse() {
            return (user, h.to_string(), Some(port));
        }
    }
    (user, rest.to_string(), None)
}

pub fn parse_ssh_g(text: &str) -> Settings {
    let mut s = Settings::default();
    for line in text.lines() {
        let Some((k, v)) = line.split_once(' ') else {
            continue;
        };
        let v = v.trim().to_string();
        match k {
            "hostname" => s.hostname = Some(v),
            "user" => s.user = Some(v),
            "port" => s.port = v.parse().ok(),
            "identityfile" => s.identity_files.push(v),
            "identitiesonly" => s.identities_only = v == "yes",
            "proxyjump" if v != "none" => s.proxy_jump = Some(v),
            "proxycommand" if v != "none" => s.proxy_command = Some(v),
            _ => {}
        }
    }
    s
}

fn glob(pattern: &str, text: &str) -> bool {
    fn rec(p: &[u8], t: &[u8]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (Some(b'*'), _) => rec(&p[1..], t) || (!t.is_empty() && rec(p, &t[1..])),
            (Some(b'?'), Some(_)) => rec(&p[1..], &t[1..]),
            (Some(a), Some(b)) if a.eq_ignore_ascii_case(b) => rec(&p[1..], &t[1..]),
            _ => false,
        }
    }
    rec(pattern.as_bytes(), text.as_bytes())
}

/// Minimal ssh_config reader for when `ssh -G` isn't there: Host blocks with * ? and !,
/// first value wins, like OpenSSH.
pub fn parse_config(text: &str, alias: &str) -> Settings {
    let mut s = Settings::default();
    let mut active = true; // lines before the first Host apply to everyone
    let mut seen: std::collections::HashSet<String> = Default::default();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = match line.split_once([' ', '=', '\t']) {
            Some((k, v)) => (
                k.to_ascii_lowercase(),
                v.trim_start_matches([' ', '=', '\t']).trim(),
            ),
            None => continue,
        };
        if key == "host" {
            let pats: Vec<&str> = value.split_whitespace().collect();
            let neg = pats
                .iter()
                .any(|p| p.strip_prefix('!').is_some_and(|p| glob(p, alias)));
            active = !neg && pats.iter().any(|p| !p.starts_with('!') && glob(p, alias));
            continue;
        }
        if key == "match" {
            active = false; // needs ssh -G
            continue;
        }
        if !active || (key != "identityfile" && !seen.insert(key.clone())) {
            continue;
        }
        let v = value.trim_matches('"').to_string();
        match key.as_str() {
            "hostname" => s.hostname = Some(v.replace("%h", alias)),
            "user" => s.user = Some(v),
            "port" => s.port = v.parse().ok(),
            "identityfile" => s.identity_files.push(v),
            "identitiesonly" => s.identities_only = v.eq_ignore_ascii_case("yes"),
            "proxyjump" if !v.eq_ignore_ascii_case("none") => s.proxy_jump = Some(v),
            "proxycommand" if !v.eq_ignore_ascii_case("none") => s.proxy_command = Some(v),
            _ => {}
        }
    }
    s
}

fn config_path() -> PathBuf {
    match std::env::var_os("GPUWHO_SSH_CONFIG") {
        Some(p) => PathBuf::from(p),
        None => dirs::home_dir()
            .unwrap_or_default()
            .join(".ssh")
            .join("config"),
    }
}

static CACHE: Mutex<Option<HashMap<String, Settings>>> = Mutex::new(None);

/// What `ssh ALIAS` would use. Cached for the life of the process.
pub fn settings(alias: &str) -> Settings {
    if let Some(s) = CACHE
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .get(alias)
    {
        return s.clone();
    }
    let mut cmd = Command::new("ssh");
    if let Some(p) = std::env::var_os("GPUWHO_SSH_CONFIG") {
        cmd.arg("-F").arg(p);
    }
    let resolved = cmd
        .arg("-G")
        .arg(alias)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_ssh_g(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_else(|| {
            parse_config(
                &std::fs::read_to_string(config_path()).unwrap_or_default(),
                alias,
            )
        });
    CACHE
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(alias.to_string(), resolved.clone());
    resolved
}

pub fn expand_home(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
        None => PathBuf::from(p),
    }
}

/// ProxyJump 'a,b' -> an `ssh -W` ProxyCommand through those hosts (run by OpenSSH, which
/// handles their keys, agent and known_hosts).
pub fn jump_command(jump: &str, host: &str, port: u16) -> Vec<String> {
    let hops: Vec<&str> = jump
        .split(',')
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .collect();
    let mut args = vec!["ssh".to_string(), "-o".into(), "BatchMode=yes".into()];
    if hops.len() > 1 {
        args.push("-J".into());
        args.push(hops[..hops.len() - 1].join(","));
    }
    args.push("-W".into());
    args.push(format!("{}:{}", host, port));
    args.push(hops.last().copied().unwrap_or_default().to_string());
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        let t = |s: &str| parse_target(s);
        assert_eq!(t("gpu1"), (None, "gpu1".into(), None));
        assert_eq!(
            t("alice@gpu1:2222"),
            (Some("alice".into()), "gpu1".into(), Some(2222))
        );
        assert_eq!(t("10.0.0.11:22"), (None, "10.0.0.11".into(), Some(22)));
        assert_eq!(t("[fe80::1]:2200"), (None, "fe80::1".into(), Some(2200)));
        assert_eq!(t("fe80::1"), (None, "fe80::1".into(), None));
        assert_eq!(
            t("me@host:notaport"),
            (Some("me".into()), "host:notaport".into(), None)
        );
    }

    #[test]
    fn ssh_g_output() {
        let s = parse_ssh_g("user alice\nhostname 10.0.0.5\nport 2222\nidentityfile ~/.ssh/a\nidentityfile ~/.ssh/b\nproxyjump none\n");
        assert_eq!(s.user.as_deref(), Some("alice"));
        assert_eq!(
            (s.port, s.identity_files.len(), s.proxy_jump),
            (Some(2222), 2, None)
        );
    }

    #[test]
    fn fallback_parser() {
        let cfg = "User everyone\nHost lab-* !lab-bad\n  User alice\n  Port 2222\n  ProxyJump bastion\nHost pw\n  HostName 10.9.9.9\n";
        let s = parse_config(cfg, "lab-g1");
        assert_eq!(
            (s.user.as_deref(), s.port, s.proxy_jump.as_deref()),
            (Some("everyone"), Some(2222), Some("bastion"))
        );
        assert_eq!(parse_config(cfg, "lab-bad").port, None);
        assert_eq!(
            parse_config(cfg, "pw").hostname.as_deref(),
            Some("10.9.9.9")
        );
    }

    #[test]
    fn jumps() {
        assert_eq!(
            jump_command("a,b", "10.0.0.1", 22).join(" "),
            "ssh -o BatchMode=yes -J a -W 10.0.0.1:22 b"
        );
        assert_eq!(
            jump_command("bastion", "h", 2222).join(" "),
            "ssh -o BatchMode=yes -W h:2222 bastion"
        );
    }
}
