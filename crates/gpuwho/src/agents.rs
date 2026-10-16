//! The two agents gpuwho can start on a server, and the shell snippets that start them.
//!
//! * the Rust agent, embedded in this binary per architecture: uploaded once to the
//!   server's per-user runtime directory (or ~/.cache/gpuwho) and reused while it's there
//! * probe.py, piped through `python3 -c`: nothing touches the disk; the fallback for
//!   servers where the binary can't run (unknown arch, noexec, very old glibc)
//!
//! Both answer one JSON line per newline with the same document.

use std::io::Read;

include!(concat!(env!("OUT_DIR"), "/agents.rs"));

pub const PROBE_PY: &str = include_str!("../assets/probe.py");

pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// A command for the remote login shell that runs `script` with sh, whatever that shell is.
pub fn sh(script: &str) -> String {
    format!("sh -c {}", shell_quote(script))
}

/// (gzip bytes, hash tag) of the embedded agent for `arch` (uname -m), if any.
pub fn embedded(arch: &str) -> Option<(&'static [u8], &'static str)> {
    let arch = match arch {
        "arm64" => "aarch64",
        "amd64" => "x86_64",
        a => a,
    };
    EMBEDDED
        .iter()
        .find(|(a, _, _)| *a == arch)
        .map(|(_, gz, tag)| (*gz, *tag))
}

pub fn file_name(arch: &str, tag: &str) -> String {
    format!(
        "gpuwho-agent-{}-{}-{}",
        env!("CARGO_PKG_VERSION"),
        arch,
        tag
    )
}

/// The uncompressed agent for this machine (used for `local` servers).
pub fn local_binary() -> Option<Vec<u8>> {
    let (gz, _) = embedded(std::env::consts::ARCH)?;
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(gz)
        .read_to_end(&mut out)
        .ok()?;
    Some(out)
}

/// Prints arch, a usable directory, cached agents, and whether gzip/python3 exist.
pub fn probe_script() -> String {
    let mut names = Vec::new();
    for (arch, _, tag) in EMBEDDED {
        names.push(file_name(arch, tag));
    }
    format!(
        r#"u=$(id -u); a=$(uname -m)
for d in "$XDG_RUNTIME_DIR" "/run/user/$u" "$HOME/.cache/gpuwho"; do
  [ -n "$d" ] || continue
  if [ "$d" = "$HOME/.cache/gpuwho" ]; then mkdir -p "$d" 2>/dev/null; chmod 700 "$d" 2>/dev/null; fi
  if [ -d "$d" ] && [ -w "$d" ] && [ -O "$d" ]; then break; fi
  d=
done
echo "arch=$a"; echo "dir=$d"
for f in {names}; do [ -n "$d" ] && [ -x "$d/$f" ] && echo "cached=$f"; done
command -v gzip >/dev/null 2>&1 && echo gzip=1
command -v python3 >/dev/null 2>&1 && echo python3=1
true"#,
        names = if names.is_empty() {
            "none".to_string()
        } else {
            names.join(" ")
        }
    )
}

/// Reads the (gzip or raw) agent from stdin into `dir/name`, atomically.
pub fn upload_script(dir: &str, name: &str, gzip: bool) -> String {
    let f = shell_quote(&format!("{}/{}", dir, name));
    format!(
        "umask 077; t={f}.$$; {} > \"$t\" && chmod 700 \"$t\" && mv -f \"$t\" {f} && echo ok",
        if gzip { "gzip -dc" } else { "cat" },
        f = f
    )
}

pub fn rust_command(dir: &str, name: &str, idle: u64) -> String {
    sh(&format!(
        "exec 2>&1; exec {} {}",
        shell_quote(&format!("{}/{}", dir, name)),
        idle
    ))
}

pub fn python_command(idle: u64) -> String {
    use std::io::Write;
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    z.write_all(PROBE_PY.as_bytes()).unwrap();
    let b64 = base64(&z.finish().unwrap());
    sh(&format!(
        "exec 2>&1; exec python3 -u -c \"import base64,zlib;exec(zlib.decompress(base64.b64decode('{}')))\" {}",
        b64, idle
    ))
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(sh("echo 'a'"), "sh -c 'echo '\\''a'\\'''");
    }

    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn python_agent_command_runs_in_a_shell() {
        // the exact command sent over SSH, run by a local shell
        use std::io::{BufRead, Write};
        if std::process::Command::new("python3")
            .arg("-V")
            .output()
            .is_err()
        {
            return;
        }
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(python_command(30))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.as_mut().unwrap().write_all(b"\n").unwrap();
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.as_mut().unwrap())
            .read_line(&mut line)
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(v["cpu"]["count"].as_i64().unwrap() > 0);
        drop(child.stdin.take());
        assert!(child.wait().unwrap().success());
    }
}
