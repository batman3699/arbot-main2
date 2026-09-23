//! A process that exists to be killed.
//!
//! `tests/crash_recovery.rs` spawns this, waits for it to say it has tickets in
//! flight, and `SIGKILL`s it. There is no way to simulate that in-process: the
//! thing under test is what a real journal file looks like after a real process
//! stops existing between one write and the next, and a mock would only ever
//! produce the file shape its author already imagined.
//!
//! Deliberately has no shutdown path. A victim that could exit cleanly would
//! eventually be made to, and then the test would stop testing a crash.

use alloy_primitives::{B256, U256};
use apex_capture::journal::FileJournal;
use apex_capture::registry::TicketRegistry;
use apex_capture::SystemClock;
use apex_types::cost::GasLimit;
use apex_types::ids::{ChainId, StrategyId, TicketId};
use apex_types::route::{ComplexityCost, RouteCommitment};
use apex_types::state::StateFingerprint;
use apex_types::ticket::{
    ExecutionWindow, OpportunityTicket, SubmissionPolicy, TicketStatus,
};
use apex_types::time::UnixNanos;
use std::collections::BTreeMap;
use std::io::Write;
use std::process::ExitCode;

fn ticket() -> OpportunityTicket {
    OpportunityTicket {
        ticket_id: TicketId(0),
        chain_id: ChainId::BASE,
        strategy: StrategyId(1),
        state_fingerprint: StateFingerprint {
            chain_id: ChainId::BASE,
            parent_block_hash: B256::repeat_byte(0x11),
            confirmed_block_number: 50_684_845,
            preconf_sequence: Some(7),
            flashblock_index: Some(3),
            state_root_or_equivalent: None,
            block_hash_if_available: Some(B256::repeat_byte(0x22)),
            state_delta_hash: B256::repeat_byte(0x33),
            venue_state_version: BTreeMap::new(),
            external_dependency_fingerprint: None,
        },
        route_commitment: RouteCommitment {
            hops: Vec::new(),
            complexity_cost: ComplexityCost {
                hops: 2,
                external_calls: 3,
                calldata_bytes: 1_200,
                state_deps: 4,
                tick_crossings: 1,
                hooks: 0,
                gas_estimate: 310_000,
                failure_surface: 0.02,
            },
            route_hash: B256::repeat_byte(0x44),
        },
        exact_input: U256::from(1_000_000_000_000_000_000u128),
        expected_net_ev: 1_234,
        robustness_margin: 0.25,
        validity_start: UnixNanos(1_700_000_000_000_000_000),
        // Far out, so the test's own sweep is the thing that closes tickets
        // rather than a deadline that happened to pass while the test ran.
        dispatch_deadline: UnixNanos(u64::MAX),
        target_execution_window: ExecutionWindow {
            earliest: UnixNanos(1_700_000_000_000_000_000),
            latest: UnixNanos(u64::MAX),
            earliest_eligible_flashblock: Some(3),
        },
        signer_lane: None,
        nonce: None,
        flash_source: None,
        simulation_result_hash: B256::repeat_byte(0x55),
        submission_policy: SubmissionPolicy::Private,
        required_gas_limit: GasLimit(500_000),
        created_at: UnixNanos(1_700_000_000_000_000_000),
        status: TicketStatus::Observed,
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let (Some(path), Some(n_unsigned), Some(n_signed)) = (
        args.get(1),
        args.get(2).and_then(|s| s.parse::<usize>().ok()),
        args.get(3).and_then(|s| s.parse::<usize>().ok()),
    ) else {
        eprintln!("usage: crash_victim <journal path> <n unsigned> <n signed>");
        return ExitCode::FAILURE;
    };

    let Ok(journal) = FileJournal::open(path) else {
        eprintln!("cannot open {path}");
        return ExitCode::FAILURE;
    };
    // What a real boot does before admitting anything: find out what the
    // previous run handed out. Without this the fresh counter restarts at 1 and
    // the second generation of tickets reuses the first's ids, which the
    // journal cannot represent -- `scan` refuses such a file outright.
    let Ok(previous) = apex_capture::recover::scan(&journal) else {
        eprintln!("cannot scan the existing journal");
        return ExitCode::FAILURE;
    };
    let registry = TicketRegistry::new(Box::new(journal), Box::new(SystemClock));
    registry.reserve_ids_through(previous.max_ticket_id);

    // Held for the rest of the process, which is the point: these tickets are
    // checked out and in flight at the moment of death. Dropping them would
    // close them, and a cleanly closed ticket is not what recovery is for.
    let mut held = Vec::new();
    for (n, stop) in [(n_unsigned, TicketStatus::Authorized), (n_signed, TicketStatus::Signed)] {
        for _ in 0..n {
            let Ok(id) = registry.admit(ticket()) else {
                eprintln!("admit failed");
                return ExitCode::FAILURE;
            };
            let Ok(mut g) = registry.checkout(id) else {
                eprintln!("checkout failed");
                return ExitCode::FAILURE;
            };
            for s in [TicketStatus::Reserved, TicketStatus::Exacting, TicketStatus::Simulated] {
                if g.advance(s).is_err() {
                    eprintln!("advance failed");
                    return ExitCode::FAILURE;
                }
            }
            if stop >= TicketStatus::Authorized && g.advance(TicketStatus::Authorized).is_err() {
                eprintln!("advance failed");
                return ExitCode::FAILURE;
            }
            if stop >= TicketStatus::Signed && g.advance(TicketStatus::Signed).is_err() {
                eprintln!("advance failed");
                return ExitCode::FAILURE;
            }
            held.push(g);
        }
    }

    println!("READY {}", held.len());
    let _ = std::io::stdout().flush();

    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
