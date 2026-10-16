//! Soft reservations: `gpuwho claim gpu1:0,1 --for 6h --note "rebuttal"`.
//!
//! Nothing is enforced; a claim is a note everyone sees on the dashboard, and `free` skips
//! GPUs claimed by someone else. Claims live on each server in /var/tmp/gpuwho-claims (a
//! sticky, world-writable directory like /tmp), one file per user, written over SSH. Files
//! get fresh random names and the agents attribute them by file owner, so nobody can
//! claim as someone else or block someone's claims by pre-creating their file.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

pub const DIR: &str = "/var/tmp/gpuwho-claims";

/// "gpu1:0,1" / "gpu1:0-3" / "gpu1" (all GPUs) -> (server, Some(indices) or None)
pub fn parse_target(spec: &str) -> Result<(String, Option<Vec<u64>>)> {
    let (server, gpus) = match spec.split_once(':') {
        Some((s, g)) => (s, Some(g)),
        None => (spec, None),
    };
    if server.is_empty() {
        bail!("expected SERVER[:GPUS], e.g. gpu1:0,1");
    }
    let Some(gpus) = gpus.filter(|g| !g.is_empty() && *g != "all") else {
        return Ok((server.into(), None));
    };
    let mut out = Vec::new();
    for part in gpus.split(',') {
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b): (u64, u64) = (a.trim().parse()?, b.trim().parse()?);
                if b < a || b - a > 64 {
                    bail!("bad GPU range {}", part);
                }
                out.extend(a..=b);
            }
            None => out.push(
                part.trim()
                    .parse()
                    .map_err(|_| anyhow!("bad GPU index '{}'", part))?,
            ),
        }
    }
    out.sort();
    out.dedup();
    Ok((server.into(), Some(out)))
}

/// "6h", "90m", "2d", "1h30m", "45s" -> seconds
pub fn parse_duration(s: &str) -> Result<f64> {
    let mut total = 0.0;
    let mut num = String::new();
    for c in s.trim().chars() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
            continue;
        }
        let n: f64 = num.parse().map_err(|_| anyhow!("bad duration '{}'", s))?;
        num.clear();
        total += n * match c {
            's' => 1.0,
            'm' => 60.0,
            'h' => 3600.0,
            'd' => 86400.0,
            'w' => 7.0 * 86400.0,
            _ => bail!("bad duration '{}' (use s, m, h, d, w)", s),
        };
    }
    if !num.is_empty() {
        total += num
            .parse::<f64>()
            .map_err(|_| anyhow!("bad duration '{}'", s))?
            * 3600.0; // bare number = hours
    }
    if total <= 0.0 {
        bail!("duration must be positive");
    }
    Ok(total)
}

/// Prints the caller's current claim files on that server, one JSON document per line.
pub fn read_script() -> String {
    format!(
        r#"d={dir}; for f in "$d"/*.json; do [ -f "$f" ] && [ -O "$f" ] && cat "$f" && echo; done; true"#,
        dir = DIR
    )
}

/// Replaces the caller's claim files with stdin (or just removes them if stdin is empty).
pub fn write_script() -> String {
    format!(
        r#"d={dir}; umask 022
mkdir -p "$d" 2>/dev/null && chmod 1777 "$d" 2>/dev/null
[ -d "$d" ] && [ -w "$d" ] || {{ echo "can't write $d"; exit 1; }}
t=$(mktemp "$d/claim.XXXXXXXX") || exit 1
cat > "$t"
for f in "$d"/*.json; do [ -f "$f" ] && [ -O "$f" ] && rm -f "$f"; done
if [ -s "$t" ]; then mv -f "$t" "$t.json"; else rm -f "$t"; fi
echo ok"#,
        dir = DIR
    )
}

/// Own claims from read_script output, dropping expired ones.
pub fn own_claims(out: &str, now: f64) -> Vec<Value> {
    out.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .flat_map(|d| d["claims"].as_array().cloned().unwrap_or_default())
        .filter(|c| c["until"].as_f64().is_some_and(|u| u > now))
        .collect()
}

/// Add a claim; a new claim on a GPU replaces the old one on that GPU.
pub fn add(mut claims: Vec<Value>, gpus: &[u64], until: f64, note: &str, now: f64) -> Vec<Value> {
    claims = remove(claims, Some(gpus));
    claims.push(json!({"gpus": gpus, "since": now, "until": until, "note": note}));
    claims
}

/// Drop `gpus` (or everything) from the claims.
pub fn remove(claims: Vec<Value>, gpus: Option<&[u64]>) -> Vec<Value> {
    let Some(gpus) = gpus else { return vec![] };
    claims
        .into_iter()
        .filter_map(|mut c| {
            let left: Vec<u64> = c["gpus"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_u64)
                .filter(|g| !gpus.contains(g))
                .collect();
            if left.is_empty() {
                None
            } else {
                c["gpus"] = json!(left);
                Some(c)
            }
        })
        .collect()
}

pub fn document(claims: &[Value]) -> String {
    if claims.is_empty() {
        String::new()
    } else {
        json!({"claims": claims}).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        assert_eq!(
            parse_target("gpu1:0,1").unwrap(),
            ("gpu1".into(), Some(vec![0, 1]))
        );
        assert_eq!(
            parse_target("gpu1:3,0-2").unwrap(),
            ("gpu1".into(), Some(vec![0, 1, 2, 3]))
        );
        assert_eq!(parse_target("gpu1").unwrap(), ("gpu1".into(), None));
        assert_eq!(parse_target("gpu1:all").unwrap(), ("gpu1".into(), None));
        assert!(parse_target("gpu1:x").is_err() && parse_target(":1").is_err());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("6h").unwrap(), 21600.0);
        assert_eq!(parse_duration("1h30m").unwrap(), 5400.0);
        assert_eq!(parse_duration("2d").unwrap(), 172800.0);
        assert_eq!(parse_duration("3").unwrap(), 10800.0);
        assert!(parse_duration("0m").is_err() && parse_duration("5x").is_err());
    }

    #[test]
    fn merge() {
        let now = 1000.0;
        let out = "{\"claims\":[{\"gpus\":[0,1],\"until\":5000},{\"gpus\":[2],\"until\":10}]}\n\n";
        let mine = own_claims(out, now);
        assert_eq!(mine.len(), 1); // the expired one is gone
        let mine = add(mine, &[1, 3], 9000.0, "eval", now);
        let gpus: Vec<Value> = mine.iter().map(|c| c["gpus"].clone()).collect();
        assert_eq!(gpus, vec![json!([0]), json!([1, 3])]);
        assert!(remove(mine.clone(), None).is_empty());
        assert_eq!(remove(mine, Some(&[0])).len(), 1);
        assert_eq!(document(&[]), "");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn scripts_roundtrip_in_a_scratch_dir() {
        use std::process::{Command, Stdio};
        let dir = std::env::temp_dir().join(format!("gpuwho-claim-test-{}", std::process::id()));
        let swap = |s: String| s.replace(DIR, dir.to_str().unwrap());
        let run = |script: String, input: &str| {
            use std::io::Write;
            let mut c = Command::new("sh")
                .arg("-c")
                .arg(script)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            c.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
            String::from_utf8(c.wait_with_output().unwrap().stdout).unwrap()
        };
        let doc = document(&add(vec![], &[0], 9e12, "x", 1.0));
        assert_eq!(run(swap(write_script()), &doc).trim(), "ok");
        assert_eq!(own_claims(&run(swap(read_script()), ""), 2.0).len(), 1);
        assert_eq!(run(swap(write_script()), "").trim(), "ok"); // unclaim everything
        assert!(own_claims(&run(swap(read_script()), ""), 2.0).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
