//! **`startV2`'s calldata, byte for byte against the compiler.**
//!
//! The signer signs these bytes, so an encoder that is one word off does not
//! fail — it signs a different plan. `fixtures/start_v2_calldata.json` holds
//! what Solidity's own `abi.encodeCall(MultiVenueArbImplementation.startV2, …)`
//! produces for three plans; `test/StartV2Calldata.t.sol` asserts it is still
//! the compiler's encoding and decodes it field by field; this asserts the Rust
//! encoder reproduces every byte. Neither side writes the file.
//!
//! The three plans are the ones `StartV2CalldataTest::_plan` builds, field for
//! field — change one and not the other and this fails by case name.

use alloy_primitives::{address, hex, Address, B256, U256};
use apex_exec::commitment::{Loan, LoanProvider, Op, PlanV2, Step};
use apex_exec::encode::{start_v2_calldata, START_V2_SELECTOR};

const TOKEN_A: Address = address!("1111111111111111111111111111111111111111");
const TOKEN_B: Address = address!("2222222222222222222222222222222222222222");
const LENDER_A: Address = address!("3333333333333333333333333333333333333333");
const LENDER_B: Address = address!("4444444444444444444444444444444444444444");

fn fixture() -> serde_json::Value {
    let text = std::fs::read_to_string(format!(
        "{}/tests/fixtures/start_v2_calldata.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("read the fixture");
    serde_json::from_str(&text).expect("the fixture is JSON")
}

fn word(byte: u8) -> B256 {
    B256::repeat_byte(byte)
}

/// `(name, plan, commitment)`, mirroring `StartV2CalldataTest::_plan`.
fn cases() -> Vec<(&'static str, PlanV2, B256)> {
    let empty = PlanV2 {
        loans: Vec::new(),
        cycle_slippage_bps: 0,
        steps: Vec::new(),
        min_profit: U256::from(1u64),
        declared_residue: U256::ZERO,
        chain_id: 8453,
        deadline: 1_781_049_614,
    };

    let one_loan_two_steps = PlanV2 {
        loans: vec![Loan {
            token: TOKEN_A,
            amount: U256::from(1_000_000_000_000_000_000u128),
            provider: LoanProvider::Balancer,
            provider_addr: LENDER_A,
        }],
        cycle_slippage_bps: 30,
        steps: vec![
            Step { op: Op::UniV3, data: vec![0xaa, 0xbb] },
            Step { op: Op::Generic, data: vec![0xcc, 0xdd, 0xee] },
        ],
        min_profit: U256::from(1_234_567u64),
        declared_residue: U256::ZERO,
        chain_id: 8453,
        deadline: 1_781_049_614,
    };

    let two_loans_three_steps = PlanV2 {
        loans: vec![
            Loan {
                token: TOKEN_A,
                amount: U256::from(5u64),
                provider: LoanProvider::Aave,
                provider_addr: LENDER_A,
            },
            Loan {
                token: TOKEN_B,
                amount: U256::MAX,
                provider: LoanProvider::UniV3,
                provider_addr: LENDER_B,
            },
        ],
        cycle_slippage_bps: u16::MAX,
        steps: vec![
            Step { op: Op::Balancer, data: vec![0x5a; 33] },
            Step { op: Op::UniV3, data: vec![0x6b; 32] },
            Step { op: Op::Generic, data: Vec::new() },
        ],
        min_profit: U256::MAX,
        declared_residue: U256::from(7u64),
        chain_id: u64::MAX,
        deadline: u64::MAX,
    };

    let mut coffee = [0u8; 32];
    coffee[29..].copy_from_slice(&[0xc0, 0xff, 0xee]);
    vec![
        ("empty_lists", empty, B256::from(coffee)),
        ("one_loan_two_steps", one_loan_two_steps, word(0xab)),
        ("two_loans_three_steps", two_loans_three_steps, word(0xfe)),
    ]
}

#[test]
fn the_selector_is_the_contracts() {
    let recorded = fixture()["selector"].as_str().unwrap().to_string();
    assert_eq!(format!("0x{}", hex::encode(START_V2_SELECTOR)), recorded);
}

/// **The differential.** Every byte of every case.
#[test]
fn start_v2_calldata_matches_the_compiler() {
    let f = fixture();
    for (name, plan, commitment) in cases() {
        let ours = format!("0x{}", hex::encode(start_v2_calldata(&plan, commitment)));
        let theirs = f["cases"][name].as_str().unwrap_or_else(|| panic!("no case {name}"));
        if ours != theirs {
            // Point at the first word that differs: an offset bug is usually one
            // word, and a 1,000-byte diff hides it.
            let (a, b) = (&ours[10..], &theirs[10..]);
            let first = a
                .as_bytes()
                .chunks(64)
                .zip(b.as_bytes().chunks(64))
                .position(|(x, y)| x != y);
            panic!("{name}: first differing word after the selector is {first:?}\n ours={ours}\ntheirs={theirs}");
        }
    }
}

/// Every case the fixture holds is one this test builds, so a case added on the
/// Solidity side cannot sit there unchecked.
#[test]
fn the_fixture_holds_no_case_this_test_skips() {
    let f = fixture();
    let recorded: Vec<&String> = f["cases"].as_object().unwrap().keys().collect();
    let built: Vec<&str> = cases().iter().map(|(n, _, _)| *n).collect();
    assert_eq!(recorded.len(), built.len(), "fixture {recorded:?}, built {built:?}");
    for n in recorded {
        assert!(built.contains(&n.as_str()), "fixture case {n} is not built here");
    }
}
