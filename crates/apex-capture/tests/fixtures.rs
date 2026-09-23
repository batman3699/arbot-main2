// Minimal valid instances. `include!`d rather than declared as a module so each
// integration-test binary gets its own copy without a shared dev-dependency.
// Inner doc comments (`//!`) are invalid inside the `mod { }` that includes it.

use alloy_primitives::{Address, B256, U256};
use apex_types::cost::{GasDistribution, GasLimit, GasUsed};
use apex_types::ids::*;
use apex_types::route::{ComplexityCost, RouteCommitment};
use apex_types::state::StateFingerprint;
use apex_types::ticket::{ExecutionWindow, OpportunityTicket, SubmissionPolicy, TicketStatus};
use apex_types::time::UnixNanos;
use std::collections::BTreeMap;

#[allow(dead_code)]
pub fn fingerprint() -> StateFingerprint {
    StateFingerprint {
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
    }
}

#[allow(dead_code)]
pub fn ticket() -> OpportunityTicket {
    OpportunityTicket {
        ticket_id: TicketId(1),
        chain_id: ChainId::BASE,
        strategy: StrategyId(1),
        state_fingerprint: fingerprint(),
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
        dispatch_deadline: UnixNanos(1_700_000_000_200_000_000),
        target_execution_window: ExecutionWindow {
            earliest: UnixNanos(1_700_000_000_000_000_000),
            latest: UnixNanos(1_700_000_000_400_000_000),
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

#[allow(dead_code)]
pub fn gas_distribution() -> GasDistribution {
    GasDistribution {
        p50: GasUsed(260_000),
        p90: GasUsed(295_000),
        p99: GasUsed(310_000),
        max_observed: GasUsed(402_000),
    }
}

#[allow(dead_code)]
pub fn address() -> Address {
    Address::repeat_byte(0xAB)
}
