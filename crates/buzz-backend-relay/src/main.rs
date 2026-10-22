//! Relay provider for Buzz remote agents (`docs/remote-agents.md` §Relay
//! transport). One process per operation: one JSON request on stdin, one
//! JSON response on stdout. The exit code is 1 only when no request could
//! be read; every outcome, refusals included, is inside the response.

use std::io::Read;

fn main() {
    // rustls needs a process-level provider before the first TLS connection;
    // the release build unifies ring and aws-lc-rs, so it cannot auto-select.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        eprintln!("could not read the request from stdin");
        std::process::exit(1);
    }
    let response = if std::env::args().len() > 1 {
        serde_json::json!({"ok": false, "error": "unsupported provider arguments"})
    } else {
        buzz_backend_relay::respond(&input)
    };
    println!("{response}");
}
