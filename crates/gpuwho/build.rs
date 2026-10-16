//! Embed prebuilt agents in the client, so it can upload one to each server.
//!
//! GPUWHO_AGENT_X86_64 / GPUWHO_AGENT_AARCH64 point at gpuwho-agent binaries for those
//! server architectures (release builds use cargo-zigbuild against glibc 2.17). Without
//! them the client still works and runs the Python agent on every server.

use std::io::Write;
use std::path::PathBuf;

use sha2::{Digest, Sha256};

fn main() {
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let mut entries = String::from("pub const EMBEDDED: &[(&str, &[u8], &str)] = &[\n");
    for (arch, var) in [
        ("x86_64", "GPUWHO_AGENT_X86_64"),
        ("aarch64", "GPUWHO_AGENT_AARCH64"),
    ] {
        println!("cargo:rerun-if-env-changed={}", var);
        let Ok(path) = std::env::var(var) else {
            continue;
        };
        if path.is_empty() {
            continue;
        }
        println!("cargo:rerun-if-changed={}", path);
        let bin = std::fs::read(&path).unwrap_or_else(|e| panic!("{}={}: {}", var, path, e));
        let hash = Sha256::digest(&bin);
        let tag: String = hash.iter().take(6).map(|b| format!("{:02x}", b)).collect();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        gz.write_all(&bin).unwrap();
        let file = out.join(format!("agent-{}.gz", arch));
        std::fs::write(&file, gz.finish().unwrap()).unwrap();
        entries.push_str(&format!(
            "    ({:?}, include_bytes!({:?}), {:?}),\n",
            arch,
            file.display().to_string(),
            tag
        ));
    }
    entries.push_str("];\n");
    std::fs::write(out.join("agents.rs"), entries).unwrap();
}
