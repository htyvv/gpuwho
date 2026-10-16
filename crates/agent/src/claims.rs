//! Soft reservations (`gpuwho claim`): one JSON file per user in a sticky, world-writable
//! directory. The owner is the file's owner, not its name, so nobody can claim as someone
//! else. Expired claims are dropped here.

use std::os::unix::fs::MetadataExt;

use serde_json::{json, Value};

pub const DIR: &str = "/var/tmp/gpuwho-claims";

pub fn read(dir: &str, now: f64, name_of: &mut impl FnMut(u32) -> String) -> Vec<Value> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let (Ok(meta), Ok(text)) = (std::fs::metadata(&path), std::fs::read_to_string(&path))
        else {
            continue;
        };
        let Ok(doc) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let user = name_of(meta.uid());
        for c in doc["claims"].as_array().into_iter().flatten() {
            let (Some(until), Some(gpus)) = (c["until"].as_f64(), c["gpus"].as_array()) else {
                continue;
            };
            if until <= now {
                continue;
            }
            let gpus: Vec<u64> = gpus.iter().filter_map(Value::as_u64).collect();
            let note: String = c["note"].as_str().unwrap_or("").chars().take(200).collect();
            out.push(json!({"user": user, "gpus": gpus, "until": until, "since": c["since"], "note": note}));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_live_claims_and_trusts_the_file_owner() {
        let dir = std::env::temp_dir().join(format!("gpuwho-claims-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("bob.json"), // written by us, named after someone else
            r#"{"claims": [{"gpus": [0, 1], "until": 2000, "note": "rebuttal"}, {"gpus": [2], "until": 500}]}"#,
        )
        .unwrap();
        std::fs::write(dir.join("junk.json"), "not json").unwrap();
        let me = unsafe { libc::getuid() };
        let got = read(dir.to_str().unwrap(), 1000.0, &mut |uid| {
            if uid == me {
                "me".into()
            } else {
                "?".into()
            }
        });
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["user"], "me");
        assert_eq!(got[0]["gpus"], json!([0, 1]));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
