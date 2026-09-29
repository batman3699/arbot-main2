//! **INV-06's cross-language differential.**
//!
//! `apex-exec` recomputes `MultiVenueArbImplementation.planCommitment` in Rust,
//! because the signer has to produce the value the contract will recompute and
//! revert on. A Rust mirror of a Solidity hash drifts unless something holds the
//! two together, and the honest thing to hold them together with is the
//! contract's own answer.
//!
//! `crates/apex-exec/tests/fixtures/plan_commitments.json` is that answer.
//! `test/PlanCommitmentFixture.t.sol` asserts the contract still produces it;
//! this asserts the Rust encoder reproduces it. **Neither side writes it** — a
//! test that regenerated the fixture would make the two agree by construction,
//! which is the one thing a differential must not do.

use alloy_primitives::{address, Address, B256, U256};
use apex_exec::commitment::{
    loans_hash, plan_commitment, steps_hash, Loan, LoanProvider, Op, PlanV2,
};
use std::collections::BTreeMap;

const TOKEN_A: Address = address!("1111111111111111111111111111111111111111");
const LENDER: Address = address!("3333333333333333333333333333333333333333");

fn empty() -> PlanV2 {
    PlanV2 {
        loans: Vec::new(),
        cycle_slippage_bps: 0,
        steps: Vec::new(),
        min_profit: U256::ZERO,
        declared_residue: U256::ZERO,
        chain_id: 0,
        deadline: 0,
    }
}

fn one_loan(amount: u128) -> Vec<Loan> {
    vec![Loan {
        token: TOKEN_A,
        amount: U256::from(amount),
        provider: LoanProvider::Balancer,
        provider_addr: LENDER,
    }]
}

fn steps(a: &[u8], b: &[u8]) -> Vec<apex_exec::commitment::Step> {
    vec![
        apex_exec::commitment::Step { op: Op::UniV3, data: a.to_vec() },
        apex_exec::commitment::Step { op: Op::Balancer, data: b.to_vec() },
    ]
}

/// The six cases, in the same order and with the same contents as
/// `PlanCommitmentFixture.t.sol::_cases`.
fn cases() -> Vec<(&'static str, PlanV2)> {
    let mut loan_and_steps = empty();
    loan_and_steps.loans = one_loan(1_000_000_000_000_000_000);
    loan_and_steps.steps = steps(&[0xaa, 0xbb], &[0xcc, 0xdd]);
    loan_and_steps.cycle_slippage_bps = 30;

    let mut split_a = empty();
    split_a.steps = steps(&[0xaa, 0xbb, 0xcc], &[0xdd]);

    let mut split_b = empty();
    split_b.steps = steps(&[0xaa], &[0xbb, 0xcc, 0xdd]);

    let mut every = empty();
    every.loans = one_loan(1);
    every.steps = steps(&[0xaa, 0xbb], &[0xcc, 0xdd]);
    every.cycle_slippage_bps = u16::MAX;
    every.min_profit = U256::MAX;
    every.declared_residue = U256::from(7u64);
    every.chain_id = 8453;
    every.deadline = 1_781_049_614;

    let mut one = empty();
    one.loans = one_loan(1_000_000_000_000_000_000);

    vec![
        ("empty", empty()),
        ("one_loan", one),
        ("loan_and_steps", loan_and_steps),
        ("split_payload_a", split_a),
        ("split_payload_b", split_b),
        ("every_field_set", every),
    ]
}

struct Fixture {
    block_chain_id: u64,
    executor: Address,
    cases: BTreeMap<String, B256>,
}

fn fixture() -> Fixture {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/plan_commitments.json");
    let text = std::fs::read_to_string(path).expect("the tracked fixture");
    let doc: serde_json::Value = serde_json::from_str(&text).expect("valid json");

    let block_chain_id =
        doc.get("block_chain_id").and_then(serde_json::Value::as_u64).expect("block_chain_id");
    let executor: Address = doc
        .get("executor")
        .and_then(serde_json::Value::as_str)
        .expect("executor")
        .parse()
        .expect("an address");
    let cases = doc
        .get("cases")
        .and_then(serde_json::Value::as_object)
        .expect("cases")
        .iter()
        .map(|(k, v)| {
            (k.clone(), v.as_str().expect("a hash").parse::<B256>().expect("a B256"))
        })
        .collect();

    Fixture { block_chain_id, executor, cases }
}

/// **The differential.** Every case the contract produced, reproduced here.
#[test]
fn plan_commitment_matches_the_contract() {
    let f = fixture();
    assert_eq!(f.cases.len(), 6, "the fixture lost a case");

    for (name, plan) in cases() {
        let recorded = f.cases.get(name).copied().unwrap_or_else(|| {
            panic!("the fixture has no case named {name}");
        });
        let computed = plan_commitment(&plan, f.block_chain_id, f.executor);
        assert_eq!(
            computed, recorded,
            "{name}: the Rust encoder and the contract disagree. A signer using this \
             value would produce a transaction the executor reverts with CommitmentMismatch."
        );
    }
}

/// The empty rolling hashes are `bytes32(0)`, which is what makes the "empty"
/// case meaningful: a loop that seeded its accumulator differently would pass
/// every other case and fail this one.
#[test]
fn an_empty_list_hashes_to_zero() {
    assert_eq!(loans_hash(&[]), B256::ZERO);
    assert_eq!(steps_hash(&[]), B256::ZERO);
}

/// **The property the step hash exists for.** The contract's own comment: *"a
/// long payload cannot be split across a boundary to collide with a different
/// step list."* Two step lists whose payloads concatenate identically must not
/// produce the same hash.
#[test]
fn a_payload_boundary_moves_the_commitment() {
    let f = fixture();
    let a = plan_commitment(&cases()[3].1, f.block_chain_id, f.executor);
    let b = plan_commitment(&cases()[4].1, f.block_chain_id, f.executor);
    assert_ne!(a, b, "aabbcc|dd and aa|bbccdd hashed the same");

    // And the concatenations really are identical, or the test proves nothing.
    let concat_a: Vec<u8> = cases()[3].1.steps.iter().flat_map(|s| s.data.clone()).collect();
    let concat_b: Vec<u8> = cases()[4].1.steps.iter().flat_map(|s| s.data.clone()).collect();
    assert_eq!(concat_a, concat_b, "the two cases must differ only in where the boundary falls");
}

/// Loan order is part of the preimage — a rolling hash makes each element's
/// position part of its own input, so a reordered list is a different plan.
#[test]
fn loan_order_is_part_of_the_hash() {
    let a = Loan {
        token: TOKEN_A,
        amount: U256::from(1u64),
        provider: LoanProvider::Balancer,
        provider_addr: LENDER,
    };
    let b = Loan { amount: U256::from(2u64), ..a };

    assert_ne!(loans_hash(&[a, b]), loans_hash(&[b, a]));
}

/// The deployment is part of the hash: the same plan committed for one executor
/// must not execute on another. INV-05's wrong-chain submission, expressed where
/// it can be enforced.
#[test]
fn the_deployment_is_part_of_the_commitment() {
    let f = fixture();
    let plan = cases()[2].1.clone();
    let here = plan_commitment(&plan, f.block_chain_id, f.executor);

    assert_ne!(
        here,
        plan_commitment(&plan, f.block_chain_id + 1, f.executor),
        "the same plan on another chain id must not share a commitment"
    );
    assert_ne!(
        here,
        plan_commitment(&plan, f.block_chain_id, Address::repeat_byte(0x99)),
        "the same plan at another executor must not share a commitment"
    );
}

/// `chainId` and `block.chainid` are **separate** fields and both are hashed.
/// The contract carries both deliberately — one is where the code is running,
/// the other is what the planner said it built for — so moving either alone must
/// move the commitment.
#[test]
fn the_planners_chain_and_the_running_chain_are_both_committed() {
    let f = fixture();
    let mut plan = cases()[2].1.clone();
    let before = plan_commitment(&plan, f.block_chain_id, f.executor);

    plan.chain_id = 8453;
    let after = plan_commitment(&plan, f.block_chain_id, f.executor);
    assert_ne!(before, after, "the planner's declared chain is committed");
}

/// Every field of `PlanV2` moves the commitment. The contract has
/// `testEveryCommittedFieldMovesTheCommitment`; this is its Rust twin, and it
/// exists because a mirror can drop a field silently while every fixture case
/// still passes — the fixture only fails if a *case* varies that field.
#[test]
fn every_field_moves_the_commitment() {
    let f = fixture();
    let base = cases()[5].1.clone();
    let baseline = plan_commitment(&base, f.block_chain_id, f.executor);

    let mut variants: Vec<(&str, PlanV2)> = Vec::new();

    let mut v = base.clone();
    v.cycle_slippage_bps = 1;
    variants.push(("cycle_slippage_bps", v));

    let mut v = base.clone();
    v.min_profit = U256::from(1u64);
    variants.push(("min_profit", v));

    let mut v = base.clone();
    v.declared_residue = U256::ZERO;
    variants.push(("declared_residue", v));

    let mut v = base.clone();
    v.chain_id = 1;
    variants.push(("chain_id", v));

    let mut v = base.clone();
    v.deadline = 1;
    variants.push(("deadline", v));

    let mut v = base.clone();
    v.loans.clear();
    variants.push(("loans", v));

    let mut v = base.clone();
    v.steps.clear();
    variants.push(("steps", v));

    let mut v = base.clone();
    v.loans[0].provider = LoanProvider::Aave;
    variants.push(("loan.provider", v));

    let mut v = base.clone();
    v.steps[0].op = Op::Generic;
    variants.push(("step.op", v));

    for (field, variant) in variants {
        assert_ne!(
            plan_commitment(&variant, f.block_chain_id, f.executor),
            baseline,
            "{field} changed without moving the commitment"
        );
    }
}
