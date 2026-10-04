//! Task 8.5 — **one call per ticket**: what is simulated is what is signed.
//!
//! Before this, the `Simulator` port took only a `Candidate` and the `Signer`
//! only an `ExecutionCommitment`, so neither could see the transaction at all —
//! a real simulator had nothing to simulate and a real signer nothing to sign.
//! The plane now builds one `ExecutorCall` per ticket (§25) and hands the same
//! value to both.

mod support;

use apex_capture::recover::DispatchGate;
use apex_capture::registry::TicketRegistry;
use apex_capture::signer::{ExecutorAuth, LaneConfig, SignerPool};
use apex_capture::{InMemoryJournal, ManualClock, NullDispatcher};
use apex_chain::adapter::SignedPayload;
use apex_exec::call::ExecutorCall;
use apex_runtime::plane::{
    fee_caps, CallBuilder, Decline, DispatchLane, FeeCaps, Handled, LiveReadings, Plane, Ports,
    Signer, Simulator,
};
use apex_types::candidate::Candidate;
use apex_types::commitment::ExecutionCommitment;
use apex_types::cost::GasLimit;
use apex_types::ids::SignerLaneId;
use apex_types::sim::SimulationResult;
use apex_types::ticket::TicketOutcome;
use apex_types::time::UnixNanos;
use alloy_primitives::Address;
use std::sync::{Arc, Mutex};
use support::*;

const BOOT: UnixNanos = UnixNanos(1_000_000_000);
const LANE_ADDRESS: [u8; 20] = [0x22; 20];
/// The gas limit the chain schedules with, unlike any candidate's.
const CHOSEN: GasLimit = GasLimit(987_654);

/// Records what it was asked to simulate, as whom, and at what gas limit.
#[derive(Default)]
struct SpySimulator {
    seen: Mutex<Vec<(Vec<u8>, Address, GasLimit)>>,
}

#[async_trait::async_trait]
impl Simulator for SpySimulator {
    async fn simulate(
        &self,
        c: &Candidate,
        call: &ExecutorCall,
        from: Address,
        gas_limit: GasLimit,
    ) -> Result<SimulationResult, Decline> {
        self.seen.lock().unwrap().push((call.data().to_vec(), from, gas_limit));
        AlwaysSucceeds.simulate(c, call, from, gas_limit).await
    }
}

/// Records what it was asked to sign, at what fees and what gas limit.
#[derive(Default)]
struct SpySigner {
    seen: Mutex<Vec<(Vec<u8>, FeeCaps, GasLimit)>>,
}

impl Signer for SpySigner {
    fn sign(
        &self,
        auth: &apex_capture::revalidate::SigningAuthorization,
        call: &ExecutorCall,
        gas_limit: GasLimit,
        fees: FeeCaps,
    ) -> Result<SignedPayload, Decline> {
        self.seen.lock().unwrap().push((call.data().to_vec(), fees, gas_limit));
        EchoSigner.sign(auth, call, gas_limit, fees)
    }
}

/// A builder that refuses: a route through a venue no executor op encodes.
struct Unencodable;

impl CallBuilder for Unencodable {
    fn build(&self, _c: &Candidate, _k: &ExecutionCommitment) -> Result<ExecutorCall, Decline> {
        Err(Decline::VenueUnverified { detail: "no executor op encodes venue 9".into() })
    }
}

fn pool() -> SignerPool {
    let mut version = [0u8; 32];
    version[28..].copy_from_slice(&EXECUTOR_VERSION.to_be_bytes());
    SignerPool::new(
        ExecutorAuth { chain: BASE, executor: EXECUTOR, executor_version: version },
        vec![LaneConfig {
            id: SignerLaneId(1),
            address: LANE_ADDRESS,
            gas_reserve_wei: 1_000_000_000_000_000_000,
        }],
    )
}

fn plane(
    calls: Arc<dyn CallBuilder>,
    sim: Arc<dyn Simulator>,
    signer: Arc<dyn Signer>,
    readings: LiveReadings,
) -> Plane {
    let plane = Plane::new(Ports {
        registry: Arc::new(TicketRegistry::new(
            Box::new(InMemoryJournal::new()),
            Box::new(ManualClock::at(BOOT.0)),
        )),
        pool: Arc::new(pool()),
        gate: Arc::new(DispatchGate::shut()),
        dispatch: DispatchLane::Shadow(Arc::new(NullDispatcher::new())),
        // Not the candidate's limit: what the plane simulates and signs at must
        // be the chain's choice.
        chain: Arc::new(FakeChain::landing().choosing(CHOSEN)),
        search: Arc::new(FixedSearch::new(vec![candidate(1, 47_079_437, 320_000_000_000)])),
        econ: Arc::new(PassThroughEconomics::default()),
        sim,
        risk: Arc::new(AlwaysAdmits),
        commitments: Arc::new(FixtureCommitments),
        calls,
        signer,
        live: Arc::new(FixedReadings(readings)),
        settlement: Arc::new(LandsAndFinalizes),
    });
    plane.boot(&NoChain, BOOT).expect("boot");
    plane
}

async fn run_one(plane: &Plane) -> Handled {
    let mut handled = plane.on_event(&recorded_stream()[0]).await;
    assert_eq!(handled.len(), 1);
    handled.remove(0)
}

/// **The claim.** The simulator and the signer were handed the same bytes, and
/// they are the bytes the builder produced.
#[tokio::test]
async fn the_simulator_and_the_signer_see_the_same_call() {
    let sim = Arc::new(SpySimulator::default());
    let signer = Arc::new(SpySigner::default());
    let plane = plane(Arc::new(FixtureCalls), sim.clone(), signer.clone(), readings());
    run_one(&plane).await;

    let simulated = sim.seen.lock().unwrap().clone();
    let signed = signer.seen.lock().unwrap().clone();
    assert_eq!(simulated.len(), 1);
    assert_eq!(signed.len(), 1);
    assert_eq!(simulated[0].0, signed[0].0, "what was signed is not what was simulated");
    assert!(!signed[0].0.is_empty());
}

/// **And at the same gas limit.** The limit a call is signed with is the one
/// it ran at in simulation: a simulation given more gas than the transaction
/// passes a call that then runs out on chain.
#[tokio::test]
async fn the_simulation_runs_at_the_gas_limit_it_is_signed_with() {
    let sim = Arc::new(SpySimulator::default());
    let signer = Arc::new(SpySigner::default());
    let plane = plane(Arc::new(FixtureCalls), sim.clone(), signer.clone(), readings());
    run_one(&plane).await;

    let simulated = sim.seen.lock().unwrap()[0].2;
    let signed = signer.seen.lock().unwrap()[0].2;
    assert_eq!((simulated, signed), (CHOSEN, CHOSEN));
}

/// `onlyExecutor` gates `startV2`, so a simulation as anyone but the lane that
/// will sign answers a different question.
#[tokio::test]
async fn the_simulation_runs_as_the_assigned_lane() {
    let sim = Arc::new(SpySimulator::default());
    let plane = plane(Arc::new(FixtureCalls), sim.clone(), Arc::new(EchoSigner), readings());
    run_one(&plane).await;
    assert_eq!(sim.seen.lock().unwrap()[0].1, Address::from(LANE_ADDRESS));
}

/// A route no executor op can encode is declined **before** simulation, and the
/// reason is the venue's — which is the actionable one.
#[tokio::test]
async fn a_route_that_cannot_be_encoded_is_declined_before_simulation() {
    let sim = Arc::new(SpySimulator::default());
    let signer = Arc::new(SpySigner::default());
    let plane = plane(Arc::new(Unencodable), sim.clone(), signer.clone(), readings());

    match run_one(&plane).await {
        Handled::Closed { outcome, .. } => {
            assert!(matches!(*outcome, TicketOutcome::ExplicitFailure { .. }), "{outcome:?}")
        }
        other => panic!("expected a closed ticket, got {other:?}"),
    }
    assert!(sim.seen.lock().unwrap().is_empty(), "simulated a call that does not exist");
    assert!(signer.seen.lock().unwrap().is_empty());
    assert_eq!(plane.misses().by_reason().get("VENUE_DISABLED"), Some(&1));
}

/// Fees are **per gas**, derived from the observed base fee, and never above the
/// policy ceiling. No tip: nothing has measured what one buys on Base, and a
/// fabricated bid is a cost the EV would then have to clear.
#[test]
fn fee_caps_are_per_gas_and_under_the_ceiling() {
    let mut r = readings();
    r.observed_fee_wei = 200_000;
    r.fee_ceiling_wei = 5_000_000;
    assert_eq!(fee_caps(&r), FeeCaps { max_fee_per_gas: 400_000, max_priority_fee_per_gas: 0 });

    r.fee_ceiling_wei = 300_000;
    assert_eq!(fee_caps(&r).max_fee_per_gas, 300_000, "the ceiling binds");
}

/// And the plane actually uses them.
#[tokio::test]
async fn the_signer_is_given_the_fee_caps_of_the_readings() {
    let signer = Arc::new(SpySigner::default());
    let r = readings();
    let plane = plane(Arc::new(FixtureCalls), Arc::new(AlwaysSucceeds), signer.clone(), r.clone());
    run_one(&plane).await;
    assert_eq!(signer.seen.lock().unwrap()[0].1, fee_caps(&r));
}
