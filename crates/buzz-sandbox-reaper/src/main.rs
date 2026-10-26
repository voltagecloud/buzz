//! `buzz-sandbox-reaper`: the in-cluster sandbox reaper for Buzz Agent
//! Sandbox sessions (`docs/remote-agents.md` §Sandbox lifecycle).
//!
//! `reconcile` classifies every managed Sandbox in the configured developer
//! namespaces, tombstones sessions that have ended, and deletes tombstones
//! once their 30-day checkpoint retention has passed. It never starts or
//! resumes a session; only an explicit recovery does that.
//!
//! `serve` is the session manager (§Relay transport): it runs developers'
//! provider requests, received over the Buzz relay, in their namespaces.

mod config;
mod enroll;
mod evidence;
mod manager;
mod mobile_launch;
mod plan;
mod profile;
mod reconcile;
mod serve;
mod server;

use std::collections::BTreeMap;
use std::future::{Future, IntoFuture};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use metrics_exporter_prometheus::PrometheusHandle;
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::config::{Config, Namespace};
use crate::evidence::{CheckpointStore, S3Store};
use crate::reconcile::{run_namespace, Memory, Reconciler};
use crate::server::Health;

/// How long in-flight passes get to finish after SIGTERM before the process
/// exits anyway (inside the default 30s Pod termination grace). A pass can
/// run for up to `reconcile::PASS_DEADLINE`, so a slow one is abandoned; its
/// writes are single requests, so the next process re-decides from scratch.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(20);

#[derive(Parser)]
#[command(name = "buzz-sandbox-reaper", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Reconcile Agent Sandbox session lifecycles.
    Reconcile(ReconcileArgs),
    /// Serve Sandbox session requests received over the Buzz relay.
    Serve(serve::ServeArgs),
}

#[derive(Args)]
struct ReconcileArgs {
    /// Path to the JSON config (ConfigMap `sandbox-reaper-config`, key `config.json`).
    #[arg(
        long,
        env = "BUZZ_SANDBOX_REAPER_CONFIG",
        default_value = "/etc/buzz-sandbox-reaper/config.json"
    )]
    config: PathBuf,
    /// Address for /metrics, /healthz, and /readyz (the only listener).
    #[arg(
        long,
        env = "BUZZ_SANDBOX_REAPER_LISTEN",
        default_value = "0.0.0.0:9090"
    )]
    listen: SocketAddr,
    /// Seconds between passes over each namespace.
    #[arg(
        long,
        env = "BUZZ_SANDBOX_REAPER_INTERVAL_SECONDS",
        default_value_t = 30,
        value_parser = clap::value_parser!(u64).range(5..=3600)
    )]
    interval_seconds: u64,
    /// Log decisions without writing anything to the cluster.
    #[arg(long, env = "BUZZ_SANDBOX_REAPER_DRY_RUN")]
    dry_run: bool,
    /// Run one pass per namespace, print a summary, and exit (no listener).
    /// Timers start empty, so the pass cannot outlast the 60s lost grace or
    /// the 24h hold of a Completed session without a checkpoint: such
    /// sessions are reported, not tombstoned.
    #[arg(long)]
    once: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Explicit, as in buzz-backend-kubernetes: the workspace unifies ring and
    // aws-lc-rs, which leaves rustls unable to auto-select a provider.
    let _ = rustls::crypto::ring::default_provider().install_default();
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
                // Its debug logs quote whole relay frames (§Relay transport).
                .add_directive("buzz_ws_client=info".parse().expect("static directive")),
        )
        .init();
    match Cli::parse().command {
        Command::Reconcile(args) => reconcile(args).await,
        Command::Serve(args) => serve::serve(args).await,
    }
}

async fn reconcile(args: ReconcileArgs) -> Result<()> {
    let text = std::fs::read_to_string(&args.config)
        .with_context(|| format!("read config {}", args.config.display()))?;
    let config = Config::parse(&text).map_err(anyhow::Error::msg)?;
    let client = kube::Client::try_default()
        .await
        .context("build in-cluster Kubernetes client")?;
    let store = S3Store::new(&config.checkpoint_bucket, &config.checkpoint_region).await;
    let reconciler = Arc::new(Reconciler {
        client,
        store,
        dry_run: args.dry_run,
    });
    if args.once {
        return once(&reconciler, &config).await;
    }

    let metrics = metrics_exporter_prometheus::PrometheusBuilder::new()
        .install_recorder()
        .context("install metrics recorder")?;
    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("bind {}", args.listen))?;
    tracing::info!(listen = %args.listen, namespaces = config.namespaces.len(),
        dry_run = args.dry_run, "buzz-sandbox-reaper reconcile started");
    serve(
        reconciler,
        config.namespaces,
        Duration::from_secs(args.interval_seconds),
        listener,
        metrics,
        shutdown(),
    )
    .await
}

/// Run one loop per namespace beside the listener until `shutdown`, then
/// drain them (see [`supervise`]).
async fn serve<S: CheckpointStore + 'static>(
    reconciler: Arc<Reconciler<S>>,
    namespaces: BTreeMap<String, Namespace>,
    interval: Duration,
    listener: tokio::net::TcpListener,
    metrics: PrometheusHandle,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let health = Arc::new(Health::new(namespaces.keys().cloned(), interval));
    let cancel = CancellationToken::new();
    let mut loops = JoinSet::new();
    for (name, namespace) in namespaces {
        loops.spawn(run_namespace(
            Arc::clone(&reconciler),
            name,
            namespace,
            interval,
            Arc::clone(&health),
            cancel.clone(),
        ));
    }
    let server = axum::serve(listener, server::router(health, metrics))
        .with_graceful_shutdown(cancel.clone().cancelled_owned());
    let server = tokio::spawn(server.into_future());
    supervise(loops, server, cancel, shutdown).await
}

/// Wait for `shutdown` or for any component to end on its own, which is a
/// failure: the loops run until cancelled and the listener until shutdown.
/// Then cancel everything and give in-flight passes [`SHUTDOWN_GRACE`] to
/// finish before aborting them.
async fn supervise(
    mut loops: JoinSet<()>,
    mut server: JoinHandle<std::io::Result<()>>,
    cancel: CancellationToken,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let outcome = tokio::select! {
        () = shutdown => {
            tracing::info!("shutting down");
            Ok(())
        }
        result = &mut server => match result {
            Ok(Ok(())) => Err(anyhow::anyhow!("metrics listener stopped")),
            Ok(Err(error)) => Err(anyhow::Error::new(error).context("metrics listener failed")),
            Err(error) => Err(anyhow::Error::new(error).context("metrics listener panicked")),
        },
        Some(result) = loops.join_next() => match result {
            Ok(()) => Err(anyhow::anyhow!("a namespace loop stopped")),
            Err(error) => Err(anyhow::Error::new(error).context("a namespace loop panicked")),
        },
    };
    cancel.cancel();
    let drained = tokio::time::timeout(SHUTDOWN_GRACE, async {
        while loops.join_next().await.is_some() {}
        // A JoinHandle must not be polled again once it has completed.
        if !server.is_finished() {
            let _ = (&mut server).await;
        }
    })
    .await;
    if drained.is_err() {
        tracing::warn!("shutdown grace elapsed; abandoning in-flight passes");
        loops.abort_all();
        server.abort();
    }
    outcome
}

async fn once(reconciler: &Reconciler<S3Store>, config: &Config) -> Result<()> {
    let mut failed = false;
    let mut report = BTreeMap::new();
    // No listener serves it: the deadline is what `--once` needs.
    let health = Health::new(config.namespaces.keys().cloned(), Duration::ZERO);
    for (name, namespace) in &config.namespaces {
        let mut memory = Memory::default();
        match reconciler
            .pass_with_deadline(name, namespace, &mut memory, &health)
            .await
        {
            Ok(summary) => {
                report.insert(name.clone(), format!("{summary:?}"));
            }
            Err(error) => {
                failed = true;
                report.insert(name.clone(), format!("error: {error}"));
            }
        }
    }
    println!("{}", serde_json::to_string_pretty(&report)?);
    anyhow::ensure!(!failed, "at least one namespace pass failed");
    Ok(())
}

pub(crate) async fn shutdown() {
    #[cfg(unix)]
    {
        if let Ok(mut term) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::tests::FakeStore;
    use clap::CommandFactory;
    use futures_util::FutureExt;
    use kube::client::Body;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::Instant;

    /// An apiserver that answers every request with HTTP 500 after `delay`,
    /// or never when `delay` is `None`. Counts the requests it answered.
    fn apiserver(delay: Option<Duration>) -> (kube::Client, Arc<AtomicUsize>) {
        let answered = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&answered);
        let service = tower::service_fn(move |_: http::Request<Body>| {
            let count = Arc::clone(&count);
            async move {
                match delay {
                    Some(delay) => tokio::time::sleep(delay).await,
                    None => std::future::pending().await,
                }
                count.fetch_add(1, Ordering::SeqCst);
                http::Response::builder()
                    .status(500)
                    .body(Body::from(Vec::new()))
            }
        });
        (kube::Client::new(service, "default"), answered)
    }

    fn namespaces() -> BTreeMap<String, Namespace> {
        BTreeMap::from([(
            "ns".to_string(),
            Namespace {
                developer: "dev".into(),
                owner_pubkey: "a".repeat(64),
                checkpoint_prefix: "dev/".into(),
            },
        )])
    }

    /// SIGTERM during the first pass: a pass that ends within the grace is
    /// waited for, and a hung one is abandoned when the grace runs out.
    #[tokio::test(start_paused = true)]
    async fn shutdown_drains_an_in_flight_pass_within_the_grace() {
        let sigterm = Duration::from_secs(1);
        let cases = [
            (Some(Duration::from_secs(10)), Duration::from_secs(10), true),
            (None, sigterm + SHUTDOWN_GRACE, false),
        ];
        for (delay, returns_at, answered) in cases {
            let (client, count) = apiserver(delay);
            let reconciler = Arc::new(Reconciler {
                client,
                store: FakeStore::default(),
                dry_run: false,
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let metrics = metrics_exporter_prometheus::PrometheusBuilder::new()
                .build_recorder()
                .handle();
            let started = Instant::now();
            serve(
                reconciler,
                namespaces(),
                Duration::from_secs(30),
                listener,
                metrics,
                tokio::time::sleep(sigterm),
            )
            .await
            .unwrap();
            assert_eq!(started.elapsed(), returns_at, "{delay:?}");
            assert_eq!(count.load(Ordering::SeqCst) > 0, answered, "{delay:?}");
        }
    }

    /// Any component ending before shutdown fails the process, after the
    /// others are drained.
    #[tokio::test(start_paused = true)]
    async fn a_component_that_stops_on_its_own_is_an_error() {
        /// Starts the listener and any extra loops for one case.
        type Start = fn(&CancellationToken, &mut JoinSet<()>) -> JoinHandle<std::io::Result<()>>;
        let cases: [(&str, Start); 5] = [
            ("metrics listener stopped", |_, _| {
                tokio::spawn(async { Ok(()) })
            }),
            ("metrics listener failed", |_, _| {
                tokio::spawn(async { Err(std::io::Error::other("accept")) })
            }),
            ("metrics listener panicked", |_, _| {
                tokio::spawn(async { panic!("listener") })
            }),
            ("a namespace loop stopped", |cancel, loops| {
                loops.spawn(async {});
                tokio::spawn(cancel.clone().cancelled_owned().map(Ok))
            }),
            ("a namespace loop panicked", |cancel, loops| {
                loops.spawn(async { panic!("loop") });
                tokio::spawn(cancel.clone().cancelled_owned().map(Ok))
            }),
        ];
        for (expected, start) in cases {
            let cancel = CancellationToken::new();
            let mut loops = JoinSet::new();
            let drained = Arc::new(AtomicUsize::new(0));
            let healthy = cancel.clone();
            let count = Arc::clone(&drained);
            loops.spawn(async move {
                healthy.cancelled().await;
                count.fetch_add(1, Ordering::SeqCst);
            });
            let server = start(&cancel, &mut loops);
            let started = Instant::now();
            let error = supervise(loops, server, cancel, std::future::pending())
                .await
                .unwrap_err();
            assert_eq!(error.to_string(), expected);
            assert_eq!(drained.load(Ordering::SeqCst), 1, "{expected}");
            assert!(started.elapsed() < SHUTDOWN_GRACE, "{expected}");
        }
    }

    /// Components that ignore cancellation are aborted once the grace ends.
    #[tokio::test(start_paused = true)]
    async fn components_that_ignore_shutdown_are_aborted_after_the_grace() {
        let (loop_alive, loop_gone) = tokio::sync::oneshot::channel::<()>();
        let (server_alive, server_gone) = tokio::sync::oneshot::channel::<()>();
        let mut loops = JoinSet::new();
        loops.spawn(async move {
            let _alive = loop_alive;
            std::future::pending::<()>().await;
        });
        let server = tokio::spawn(async move {
            let _alive = server_alive;
            std::future::pending::<std::io::Result<()>>().await
        });
        let started = Instant::now();
        supervise(loops, server, CancellationToken::new(), async {})
            .await
            .unwrap();
        assert_eq!(started.elapsed(), SHUTDOWN_GRACE);
        let gone = Duration::from_secs(1);
        assert!(tokio::time::timeout(gone, loop_gone)
            .await
            .unwrap()
            .is_err());
        assert!(tokio::time::timeout(gone, server_gone)
            .await
            .unwrap()
            .is_err());
    }

    #[test]
    fn cli_is_well_formed_and_defaults_match_the_deployment_contract() {
        Cli::command().debug_assert();
        let cli = Cli::try_parse_from(["buzz-sandbox-reaper", "reconcile"]).unwrap();
        let Command::Reconcile(args) = cli.command else {
            panic!("not reconcile");
        };
        assert_eq!(
            args.config,
            PathBuf::from("/etc/buzz-sandbox-reaper/config.json")
        );
        assert_eq!(args.listen, "0.0.0.0:9090".parse().unwrap());
        assert_eq!(args.interval_seconds, 30);
        assert!(!args.dry_run && !args.once);
        assert!(Cli::try_parse_from([
            "buzz-sandbox-reaper",
            "reconcile",
            "--interval-seconds",
            "1"
        ])
        .is_err());
        assert!(Cli::try_parse_from(["buzz-sandbox-reaper", "api"]).is_err());
        let serve = ["buzz-sandbox-reaper", "serve", "--relay-url", "wss://relay"];
        assert!(matches!(
            Cli::try_parse_from(serve).unwrap().command,
            Command::Serve(_)
        ));
    }
}
