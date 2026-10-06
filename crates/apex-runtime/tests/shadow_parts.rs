//! Task 8.5 R9 — the shadow run's parts (`apex_runtime::shadow`): its
//! configuration, its funnel, its reload queue and the chain it says nothing
//! was sent to. The assembly over the network is R10's to run.

mod support;

use alloy_primitives::{address, Address, B256};
use apex_capture::dispatch::NullDispatcher;
use apex_capture::recover::{ChainOutcomeSource, DispatchGate};
use apex_capture::registry::TicketRegistry;
use apex_capture::revalidate::LastMileCheck;
use apex_capture::signer::{ExecutorAuth, LaneConfig, SignerPool};
use apex_capture::{InMemoryJournal, ManualClock};
use apex_chain::adapter::RejectReason;
use apex_config::Env;
use apex_econ::eligibility::{Clause, Decision, EligibilityContext, EligibilityGate, EligibilityPolicy};
use apex_runtime::plane::{Decline, DispatchLane, Handled, Plane, Ports, SettlementFeed, Suppressed};
use apex_runtime::shadow::config::{ShadowConfig, ShadowConfigError};
use apex_runtime::shadow::funnel::{decline_label, outcome_label, Funnel};
use apex_runtime::shadow::nothing_sent::{NothingSent, NEVER_SENT};
use apex_runtime::shadow::reload::{ReloadQueue, Work};
use apex_types::cost::GasLimit;
use apex_types::ids::{ChainId, SignerLaneId};
use apex_types::sim::RevertClass;
use apex_types::ticket::{TerminalFailure, TicketOutcome, TicketStatus};
use apex_types::time::{DurationNanos, UnixNanos};
use std::sync::Arc;
use std::time::Duration;
use support::*;

// ------------------------------------------------------------------ config

/// A shadow config whose endpoints name `var`, journalling to `journal`.
fn yaml(var: &str, journal: &str) -> String {
    format!(
        r#"
chain_id: 8453
rpc:
  http: ["https://rpc.example/v1/${{{var}}}"]
  ws: wss://rpc.example/ws/${{{var}}}
inventory: data/base
journal: {journal}
report: var/r.jsonl
misses: var/m.jsonl
executor:
  address: "0x00000000000000000000000000000000000000e1"
  code_hash: "0x00000000000000000000000000000000000000000000000000000000000000c0"
  plan_version: 2
signer:
  lane: 1
  key_env: APEX_SECRET_SHADOW_TEST_TRADER
  address: "0x00000000000000000000000000000000000000a1"
  gas_reserve_wei: 1
policy:
  fee_ceiling_wei: 100000000
  min_profit_wei: 1
  slippage_bps_per_hop: 30
  max_evidence_age_blocks: 1800
  max_view_age_ms: 6000
  full_reload_every_s: 900
  report_every_s: 300
  l1_every_blocks: 30
  max_cost_confidence_bps: 10000
costs:
  gas_on_failure: 411945
  failure_ppm: 50000
capacity:
  boot_sample_s: 60
  sample_every_s: 900
  sample_for_s: 20
"#
    )
}

const SECRET: &str = "k3y-that-must-never-print";

/// `secrets_only` over one variable, never the process environment.
fn env(var: &str, value: &str) -> Env {
    Env::fixed_secrets([(var.to_string(), value.to_string())])
}

/// **The key is in the endpoint and in no rendering of the config.**
#[test]
fn a_resolved_key_is_held_as_a_secret() {
    let var = "APEX_SECRET_SHADOW_TEST_A";
    let c = ShadowConfig::from_yaml_str(&yaml(var, "var/apex/shadow.journal"), &env(var, SECRET)).unwrap();
    assert_eq!(c.rpc_http[0].expose(), &format!("https://rpc.example/v1/{SECRET}"));
    assert_eq!(c.rpc_ws.expose(), &format!("wss://rpc.example/ws/{SECRET}"));
    assert!(!format!("{c:?}").contains(SECRET), "Debug printed the key");
    assert_eq!(c.signer.key_env, "APEX_SECRET_SHADOW_TEST_TRADER");
    assert_eq!(c.executor.plan_version, 2);
}

/// An unset placeholder refuses to start, naming the variable — and only
/// `APEX_SECRET_*` names resolve at all.
#[test]
fn an_unresolved_or_unsecret_placeholder_refuses() {
    let unset = env("APEX_SECRET_SHADOW_TEST_OTHER", SECRET);
    let err = ShadowConfig::from_yaml_str(&yaml("APEX_SECRET_SHADOW_TEST_UNSET", "shadow.journal"), &unset).unwrap_err();
    assert!(matches!(&err, ShadowConfigError::Unresolved(_)), "{err}");
    assert!(err.to_string().contains("APEX_SECRET_SHADOW_TEST_UNSET"), "{err}");

    let unsecret = env("SHADOW_TEST_NOT_SECRET", "set");
    let err = ShadowConfig::from_yaml_str(&yaml("SHADOW_TEST_NOT_SECRET", "shadow.journal"), &unsecret).unwrap_err();
    assert!(matches!(err, ShadowConfigError::Unresolved(_)));
}

/// **A parse error never quotes a secret.** A key interpolated into a field of
/// the wrong type is exactly what `serde_yaml` quotes.
#[test]
fn a_parse_error_is_scrubbed_of_every_secret() {
    let var = "APEX_SECRET_SHADOW_TEST_B";
    let text = yaml(var, "shadow.journal").replace("chain_id: 8453", &format!("chain_id: x${{{var}}}"));
    let err = ShadowConfig::from_yaml_str(&text, &env(var, SECRET)).unwrap_err();
    assert!(matches!(&err, ShadowConfigError::Parse(_)), "{err}");
    let shown = err.to_string();
    assert!(!shown.contains(SECRET), "{shown}");
    assert!(shown.contains("***"), "the value was not scrubbed but elided: {shown}");
}

/// Fail closed: an unknown field, another chain, a key outside the secret
/// namespace, a journal not named as a shadow's, a period of zero.
#[test]
fn what_the_run_will_not_do_is_refused() {
    let var = "APEX_SECRET_SHADOW_TEST_C";
    let ok = yaml(var, "var/apex/shadow.journal");
    let refused = |text: String| ShadowConfig::from_yaml_str(&text, &env(var, SECRET)).unwrap_err();
    assert!(ShadowConfig::from_yaml_str(&ok, &env(var, SECRET)).is_ok());

    assert!(matches!(refused(ok.replace("  report_every_s: 300", "  report_every_s: 300\n  reprot_every_s: 1")), ShadowConfigError::Parse(_)));
    for (from, to) in [
        ("chain_id: 8453", "chain_id: 1"),
        ("key_env: APEX_SECRET_SHADOW_TEST_TRADER", "key_env: PRIVATE_KEY"),
        ("journal: var/apex/shadow.journal", "journal: var/apex/tickets.journal"),
        ("report_every_s: 300", "report_every_s: 0"),
        ("full_reload_every_s: 900", "full_reload_every_s: 0"),
        ("l1_every_blocks: 30", "l1_every_blocks: 0"),
        ("boot_sample_s: 60", "boot_sample_s: 0"),
        ("sample_every_s: 900", "sample_every_s: 0"),
        ("sample_for_s: 20", "sample_for_s: 0"),
    ] {
        assert!(matches!(refused(ok.replace(from, to)), ShadowConfigError::Invalid(_)), "{to}");
    }
    assert!(matches!(refused(ok.replace(&format!(r#"["https://rpc.example/v1/${{{var}}}"]"#), "[]")), ShadowConfigError::Invalid(_)));
}

/// **The shipped file parses**, names a shadow journal, and keeps its secrets
/// in placeholders.
#[test]
fn the_shipped_config_parses() {
    let path = format!("{}/../../ops/shadow.base.yaml", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("${APEX_SECRET_BLOCKPI_KEY}"));
    let c = ShadowConfig::load(&path, &env("APEX_SECRET_BLOCKPI_KEY", "test-only-not-a-key")).unwrap();
    assert_eq!(c.chain_id, 8453);
    assert_eq!(c.signer.key_env, "APEX_SECRET_TRADER_KEY");
    assert!(c.journal.to_str().unwrap().contains("shadow"));
    assert_eq!(c.executor.plan_version, 2);
    // Operator decision, 2026-10-06: the shadow refuses only a trade its
    // worst-case gas would leave unprofitable.
    assert_eq!(c.policy.max_cost_confidence_bps, 10_000);
}

/// **The shadow sets its own cost-confidence cap; everything else is the
/// default.** The first trade ever to pass Tier 2 in the block being built —
/// ticket 46, 2026-10-06, half a cent expected, still profitable at p99 gas —
/// was refused at the default's 1,000 bps with a width of 1,629. The operator
/// relaxed the cap for the shadow only, to 10,000 bps: a width as large as the
/// whole profit. A live run keeps the default.
#[test]
fn the_shadow_sets_its_own_cost_confidence_cap() {
    let var = "APEX_SECRET_SHADOW_TEST_CAP";
    let c = ShadowConfig::from_yaml_str(&yaml(var, "var/apex/shadow.journal"), &env(var, SECRET)).unwrap();
    let shadow = c.policy.eligibility();
    assert_eq!(shadow, EligibilityPolicy { max_cost_confidence_bps: 10_000, ..EligibilityPolicy::default() });

    let ticket_46 = EligibilityContext {
        expected_net_ev_wei: 1_841_315_099_078,
        robustness_margin_bps: 5_000,
        state_age: DurationNanos(100_000_000),
        simulation_tier: 2,
        execution_path_healthy: true,
        flash_liquidity_available: true,
        route_authorization_valid: true,
        cost_confidence_bps: 1_629,
        probability_of_profit_ppm: 900_000,
    };
    assert_eq!(
        EligibilityGate::evaluate(&ticket_46, &EligibilityPolicy::default()),
        Decision::Reject { clause: Clause::CostEstimateConfidence }
    );
    assert_eq!(EligibilityGate::evaluate(&ticket_46, &shadow), Decision::Admit);
}

// ------------------------------------------------------------------ funnel

/// **Declines are keyed fine enough to answer the run's questions**: a Tier 2
/// failure by its class, a risk refusal by its rule, a last-mile refusal by
/// its check, a chain refusal by its reason.
#[test]
fn a_decline_is_labelled_by_its_reason_and_what_refused_it() {
    let cases = [
        (Decline::SimulationFailed { class: Some(RevertClass::MinOutNotMet) }, "SIM_FAIL/MinOutNotMet"),
        (Decline::SimulationFailed { class: None }, "SIM_FAIL/no_result"),
        (Decline::RiskRefused { rule: "probability_of_profit (§2.1)".into() }, "RISK_FAIL/probability_of_profit (§2.1)"),
        (Decline::Revalidation(LastMileCheck::ExecutorFingerprint), "RISK_FAIL/executor_fingerprint"),
        (
            Decline::ChainRejected(RejectReason::NoSafeGasLimit { needed: GasLimit(777_649), largest_window: 0 }),
            "EARLIEST_FLASHBLOCK_TOO_LATE/NoSafeGasLimit",
        ),
        (Decline::NoProfitableSize, "LOW_EV/NoProfitableSize"),
        (Decline::StaleState { age: apex_types::time::DurationNanos(0) }, "STALE_STATE/StaleState"),
        (Decline::VenueUnverified { detail: "x".into() }, "VENUE_DISABLED/VenueUnverified"),
        (Decline::DispatchGateShut, "RISK_FAIL/DispatchGateShut"),
    ];
    for (d, label) in cases {
        assert_eq!(decline_label(&d), label);
    }
}

#[test]
fn an_outcome_is_labelled_by_what_it_was() {
    let shadow = |in_time| TicketOutcome::ShadowDispatched {
        stage: TicketStatus::Acknowledged,
        at: UnixNanos(1),
        in_time,
        waived: vec![],
    };
    assert_eq!(outcome_label(&shadow(true)), "shadow_dispatched/in_time");
    assert_eq!(outcome_label(&shadow(false)), "shadow_dispatched/late");
    let failed = TicketOutcome::ExplicitFailure {
        code: TerminalFailure::RiskRejected { rule: "r".into() },
        at: UnixNanos(1),
        state: Box::new(fingerprint(1, 1)),
        cause: "c".into(),
    };
    assert_eq!(outcome_label(&failed), "failed/RiskRejected");
}

#[test]
fn the_funnel_counts_every_handled_outcome() {
    let f = Funnel::default();
    f.event();
    f.event();
    f.skipped_unverified();
    f.record(&[
        Handled::Declined(Decline::NoProfitableSize),
        Handled::Declined(Decline::NoProfitableSize),
        Handled::Declined(Decline::SimulationFailed { class: Some(RevertClass::Unknown) }),
        Handled::Suppressed(Suppressed { commitment: B256::ZERO }),
        Handled::Redelivered { chain: ChainId::BASE, at: recorded_stream()[0].at },
    ]);
    let c = f.counts();
    assert_eq!((c.events, c.skipped_unverified, c.suppressed, c.redelivered), (2, 1, 1, 1));
    assert_eq!(c.declined.get("LOW_EV/NoProfitableSize"), Some(&2));
    assert_eq!(c.declined.get("SIM_FAIL/Unknown"), Some(&1));
    assert!(c.closed.is_empty());
}

// ------------------------------------------------------------------ reload queue

const A: Address = address!("00000000000000000000000000000000000000a1");
const B: Address = address!("00000000000000000000000000000000000000b2");

/// **Coalesced**: requests merge into one reload, a full request swallows the
/// pools beside it, and a take leaves nothing behind.
#[test]
fn requests_coalesce_into_one_reload() {
    let q = ReloadQueue::default();
    assert_eq!(q.take(), None);
    q.request([B, A]);
    q.request([A]);
    assert_eq!(q.take(), Some(Work::Pools(vec![A, B])));
    assert_eq!(q.take(), None);

    q.request([A]);
    q.request_full();
    q.request([B]);
    assert_eq!(q.take(), Some(Work::Full));
    assert_eq!(q.take(), None, "the pools beside a full reload are part of it");
}

/// A request made before the reload task waits is not lost.
#[tokio::test]
async fn a_request_before_the_wait_is_not_lost() {
    let q = Arc::new(ReloadQueue::default());
    q.request([A]);
    let got = tokio::time::timeout(Duration::from_secs(1), q.next()).await.expect("woken");
    assert_eq!(got, Work::Pools(vec![A]));

    let waiter = {
        let q = Arc::clone(&q);
        tokio::spawn(async move { q.next().await })
    };
    tokio::task::yield_now().await;
    q.request_full();
    assert_eq!(tokio::time::timeout(Duration::from_secs(1), waiter).await.unwrap().unwrap(), Work::Full);
}

// ------------------------------------------------------------------ nothing sent

/// **Recovery says what is true of a shadow journal**: nothing was sent, at
/// whatever stage the ticket stopped.
#[test]
fn recovery_closes_a_shadow_ticket_as_never_sent() {
    for status in [TicketStatus::Simulated, TicketStatus::Signed, TicketStatus::Dispatching, TicketStatus::Acknowledged] {
        let t = ticket_at(status);
        match NothingSent::default().resolve(&t).unwrap() {
            TicketOutcome::ExplicitFailure { code: TerminalFailure::Abandoned { at_status }, cause, state, .. } => {
                assert_eq!(at_status, status);
                assert_eq!(cause, NEVER_SENT);
                assert_eq!(*state, t.state_fingerprint);
            }
            other => panic!("{other:?}"),
        }
    }
}

#[tokio::test]
async fn settlement_is_refused_and_counted() {
    let n = NothingSent::default();
    assert!(matches!(n.observe(B256::ZERO).await, Err(Decline::ChainUnavailable { .. })));
    assert!(n.observe(B256::ZERO).await.is_err());
    assert_eq!(n.asked(), 2);
}

// ------------------------------------------------------------------ universe

/// **The pools a WETH cycle can use**, on the venues the executor can reach,
/// and nothing else the census kept. Filtered by venue **before** pairing: with
/// Uniswap unreachable, PancakeSwap's lone WETH/USDC pool does not make a pair
/// out of Uniswap's two.
#[test]
fn the_universe_is_the_reachable_weth_pairs_with_two_pools() {
    use apex_runtime::live::inventory::Venue;
    let dir = tempfile::tempdir().unwrap();
    let weth = "0x4200000000000000000000000000000000000006";
    let usdc = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913";
    let other = "0x00000000000000000000000000000000000000c3";
    let rec = |pool: &str, t0: &str, t1: &str| {
        serde_json::json!({ "pool": pool, "token0": t0, "token1": t1, "fee": 500, "fee_ppm_onchain": 500, "hub_usd_liquidity": 1e6 }).to_string()
    };
    let p = |n: u8| format!("0x{:040x}", n);
    for (venue, lines) in [
        ("uniswap_v3", vec![rec(&p(1), weth, usdc), rec(&p(2), weth, usdc), rec(&p(3), usdc, other), rec(&p(4), usdc, other)]),
        ("aerodrome_slipstream", vec![rec(&p(5), weth, other)]),
        ("pancakeswap_v3", vec![rec(&p(6), weth, other), rec(&p(7), weth, usdc)]),
    ] {
        std::fs::create_dir_all(dir.path().join(venue)).unwrap();
        std::fs::write(dir.path().join(venue).join("pools.jsonl"), lines.join("\n")).unwrap();
    }
    let universe = |venues: &[Venue]| {
        let mut got: Vec<u8> = apex_runtime::shadow::universe(dir.path(), venues)
            .unwrap()
            .iter()
            .map(|s| s.pool.as_slice()[19])
            .collect();
        got.sort();
        got
    };
    assert_eq!(universe(&[Venue::UniswapV3, Venue::Slipstream]), vec![1, 2]);
    assert_eq!(universe(&Venue::ALL), vec![1, 2, 5, 6, 7]);
    assert_eq!(universe(&[Venue::Slipstream, Venue::PancakeV3]), vec![5, 6]);
    assert_eq!(universe(&[Venue::UniswapV3]), vec![1, 2]);
}

// ------------------------------------------------------------------ the plane's misses

fn signer_pool() -> SignerPool {
    let mut version = [0u8; 32];
    version[28..].copy_from_slice(&EXECUTOR_VERSION.to_be_bytes());
    SignerPool::new(
        ExecutorAuth { chain: BASE, executor: EXECUTOR, executor_version: version },
        vec![LaneConfig { id: SignerLaneId(1), address: [0x22; 20], gas_reserve_wei: 1 }],
    )
}

/// **Drained, not cloned**: the report takes the ledger and the plane starts
/// the next period empty. Unbooted, so the gate is shut and every candidate is
/// declined with a miss filed.
#[tokio::test]
async fn the_miss_ledger_is_drained() {
    let c = candidate(1, 47_079_437, 320_000_000_000);
    let plane = Plane::new(Ports {
        registry: Arc::new(TicketRegistry::new(Box::new(InMemoryJournal::new()), Box::new(ManualClock::at(1)))),
        pool: Arc::new(signer_pool()),
        gate: Arc::new(DispatchGate::shut()),
        dispatch: DispatchLane::Shadow(Arc::new(NullDispatcher::new())),
        chain: Arc::new(FakeChain::landing()),
        search: Arc::new(FixedSearch::new(vec![c.clone()])),
        econ: Arc::new(PassThroughEconomics(c)),
        sim: Arc::new(AlwaysSucceeds),
        risk: Arc::new(AlwaysAdmits),
        commitments: Arc::new(FixtureCommitments),
        calls: Arc::new(FixtureCalls),
        signer: Arc::new(EchoSigner),
        live: Arc::new(FixedReadings(readings())),
        settlement: Arc::new(NothingSent::default()),
    });
    let stream = recorded_stream();
    let handled = plane.on_event(&stream[0]).await;
    assert!(matches!(&handled[..], [Handled::Declined(Decline::DispatchGateShut)]), "{handled:?}");
    assert_eq!(plane.misses().len(), 1);

    assert_eq!(plane.drain_misses().len(), 1);
    assert!(plane.misses().is_empty(), "drained, not copied");
    plane.on_event(&stream[1]).await;
    assert_eq!(plane.drain_misses().len(), 1, "the next period's alone");
}
