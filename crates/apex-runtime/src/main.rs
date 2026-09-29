//! `apex` — the control-plane binary.
//!
//! # What this binary does today, stated plainly
//!
//! It **boots and reports its own readiness**. It does not trade, and it does not
//! pretend to: three of [`apex_runtime::plane`]'s eight ports are implemented by
//! crates that do not exist yet — `apex-search` (Phase 2/12), `apex-exec`
//! (Phase 5) — so there is no candidate source to drive and no payload builder to
//! sign with. A `main` that started a loop over stub ports would produce a
//! process that looks alive and captures nothing, which is §6.5's "written but
//! never wired" pattern with a PID.
//!
//! So it does the four things that are real, in the order §46.1 requires:
//!
//! 1. Load and validate configuration, failing closed (`apex-config`).
//! 2. Replay the ticket journal and report what boot reconciliation would owe the
//!    chain — **without** opening the dispatch gate, because opening it requires
//!    resolving those tickets and resolving them requires a chain adapter this
//!    binary is not yet wired to.
//! 3. Bring up the event bus and a supervised worker, so supervision and the
//!    two-lane feed are exercised by the real binary rather than only by tests.
//! 4. Wait for a signal, then drain: stop admitting, stop the workers, and hand
//!    anything that may be on chain to the next boot.
//!
//! Its exit status says which. A reader who runs it learns what is wired and what
//! is not, which is the most useful thing a control plane can say before its
//! search exists.
//!
//! # Two paths, and they are arguments rather than environment variables
//!
//! `apex-config`'s `env_coverage` gate caught the first draft: it read
//! `APEX_INPUTS` and `APEX_JOURNAL` from the environment, and every environment
//! read in this workspace must have a registered migration destination. The gate
//! was right, and registering them would have been the wrong repair — that table
//! is `LEGACY_ENV_VARS`, a list of 84 variables being *retired*, and adding two
//! new arrivals to it is a category error.
//!
//! §2.4's complaint is **late configuration lookup**. A bootstrap path cannot come
//! from the file it points at, so it has to come from outside the process; a
//! command-line argument is the explicit form of that — visible in the process
//! table, supplied by whoever starts the service, impossible to inherit by
//! accident from an operator's shell. The journal path belongs in
//! `ops/inputs.yaml` and will move there when the schema next changes; until then
//! it is an argument with a default rather than a new environment variable.
//!
//! # Nothing here prints a credential
//!
//! INV-46. The config reports its version and the *number* of environment keys it
//! consulted, never a key's value. `apex-config`'s `Secret<T>` redacts in
//! `Debug`, `Display` and `Serialize`, and `scripts/ci/secret_scan.sh` holds the
//! line; this binary does not rely on either, and simply never reaches for one.

use apex_capture::journal::FileJournal;
use apex_capture::recover::scan;
use apex_config::{ApexConfig, Env};
use apex_runtime::bus::{EventBus, Lane};
use apex_runtime::shutdown::Shutdown;
use apex_runtime::supervise::spawn_supervised;
use std::process::ExitCode;
use std::sync::Arc;
use tracing::{error, info, warn};

/// Where the journal lives. One file, appended to, replayed once at boot (§17.5).
const DEFAULT_JOURNAL: &str = "var/apex/tickets.journal";
const DEFAULT_INPUTS: &str = "ops/inputs.yaml";

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match run().await {
        Ok(report) => {
            info!(
                config_version = %report.config_version,
                env_keys_consulted = report.env_keys_consulted,
                chains = report.chains,
                unreconciled = report.unreconciled,
                "apex booted"
            );
            if report.unreconciled > 0 {
                warn!(
                    unreconciled = report.unreconciled,
                    "INV-39: the dispatch gate stays shut until these are resolved against the chain"
                );
            }
            info!("ports not yet implemented: CandidateSource, Commitments, Signer");
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!(error = %e, "apex refused to start");
            ExitCode::FAILURE
        }
    }
}

struct BootReport {
    config_version: String,
    env_keys_consulted: usize,
    chains: usize,
    unreconciled: usize,
}

/// `--inputs <path>` and `--journal <path>`. Hand-parsed rather than pulling in an
/// argument crate for two flags, and it refuses an unknown one rather than
/// ignoring it: a typo in a path argument that silently used the default is how a
/// service comes up against the wrong configuration and looks healthy.
struct Args {
    inputs: String,
    journal: String,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        inputs: DEFAULT_INPUTS.to_string(),
        journal: DEFAULT_JOURNAL.to_string(),
    };
    let mut argv = std::env::args().skip(1);
    while let Some(flag) = argv.next() {
        match flag.as_str() {
            "--inputs" => {
                args.inputs = argv.next().ok_or("--inputs needs a path")?;
            }
            "--journal" => {
                args.journal = argv.next().ok_or("--journal needs a path")?;
            }
            "--help" | "-h" => {
                return Err(format!(
                    "usage: apex [--inputs {DEFAULT_INPUTS}] [--journal {DEFAULT_JOURNAL}]"
                ));
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(args)
}

async fn run() -> Result<BootReport, Box<dyn std::error::Error>> {
    let args = parse_args()?;

    // ---- 1. Configuration, fail-closed.
    let inputs = args.inputs;
    // `secrets_only`, the production constructor: only `APEX_SECRET_*` may be
    // read. `permissive()` exists for the migration window and using it here
    // would let a legacy `${VAR}` placeholder resolve from an operator's shell
    // and look configured.
    let config = ApexConfig::load_from(&inputs, &Env::secrets_only())?;

    // ---- 2. The journal. Replayed, reported, and NOT reconciled: reconciling
    // needs a chain adapter, and a gate opened without one would be the exact
    // "we forgot to run recovery" state INV-39 exists to make impossible.
    let journal_path = args.journal;
    if let Some(parent) = std::path::Path::new(&journal_path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let journal = FileJournal::open(&journal_path)?;
    let found = scan(&journal)?;
    for disposition in &found.in_flight {
        warn!(
            ticket = disposition.id().0,
            status = ?disposition.ticket().status,
            needs_the_chain = disposition.needs_the_chain(),
            "outstanding from a previous run"
        );
    }

    // ---- 3. The feed and one supervised worker.
    let shutdown = Arc::new(Shutdown::new());
    let mut bus = EventBus::with_capacity(1_024);
    let capture_feed = bus.subscribe("capture", Lane::Fast);
    let research_feed = bus.subscribe("coverage", Lane::Slow);
    let bus = Arc::new(bus);

    // A supervised worker is restarted by calling its factory again, so the
    // factory is `FnMut` and cannot move a `Subscription` out of itself. That
    // constraint is not an inconvenience to route around -- it is the statement
    // that **a worker owning a resource it cannot recreate cannot be restarted**,
    // and a subscription is exactly such a resource: re-subscribing would need
    // `&mut` on a bus that is already shared.
    //
    // So the subscription is shared and the worker borrows it per attempt. The
    // queue keeps filling while the worker is down, up to its capacity and then
    // as counted drops -- which is how the loss counter ends up telling you a
    // worker was restarting.
    let capture_feed = Arc::new(tokio::sync::Mutex::new(capture_feed));
    let research_feed = Arc::new(tokio::sync::Mutex::new(research_feed));

    let mut workers = vec![
        {
            let signal = shutdown.subscribe();
            let feed = Arc::clone(&capture_feed);
            spawn_supervised("capture-feed", "base".to_string(), signal, move || {
                // Draining the subscription is the worker's whole job until a
                // candidate source exists. It is here so the fast lane is
                // actually consumed: an unconsumed fast lane would report loss
                // (`fast_lane_is_lossless() == false`) the moment any event were
                // published, which would be a true statement about a system
                // nobody had finished wiring.
                let feed = Arc::clone(&feed);
                async move {
                    let mut feed = feed.lock().await;
                    while feed.recv().await.is_some() {}
                }
            })
        },
        {
            let signal = shutdown.subscribe();
            let feed = Arc::clone(&research_feed);
            spawn_supervised("coverage-feed", "base".to_string(), signal, move || {
                let feed = Arc::clone(&feed);
                async move {
                    let mut feed = feed.lock().await;
                    while feed.recv().await.is_some() {}
                }
            })
        },
    ];

    // ---- 4. Wait for a signal, then drain.
    wait_for_signal().await;
    let stopped = shutdown.stop_workers(&mut workers).await;
    info!(
        workers = stopped.workers_stopped(),
        published = bus.published(),
        fast_lane_lossless = bus.fast_lane_is_lossless(),
        "feed stopped"
    );

    Ok(BootReport {
        config_version: config.config_version().to_string(),
        env_keys_consulted: config.consulted_env_keys().len(),
        chains: config.chains().len(),
        unreconciled: found.unreconciled(),
    })
}

/// SIGINT or SIGTERM. A control plane that only handled Ctrl-C would be killed by
/// its supervisor with no drain at all, and §46.1's reconciliation is the thing
/// that would be skipped.
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "no SIGTERM handler; falling back to Ctrl-C only");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => info!("SIGINT"),
            _ = term.recv() => info!("SIGTERM"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
