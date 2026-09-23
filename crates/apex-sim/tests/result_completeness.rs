//! Task 4.3 — a Tier 2 result says everything §20 requires, and its hash
//! compares two backends rather than two runs.

use alloy_primitives::{Address, B256};
use apex_types::ids::{ChainId, TokenId, VenueId};
use apex_types::sim::{RevertClass, SimulationResult, SimulationTier};
use apex_types::state::StateFingerprint;
use apex_types::time::DurationNanos;
use std::collections::BTreeMap;

fn token(n: u8) -> TokenId {
    TokenId {
        chain: ChainId::BASE,
        address: Address::repeat_byte(n),
    }
}

fn fingerprint(block: u64) -> StateFingerprint {
    StateFingerprint {
        chain_id: ChainId::BASE,
        parent_block_hash: B256::repeat_byte(0xaa),
        confirmed_block_number: block,
        preconf_sequence: Some(3),
        flashblock_index: Some(2),
        state_root_or_equivalent: Some(B256::repeat_byte(0xbb)),
        block_hash_if_available: None,
        state_delta_hash: B256::repeat_byte(0xcc),
        venue_state_version: BTreeMap::from([(VenueId(1), 42), (VenueId(2), 43)]),
        external_dependency_fingerprint: None,
    }
}

fn result(tier: SimulationTier) -> SimulationResult {
    let mut r = SimulationResult {
        tier,
        success: true,
        revert: None,
        gas_used: 187_432,
        balance_deltas: BTreeMap::from([(token(1), 1_000_000i128), (token(2), -999_000)]),
        loan_repaid: true,
        profit_invariant_held: true,
        token_residues: BTreeMap::from([(token(3), 7u128)]),
        state_after: fingerprint(20_000_001),
        simulated_at_state: fingerprint(20_000_000),
        result_hash: B256::ZERO,
        elapsed: DurationNanos(41_000_000),
    };
    r.result_hash = r.canonical_hash();
    r
}

/// Every field §20 requires of a Tier 2 result.
///
/// Checked by **exhaustive destructuring** rather than by comparing a list of
/// names against serde's output. A name list has to be maintained in step with
/// the struct and silently under-tests when it is not; a destructure that
/// binds every field fails to compile the moment one is added, which makes the
/// completeness check impossible to forget rather than merely likely to be
/// noticed.
#[test]
fn a_tier_two_result_carries_every_required_field() {
    let r = result(SimulationTier::Tier2FullEvm);
    let SimulationResult {
        tier,
        success,
        revert,
        gas_used,
        balance_deltas,
        loan_repaid,
        profit_invariant_held,
        token_residues,
        state_after,
        simulated_at_state,
        result_hash,
        elapsed,
    } = r.clone();

    assert_eq!(tier, SimulationTier::Tier2FullEvm);
    assert!(success);
    assert!(revert.is_none());
    assert!(gas_used > 0);
    assert_eq!(balance_deltas.len(), 2, "per-token deltas, signed");
    assert!(loan_repaid);
    assert!(profit_invariant_held);
    assert_eq!(token_residues.len(), 1, "residues are recorded, not assumed zero");
    assert_ne!(
        state_after.confirmed_block_number, simulated_at_state.confirmed_block_number,
        "the state it ran against and the state it left are separate facts"
    );
    assert_eq!(result_hash, r.canonical_hash());
    assert!(elapsed.0 > 0);
}

/// **`SimulationResult` cannot be serialised to JSON as it stands**, and
/// Task 4.5 needs it to be.
///
/// `balance_deltas` and `token_residues` are `BTreeMap<TokenId, _>`, and JSON
/// object keys must be strings — serde fails with "key must be a string". So
/// the red/blue diff file §35 asks for (`sim-diff-<date>.csv`) cannot be
/// written from these records yet, and neither can a replay corpus.
///
/// Recorded rather than fixed: the fix is a wire-format decision (string keys
/// via `Display`, or maps as pair sequences) and it belongs with Task 4.5,
/// which is blocked on a live Base node regardless. This test exists to FAIL
/// when somebody fixes it, so the decision is made deliberately rather than
/// discovered.
#[test]
fn simulation_results_do_not_serialise_to_json_yet() {
    let r = result(SimulationTier::Tier2FullEvm);
    assert!(
        serde_json::to_value(&r).is_err(),
        "results now serialise -- update Task 4.5's notes and delete this test"
    );
}

/// The hash exists so two BACKENDS can be compared by one equality. Including
/// the tier would make that impossible — Tier 1 agreeing with Tier 2 is the
/// entire point of §35's red/blue comparison.
#[test]
fn the_hash_ignores_which_tier_produced_the_result() {
    let local = result(SimulationTier::Tier1LocalExact);
    let node = result(SimulationTier::Tier2FullEvm);
    assert_ne!(local.tier, node.tier);
    assert_eq!(
        local.canonical_hash(),
        node.canonical_hash(),
        "two backends reporting the same outcome must hash identically, or red/blue \
         can never agree"
    );
}

/// ...and timing, for the same reason in reverse: two correct runs take
/// different amounts of time, so a hash including duration disagrees with
/// itself.
#[test]
fn the_hash_ignores_how_long_the_simulation_took() {
    let mut fast = result(SimulationTier::Tier2FullEvm);
    let mut slow = fast.clone();
    fast.elapsed = DurationNanos(1);
    slow.elapsed = DurationNanos(9_999_999_999);
    assert_eq!(fast.canonical_hash(), slow.canonical_hash());
}

/// Two results about different states are not the same result, however
/// identical their numbers. Excluding this would make the quorum agree across
/// a reorg.
#[test]
fn the_hash_distinguishes_results_simulated_at_different_states() {
    let a = result(SimulationTier::Tier2FullEvm);
    let mut b = a.clone();
    b.simulated_at_state = fingerprint(20_000_099);
    assert_ne!(
        a.canonical_hash(),
        b.canonical_hash(),
        "identical numbers against a different state must not hash the same"
    );
}

/// Every semantic field participates. A field that does not reach the hash is
/// a field two backends can disagree about while comparing equal.
#[test]
fn every_semantic_field_changes_the_hash() {
    let base = result(SimulationTier::Tier2FullEvm).canonical_hash();

    /// A named change to one semantic field.
    type Mutation = (&'static str, Box<dyn Fn(&mut SimulationResult)>);

    let mutations: Vec<Mutation> = vec![
        ("success", Box::new(|r: &mut SimulationResult| r.success = false)),
        (
            "revert",
            Box::new(|r: &mut SimulationResult| {
                r.revert = Some((RevertClass::MinOutNotMet, vec![1, 2, 3]))
            }),
        ),
        ("gas_used", Box::new(|r: &mut SimulationResult| r.gas_used += 1)),
        (
            "balance_deltas",
            Box::new(|r: &mut SimulationResult| {
                r.balance_deltas.insert(token(1), 999);
            }),
        ),
        ("loan_repaid", Box::new(|r: &mut SimulationResult| r.loan_repaid = false)),
        (
            "profit_invariant_held",
            Box::new(|r: &mut SimulationResult| r.profit_invariant_held = false),
        ),
        (
            "token_residues",
            Box::new(|r: &mut SimulationResult| {
                r.token_residues.insert(token(3), 8);
            }),
        ),
        (
            "state_after",
            Box::new(|r: &mut SimulationResult| r.state_after = fingerprint(30_000_000)),
        ),
        (
            "simulated_at_state",
            Box::new(|r: &mut SimulationResult| r.simulated_at_state = fingerprint(30_000_000)),
        ),
    ];

    for (field, mutate) in mutations {
        let mut r = result(SimulationTier::Tier2FullEvm);
        mutate(&mut r);
        assert_ne!(
            r.canonical_hash(),
            base,
            "{field} does not reach the hash, so two backends could disagree about \
             it and still compare equal"
        );
    }
}

/// A revert's DATA matters, not only its class. Two reverts classified the
/// same with different return data are different outcomes.
#[test]
fn revert_data_participates_not_only_the_class() {
    let mut a = result(SimulationTier::Tier2FullEvm);
    let mut b = a.clone();
    a.revert = Some((RevertClass::MinOutNotMet, vec![1, 2, 3]));
    b.revert = Some((RevertClass::MinOutNotMet, vec![1, 2, 4]));
    assert_ne!(a.canonical_hash(), b.canonical_hash());
}

/// `None` and an empty payload are different facts, and the tagged encoding
/// keeps them apart. A flattened encoding would collapse them.
#[test]
fn absent_and_empty_are_distinguishable() {
    let mut none = result(SimulationTier::Tier2FullEvm);
    let mut empty = none.clone();
    none.revert = None;
    empty.revert = Some((RevertClass::Unknown, Vec::new()));
    assert_ne!(none.canonical_hash(), empty.canonical_hash());

    let mut no_preconf = result(SimulationTier::Tier2FullEvm);
    let mut zero_preconf = no_preconf.clone();
    no_preconf.simulated_at_state.preconf_sequence = None;
    zero_preconf.simulated_at_state.preconf_sequence = Some(0);
    assert_ne!(no_preconf.canonical_hash(), zero_preconf.canonical_hash());
}

/// A result whose stored hash does not match its content compares equal to
/// nothing, and the quorum would read that as unanimous disagreement.
#[test]
fn a_result_reports_whether_its_stored_hash_is_honest() {
    let good = result(SimulationTier::Tier2FullEvm);
    assert!(good.hash_is_consistent());

    let mut tampered = good.clone();
    tampered.gas_used += 1;
    assert!(
        !tampered.hash_is_consistent(),
        "a result edited after hashing must not pass as consistent"
    );
}

/// Stable across runs: the same content hashes the same bytes every time, so a
/// hash recorded today can be compared with one computed tomorrow.
#[test]
fn the_hash_is_stable_across_repeated_computation() {
    let r = result(SimulationTier::Tier2FullEvm);
    let first = r.canonical_hash();
    for _ in 0..100 {
        assert_eq!(r.canonical_hash(), first);
    }
}
