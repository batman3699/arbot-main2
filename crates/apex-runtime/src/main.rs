//! `apex` — the control-plane binary.
//!
//! # `apex shadow`
//!
//! The Task 8.5 shadow run: the whole capture path over Base with the null
//! dispatcher as its only lane — see [`apex_runtime::shadow`]. Configured by
//! `--config` (default `ops/shadow.base.yaml`); `scripts/shadow.sh` starts it
//! with the operator's keys mapped onto the `APEX_SECRET_*` names it reads.
//!
//! # What the bare binary does, stated plainly
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
const DEFAULT_SHADOW_CONFIG: &str = "ops/shadow.base.yaml";
/// R28: the live canary's configuration.
const DEFAULT_CANARY_CONFIG: &str = "ops/canary.base.yaml";

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    if std::env::args().nth(1).as_deref() == Some("armed-eval") {
        return armed_eval().await;
    }
    if std::env::args().nth(1).as_deref() == Some("armed-replay") {
        return armed_replay().await;
    }
    if std::env::args().nth(1).as_deref() == Some("probe") {
        return probe().await;
    }
    if std::env::args().nth(1).as_deref() == Some("send-probe") {
        return send_probe().await;
    }
    if std::env::args().nth(1).as_deref() == Some("validity-probe") {
        return validity_probe().await;
    }
    if std::env::args().nth(1).as_deref() == Some("armed-moves-seed") {
        return armed_moves_seed().await;
    }
    if std::env::args().nth(1).as_deref() == Some("shadow") {
        return shadow().await;
    }
    if std::env::args().nth(1).as_deref() == Some("canary") {
        return canary().await;
    }
    if std::env::args().nth(1).as_deref() == Some("keys") {
        return keys();
    }

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

/// `apex keys new --count N --out PATH`: N armed senders' keys (R36), in a new
/// file written 0400. Prints their addresses, one a line, and nothing else:
/// no key leaves the file.
fn keys() -> ExitCode {
    const USAGE: &str = "usage: apex keys new --count N --out PATH (N from 1 to 256; PATH must not exist)";
    let args: Vec<String> = std::env::args().skip(2).collect();
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let count = flag("--count").and_then(|c| c.parse::<usize>().ok()).filter(|c| (1..=256).contains(c));
    let (Some("new"), Some(count), Some(out)) = (args.first().map(String::as_str), count, flag("--out")) else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };
    match apex_runtime::armed::keys::generate(count, std::path::Path::new(&out)) {
        Ok(addresses) => {
            for a in addresses {
                println!("{a}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("apex keys: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `apex armed-replay [--config PATH] DIR...`: each fire in `DIR` quoted on
/// its own book at other sizes (R36, lever 1); one JSON line a fire. Reads the
/// chain only.
async fn armed_replay() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(2).collect();
    let config = if args.first().map(String::as_str) == Some("--config") && args.len() > 1 {
        args.remove(0);
        args.remove(0)
    } else {
        DEFAULT_SHADOW_CONFIG.to_string()
    };
    if args.is_empty() || args.iter().any(|a| a.starts_with("--")) {
        error!("usage: apex armed-replay [--config PATH] DIR...");
        return ExitCode::FAILURE;
    }
    let env = Env::secrets_only();
    let loaded = match apex_runtime::shadow::config::ShadowConfig::load(&config, &env) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "armed-replay: the config did not load");
            return ExitCode::FAILURE;
        }
    };
    let dirs: Vec<std::path::PathBuf> = args.into_iter().map(Into::into).collect();
    match apex_runtime::armed::replay::replay(&loaded, &dirs).await {
        Ok(fires) => {
            for f in fires {
                println!("{f}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!(error = %e, "armed-replay failed");
            ExitCode::FAILURE
        }
    }
}

/// `apex armed-eval [--config PATH]`: judge the arms closed since the last run
/// (R26). `apex armed-eval --summary [--dir DIR]`: the bid curve so far,
/// from the verdict files alone, with no config and no secret.
async fn armed_eval() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(2).collect();
    if args.first().map(String::as_str) == Some("--summary") {
        let dir = match args.as_slice() {
            [_] => "var/apex".to_string(),
            [_, flag, dir] if flag == "--dir" => dir.clone(),
            _ => {
                error!("usage: apex armed-eval --summary [--dir DIR]");
                return ExitCode::FAILURE;
            }
        };
        return match apex_runtime::armed::run::summary(std::path::Path::new(&dir)) {
            Ok(s) => match serde_json::to_string_pretty(&s) {
                Ok(text) => {
                    println!("{text}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    error!(error = %e, "the summary did not serialize");
                    ExitCode::FAILURE
                }
            },
            Err(e) => {
                error!(error = %e, dir, "armed-eval summary failed");
                ExitCode::FAILURE
            }
        };
    }
    let config = match args.as_slice() {
        [] => DEFAULT_SHADOW_CONFIG.to_string(),
        [flag, path] if flag == "--config" => path.clone(),
        _ => {
            error!("usage: apex armed-eval [--config PATH] | apex armed-eval --summary [--dir DIR]");
            return ExitCode::FAILURE;
        }
    };
    let env = Env::secrets_only();
    let loaded = match apex_runtime::shadow::config::ShadowConfig::load(&config, &env) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "armed-eval: the config did not load");
            return ExitCode::FAILURE;
        }
    };
    match apex_runtime::armed::run::run(&loaded).await {
        Ok(r) => match serde_json::to_string(&r) {
            Ok(text) => {
                println!("{text}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                error!(error = %e, "the run report did not serialize");
                ExitCode::FAILURE
            }
        },
        Err(e) => {
            error!(error = %e, "armed-eval failed");
            ExitCode::FAILURE
        }
    }
}

/// `apex probe [--minutes N]`: the region probe (Task 8.5 R25). Reads only
/// `APEX_SECRET_BLOCKPI_KEY`, and runs without it (no BlockPI figures).
async fn probe() -> ExitCode {
    let mut minutes = 60u64;
    let mut argv = std::env::args().skip(2);
    while let Some(flag) = argv.next() {
        match (flag.as_str(), argv.next().and_then(|v| v.parse::<u64>().ok())) {
            ("--minutes", Some(m)) if m > 0 => minutes = m,
            _ => {
                error!("usage: apex probe [--minutes N]");
                return ExitCode::FAILURE;
            }
        }
    }
    let report = apex_runtime::probe::run(minutes, &Env::secrets_only()).await;
    match serde_json::to_string_pretty(&report) {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!(error = %e, "the probe's report did not serialize");
            ExitCode::FAILURE
        }
    }
}

/// `apex send-probe [--config <path>] [--count N]` (Task 8.5 R32): N zero-value
/// self-transfers from the canary's trader, alternating BlockPI's keyed
/// endpoint and Base's sequencer, each timed to the flashblock that carried it.
/// Stop the canary first: the trader's nonce moves.
async fn send_probe() -> ExitCode {
    use apex_config::Secret;
    use apex_runtime::send_probe::{run, SEQUENCER};
    let (mut config, mut count) = ("ops/canary.base.yaml".to_string(), 10usize);
    let mut argv = std::env::args().skip(2);
    while let Some(flag) = argv.next() {
        match (flag.as_str(), argv.next()) {
            ("--config", Some(p)) => config = p,
            ("--count", Some(n)) => match n.parse::<usize>() {
                // A probe costs ~0.00000013 ETH; twenty is enough to compare.
                Ok(n) if (1..=20).contains(&n) => count = n,
                _ => {
                    error!("--count is 1 to 20");
                    return ExitCode::FAILURE;
                }
            },
            _ => {
                error!("usage: apex send-probe [--config ops/canary.base.yaml] [--count N]");
                return ExitCode::FAILURE;
            }
        }
    }
    let env = Env::secrets_only();
    let c = match apex_runtime::shadow::config::ShadowConfig::load_canary(&config, &env) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "the configuration did not load");
            return ExitCode::FAILURE;
        }
    };
    let Some(key) = env.get(&c.signer.key_env).and_then(|k| apex_runtime::sign::LaneKey::from_hex(&Secret::new(k)).ok()) else {
        error!(key_env = %c.signer.key_env, "the trader's key is not set or not a key");
        return ExitCode::FAILURE;
    };
    let reads = c.rpc_http[0].expose().to_string();
    let routes = [("blockpi", reads.clone()), ("sequencer", SEQUENCER.to_string())];
    let report = run(&key, c.chain_id, &reads, &routes, count, 1_000_000).await;
    match serde_json::to_string_pretty(&report) {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!(error = %e, "the report did not serialize");
            ExitCode::FAILURE
        }
    }
}

/// `apex validity-probe [--config <path>] --sender I` (Task 8.5 R36): six
/// cases of `base_sendRawTransactionValidity` from armed sender `I`, each
/// printed as a line once the chain has settled it. The armed lane stays in
/// shadow: the sender's nonce moves.
async fn validity_probe() -> ExitCode {
    use apex_runtime::validity_probe::Probe;
    let usage = "usage: apex validity-probe [--config ops/canary.base.yaml] --sender I";
    let (mut config, mut sender) = (DEFAULT_CANARY_CONFIG.to_string(), None);
    let mut argv = std::env::args().skip(2);
    while let Some(flag) = argv.next() {
        match (flag.as_str(), argv.next()) {
            ("--config", Some(p)) => config = p,
            ("--sender", Some(i)) => match i.parse::<usize>() {
                Ok(i) => sender = Some(i),
                Err(_) => {
                    error!("--sender is a key's index in armed.live.keys_file");
                    return ExitCode::FAILURE;
                }
            },
            _ => {
                error!("{usage}");
                return ExitCode::FAILURE;
            }
        }
    }
    let Some(sender) = sender else {
        error!("{usage}");
        return ExitCode::FAILURE;
    };
    let env = Env::secrets_only();
    let c = match apex_runtime::shadow::config::ShadowConfig::load_canary(&config, &env) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "the configuration did not load");
            return ExitCode::FAILURE;
        }
    };
    if c.armed.mode != apex_runtime::shadow::config::ArmedMode::Shadow {
        error!("armed.mode is live: the probe moves a sender's nonce, so it runs with the armed lane in shadow");
        return ExitCode::FAILURE;
    }
    let Some(live) = &c.armed.live else {
        error!("the configuration has no armed.live: its keys file and sequencer");
        return ExitCode::FAILURE;
    };
    let keys = match apex_runtime::armed::keys::load(&live.keys_file) {
        Ok(k) => k,
        Err(e) => {
            error!(error = %e, "the armed keys did not load");
            return ExitCode::FAILURE;
        }
    };
    let Some(key) = keys.get(sender) else {
        error!(keys = keys.len(), "--sender {sender} is not in the keys file");
        return ExitCode::FAILURE;
    };
    match apex_runtime::validity_probe::ledger_knows(&live.ledger, key.address()) {
        Ok(false) => {}
        Ok(true) => {
            error!(sender = %key.address(), "armed-live's ledger knows this sender: the probe would move its nonce from where the ledger left it, and the next boot would trip. Pick one it has never used");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            error!(error = %e, "armed-live's ledger did not read");
            return ExitCode::FAILURE;
        }
    }
    let urls: Vec<String> = c.rpc_http.iter().map(|u| u.expose().clone()).collect();
    let rpc = match apex_chain::rpc::failover::FailoverTransport::connect(&urls, c.chain_id, Default::default()).await {
        Ok(t) => std::sync::Arc::new(t),
        Err(e) => {
            error!(error = %e, "the RPC transport did not connect");
            return ExitCode::FAILURE;
        }
    };
    drop(urls);
    let submit = match apex_chain::base::validity::ValiditySubmitter::new(&live.sequencer, std::time::Duration::from_secs(3)) {
        Ok(s) => s,
        Err(e) => {
            error!(error = %e, "the validity submitter did not start");
            return ExitCode::FAILURE;
        }
    };
    let chain = apex_runtime::armed::live_chain::RpcChain::new(rpc.clone());
    info!(sender = %key.address(), endpoint = submit.label(), "the validity probe starts");
    let probe = Probe { key, chain_id: c.chain_id, rpc: &*rpc, chain: &chain, submit: &submit };
    let all = probe
        .run(|a| match serde_json::to_string(a) {
            Ok(line) => println!("{line}"),
            Err(e) => error!(error = %e, "a case did not serialize"),
        })
        .await;
    if all {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// `apex armed-moves-seed [--config <path>] --rpc <url> [--hours N] [--force]`
/// (Task 8.5 R36): the ranking's move statistics seeded from the last `N`
/// hours (24) of the canary's pools' own logs, read through `--rpc` (a public
/// endpoint: not the run's keyed one), written to `armed.moves` for the next
/// boot. Refuses to replace a saved file without `--force`.
async fn armed_moves_seed() -> ExitCode {
    use apex_runtime::armed::moves::seed_from_logs;
    let usage = "usage: apex armed-moves-seed [--config ops/canary.base.yaml] --rpc <url> [--hours 24] [--force]";
    let (mut config, mut rpc_url, mut hours, mut force) = (DEFAULT_CANARY_CONFIG.to_string(), None, 24u64, false);
    let mut argv = std::env::args().skip(2);
    while let Some(flag) = argv.next() {
        match flag.as_str() {
            "--force" => force = true,
            "--config" | "--rpc" | "--hours" => {
                let Some(v) = argv.next() else {
                    error!("{usage}");
                    return ExitCode::FAILURE;
                };
                match flag.as_str() {
                    "--config" => config = v,
                    "--rpc" => rpc_url = Some(v),
                    _ => match v.parse::<u64>() {
                        Ok(h) if (1..=24).contains(&h) => hours = h,
                        _ => {
                            error!("--hours is 1 to 24: the statistics keep a day");
                            return ExitCode::FAILURE;
                        }
                    },
                }
            }
            _ => {
                error!("{usage}");
                return ExitCode::FAILURE;
            }
        }
    }
    let Some(rpc_url) = rpc_url else {
        error!("{usage}");
        return ExitCode::FAILURE;
    };
    let env = Env::secrets_only();
    let c = match apex_runtime::shadow::config::ShadowConfig::load_canary(&config, &env) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "the configuration did not load");
            return ExitCode::FAILURE;
        }
    };
    let Some(path) = c.armed.moves.clone() else {
        error!("the configuration has no armed.moves to write");
        return ExitCode::FAILURE;
    };
    if path.exists() && !force {
        error!(path = %path.display(), "saved move statistics exist: --force replaces them");
        return ExitCode::FAILURE;
    }
    // The canary's pools: its universe, with the census's targets below the floor.
    let now_unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let targets = match apex_runtime::shadow::load_targets(&c.armed, now_unix) {
        Ok(t) => t,
        Err(e) => {
            error!(error = %e, "the census's targets did not load");
            return ExitCode::FAILURE;
        }
    };
    let include = targets.as_ref().map(apex_runtime::armed::targets::Targets::pools).unwrap_or_default();
    // Each targeted route's spread, either pool its trigger: what a fire moves.
    let pairs: Vec<(alloy_primitives::Address, alloy_primitives::Address)> =
        targets.iter().flat_map(|t| t.routes.iter()).flat_map(|r| [(r.pools[0], r.pools[1]), (r.pools[1], r.pools[0])]).collect();
    let venues = apex_runtime::live::inventory::Venue::ALL.to_vec();
    let hooks = match c.universe.hooks() {
        Ok(h) => h,
        Err(e) => {
            error!(error = %e, "the v4 hook allowlist did not load");
            return ExitCode::FAILURE;
        }
    };
    let pools: Vec<alloy_primitives::Address> = match apex_runtime::shadow::universe(&c.inventory, &venues, c.universe.filter(), &include, &hooks) {
        Ok(specs) => specs.iter().map(|s| s.pool).collect(),
        Err(e) => {
            error!(error = %e, "the inventory did not load");
            return ExitCode::FAILURE;
        }
    };
    let rpc = match apex_chain::rpc::failover::FailoverTransport::connect(std::slice::from_ref(&rpc_url), c.chain_id, Default::default()).await {
        Ok(t) => std::sync::Arc::new(t),
        Err(e) => {
            error!(error = %e, "the RPC transport did not connect");
            return ExitCode::FAILURE;
        }
    };
    let head = {
        use apex_runtime::armed::live::LiveChain;
        match apex_runtime::armed::live_chain::RpcChain::new(rpc.clone()).head().await {
            Ok(h) => h,
            Err(e) => {
                error!(error = %e, "no head");
                return ExitCode::FAILURE;
            }
        }
    };
    let a = &c.armed;
    let blocks = hours * 3_600_000 / a.block_time_ms.max(1);
    let from = head.saturating_sub(blocks);
    info!(pools = pools.len(), spreads = pairs.len(), from, to = head, "seeding the move statistics from the pools' logs");
    let step_ns = a.block_time_ms.saturating_mul(1_000_000);
    let (moves, read) = match seed_from_logs(&*rpc, &pools, &pairs, from, head, 2_000, 10, a.window_s * 1_000_000_000, step_ns).await {
        Ok(r) => r,
        Err(e) => {
            error!(error = %e, "the seed did not read");
            return ExitCode::FAILURE;
        }
    };
    let snap = moves.snapshot(read.until_ns);
    let mut windows: Vec<usize> = snap.pools.values().map(Vec::len).collect();
    windows.sort_unstable();
    if let Err(e) = snap.save(&path) {
        error!(error = %e, path = %path.display(), "the seed was not saved");
        return ExitCode::FAILURE;
    }
    println!(
        "{}",
        serde_json::json!({
            "path": path.display().to_string(), "blocks": [from, head], "logs": read.logs, "observations": read.observations,
            "pools_asked": pools.len(), "spreads": pairs.len(), "series_seeded": snap.pools.len(),
            "windows_per_pool_median": windows.get(windows.len() / 2), "windows_per_pool_max": windows.last(),
        })
    );
    ExitCode::SUCCESS
}

/// `apex shadow [--config <path>]`. Runs until SIGINT or SIGTERM.
async fn shadow() -> ExitCode {
    let mut config = DEFAULT_SHADOW_CONFIG.to_string();
    let mut argv = std::env::args().skip(2);
    while let Some(flag) = argv.next() {
        match flag.as_str() {
            "--config" => match argv.next() {
                Some(path) => config = path,
                None => {
                    error!("--config needs a path");
                    return ExitCode::FAILURE;
                }
            },
            other => {
                error!("usage: apex shadow [--config {DEFAULT_SHADOW_CONFIG}]; unknown argument `{other}`");
                return ExitCode::FAILURE;
            }
        }
    }
    // `secrets_only`: the endpoints' key and the signer's are the only values
    // read from the environment, and only under their `APEX_SECRET_*` names.
    let env = Env::secrets_only();
    let loaded = match apex_runtime::shadow::config::ShadowConfig::load(&config, &env) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "apex shadow refused to start");
            return ExitCode::FAILURE;
        }
    };
    match apex_runtime::shadow::run(loaded, &env, wait_for_signal()).await {
        Ok(report) => {
            info!(
                events = report.funnel.events,
                null_dispatched = report.null_dispatched,
                capture_assurance = ?report.capture_assurance,
                "apex shadow stopped"
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!(error = %e, "apex shadow refused to start");
            ExitCode::FAILURE
        }
    }
}

/// `apex canary [--config ops/canary.base.yaml]` (R28): the shadow's process with
/// its reactive lane sending, under the canary's limits.
async fn canary() -> ExitCode {
    let mut config = DEFAULT_CANARY_CONFIG.to_string();
    let mut argv = std::env::args().skip(2);
    while let Some(flag) = argv.next() {
        match flag.as_str() {
            "--config" => match argv.next() {
                Some(path) => config = path,
                None => {
                    error!("--config needs a path");
                    return ExitCode::FAILURE;
                }
            },
            other => {
                error!("usage: apex canary [--config {DEFAULT_CANARY_CONFIG}]; unknown argument `{other}`");
                return ExitCode::FAILURE;
            }
        }
    }
    // `secrets_only`, as the shadow: the endpoints' key and the signer's are
    // the only values read from the environment.
    let env = Env::secrets_only();
    let loaded = match apex_runtime::shadow::config::ShadowConfig::load_canary(&config, &env) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "apex canary refused to start");
            return ExitCode::FAILURE;
        }
    };
    match apex_runtime::shadow::run(loaded, &env, wait_for_signal()).await {
        Ok(report) => {
            info!(
                events = report.funnel.events,
                canary = ?report.canary.as_ref().map(|c| &c.totals),
                "apex canary stopped"
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!(error = %e, "apex canary refused to start");
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
