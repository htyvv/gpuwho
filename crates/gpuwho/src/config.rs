//! Server list and host keys, kept private.
//!
//! servers.json may hold passwords, so the directory is forced to 0700 and every file in it
//! to 0600 each time it is loaded, and files owned by somebody else are refused. The format
//! is the same as gpuwho 0.x (Python), so existing configs keep working.
//! Set GPUWHO_CONFIG_DIR to keep them somewhere other than ~/.config/gpuwho.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const LOCAL_HOSTS: [&str; 3] = ["local", "localhost", "127.0.0.1"];

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ServerCfg {
    pub name: String,
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// "auto" (default), "rust" or "python": which agent to run on the server
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

impl ServerCfg {
    pub fn is_local(&self) -> bool {
        LOCAL_HOSTS.contains(&self.host.as_str())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Conf {
    #[serde(default)]
    pub servers: Vec<ServerCfg>,
    /// everything else in the file is kept as it was
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

pub fn dir() -> PathBuf {
    match std::env::var_os("GPUWHO_CONFIG_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".config")
            .join("gpuwho"),
    }
}

pub fn servers_path() -> PathBuf {
    dir().join("servers.json")
}

pub fn known_hosts_path() -> PathBuf {
    dir().join("known_hosts")
}

pub fn me() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "root".into())
}

#[cfg(unix)]
fn ensure_private(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let meta = fs::metadata(path).with_context(|| format!("can't read {}", path.display()))?;
    let uid = unsafe { libc::getuid() };
    if meta.uid() != uid {
        anyhow::bail!(
            "refusing to use {}: owned by uid {}, not you",
            path.display(),
            meta.uid()
        );
    }
    if meta.mode() & 0o777 != mode {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
        eprintln!("fixed permissions on {} -> {:o}", path.display(), mode);
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

/// Write via a private temp file and rename, so a crash never leaves a half-written file.
pub fn write_private(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    f.write_all(content.as_bytes())?;
    f.sync_all()?;
    drop(f);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

pub fn init_dir() -> Result<()> {
    let d = dir();
    fs::create_dir_all(&d).with_context(|| format!("can't create {}", d.display()))?;
    ensure_private(&d, 0o700)?;
    for path in [servers_path(), known_hosts_path()] {
        if !path.exists() {
            if path == servers_path() {
                write_private(&path, "{\n  \"servers\": []\n}\n")?;
            } else {
                write_private(&path, "")?;
            }
        }
        ensure_private(&path, 0o600)?;
    }
    Ok(())
}

pub fn load() -> Result<Conf> {
    init_dir()?;
    let text = fs::read_to_string(servers_path())?;
    if text.trim().is_empty() {
        return Ok(Conf::default());
    }
    serde_json::from_str(&text)
        .with_context(|| format!("{} is not valid JSON", servers_path().display()))
}

pub fn save(conf: &Conf) -> Result<()> {
    init_dir()?;
    write_private(
        &servers_path(),
        &(serde_json::to_string_pretty(conf)? + "\n"),
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // GPUWHO_CONFIG_DIR is process-wide: run these one at a time
    pub static ENV: Mutex<()> = Mutex::new(());

    fn mode(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn private_files_and_roundtrip() {
        let _g = ENV.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!("gpuwho-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        std::env::set_var("GPUWHO_CONFIG_DIR", &tmp);
        assert!(load().unwrap().servers.is_empty());
        assert_eq!(
            (mode(&tmp), mode(&servers_path()), mode(&known_hosts_path())),
            (0o700, 0o600, 0o600)
        );

        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(servers_path(), fs::Permissions::from_mode(0o644)).unwrap();
        let mut conf = load().unwrap();
        assert_eq!(mode(&servers_path()), 0o600);

        conf.servers.push(ServerCfg {
            name: "g1".into(),
            host: "10.0.0.1".into(),
            port: Some(2222),
            ..Default::default()
        });
        conf.extra.insert("future".into(), serde_json::json!(1));
        save(&conf).unwrap();
        let back = load().unwrap();
        assert_eq!(back.servers, conf.servers);
        assert_eq!(back.extra["future"], 1);
        // the 0.x format: user/port/password present, no agent field
        fs::write(
            servers_path(),
            r#"{"servers":[{"name":"a","host":"h","port":22,"user":"me","password":"pw"}]}"#,
        )
        .unwrap();
        let old = load().unwrap();
        assert_eq!(old.servers[0].password.as_deref(), Some("pw"));
        std::env::remove_var("GPUWHO_CONFIG_DIR");
        let _ = fs::remove_dir_all(&tmp);
    }
}
