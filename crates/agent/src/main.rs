//! gpuwho-agent: started once per SSH connection as `gpuwho-agent IDLE_SEC`.
//!
//! Protocol: the client writes one line per poll and the agent answers with one JSON line,
//! the same document the Python fallback agent (probe.py) produces. It exits on stdin EOF
//! (client gone), after IDLE_SEC without a request, or after six hours.

mod memcg;
mod nvml;
mod procfs;
mod sampler;

use std::io::{Read, Write};
use std::time::{Duration, Instant};

const MAX_LIFETIME: Duration = Duration::from_secs(6 * 3600);

/// Wait up to `secs` for stdin to become readable.
fn readable(secs: f64) -> bool {
    let mut fds = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = (secs * 1000.0).min(i32::MAX as f64) as libc::c_int;
    loop {
        let r = unsafe { libc::poll(&mut fds, 1, ms) };
        if r < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return r > 0;
    }
}

fn main() {
    let idle: f64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(90.0);
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("gpuwho-agent {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let mut sampler = sampler::Sampler::new();
    let started = Instant::now();
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let mut buf = [0u8; 4096];
    // tell the client we're up (it may have uploaded us a moment ago)
    if writeln!(stdout, "{{\"ready\": true}}")
        .and_then(|_| stdout.flush())
        .is_err()
    {
        return;
    }
    while started.elapsed() < MAX_LIFETIME {
        if !readable(idle) {
            break; // client went quiet (e.g. network dropped): don't linger
        }
        match stdin.read(&mut buf) {
            Ok(0) | Err(_) => break, // EOF: client closed the connection
            Ok(_) => {}
        }
        let out = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sampler.sample()))
        {
            Ok(v) => v,
            Err(e) => {
                let msg = e
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| e.downcast_ref::<String>().cloned());
                serde_json::json!({"fatal": format!("agent panicked: {}", msg.unwrap_or_default())})
            }
        };
        if writeln!(stdout, "{}", out)
            .and_then(|_| stdout.flush())
            .is_err()
        {
            break;
        }
    }
}
