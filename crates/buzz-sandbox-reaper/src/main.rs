//! `buzz-sandbox-reaper`: the in-cluster sandbox reaper for Buzz Agent
//! Sandbox sessions (`docs/remote-agents.md` §Sandbox lifecycle).
#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "stack glue: the modules are exercised by their tests until the reconcile subcommand wires them up"
    )
)]

mod config;
mod plan;

fn main() {}
