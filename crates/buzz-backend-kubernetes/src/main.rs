//! Kubernetes backend provider for Buzz remote agents
//! (spec `docs/remote-agents.md`).
//!
//! One process per operation: read exactly one JSON request from stdin, write
//! exactly one JSON response to stdout, exit. The exit code carries exactly
//! one bit — 0 for a response that was produced, 1 for a failure to produce
//! one. Everything a caller needs to distinguish is *inside* the response's
//! `ok` field, because a provider that encoded outcomes in exit codes would
//! have a second, redundant error channel to keep in sync (§Provider Protocol).

use buzz_backend_kubernetes::provider::{check_identity, respond};
use buzz_backend_kubernetes::wire::Response;
use std::io::Read;

fn main() {
    // rustls needs a process-level provider before the first TLS connection.
    // The release build compiles every sidecar in one cargo invocation, which
    // unifies the `ring` and `aws-lc-rs` features and leaves rustls unable to
    // auto-select — so this is an explicit install, not a default.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut input = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut input) {
        // No request means no request_id and no response contract to honor.
        // This is the one path that exits nonzero.
        eprintln!("could not read the request from stdin: {e}");
        std::process::exit(1);
    }

    let response = match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [] => respond(&input),
        [flag] if flag == "--check-identity" => check_identity(&input),
        _ => Response::error("unsupported provider arguments"),
    };
    println!(
        "{}",
        serde_json::to_string(&response).unwrap_or_else(|e| {
            // The response types are plain data; this cannot fail in practice,
            // and a hand-built object is still a conforming response.
            format!(r#"{{"ok":false,"error":"could not serialize a response: {e}"}}"#)
        })
    );
}
