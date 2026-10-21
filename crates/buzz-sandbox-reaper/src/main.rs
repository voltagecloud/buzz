//! `buzz-sandbox-reaper`: the in-cluster sandbox reaper for Buzz Agent
//! Sandbox sessions (`docs/remote-agents.md` §Sandbox lifecycle).
#![expect(
    dead_code,
    reason = "stack glue: the modules are exercised by their tests until the reconcile subcommand wires them up (S3Store::new has no test caller)"
)]

mod config;
mod evidence;
mod plan;
mod reconcile;
mod server;

fn main() {}
