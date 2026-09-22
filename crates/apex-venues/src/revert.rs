//! Revert classification (PLAN.md §10.4, blueprint §8.3, §20).
//!
//! `apex_types::RevertClass` exists because "venue adapters classify reverts
//! rather than returning opaque bytes, so the risk engine can attribute a loss
//! to a class instead of counting undifferentiated failures". The legacy path
//! did the opposite: `sim_revm::decode_revert_reason` turns return data into a
//! `String` — human-readable, and useless to a policy that has to decide
//! whether a venue should keep trading.
//!
//! # What is deliberately NOT classified
//!
//! **Empty return data is not evidence of out-of-gas.** It is what you get
//! from an out-of-gas frame, and also from a bare `revert()`, a call into a
//! non-contract address, and several assembly paths. `RevertClass::OutOfGas`
//! must be concluded from gas accounting — `gas_used` at or near the limit —
//! which is information this function does not have. Guessing here would
//! quietly misattribute a whole class of failures, and the risk engine's
//! response to "we keep running out of gas" is not its response to "this venue
//! keeps rejecting us".
//!
//! Anything not listed below returns `Unknown`, which is a real answer. A
//! classifier that invents a class for an unrecognised selector is worse than
//! one that admits it does not know.

use apex_types::sim::RevertClass;
use once_cell::sync::Lazy;
use std::collections::HashMap;

/// `Error(string)` — the Solidity `require(cond, "msg")` encoding.
const ERROR_STRING: [u8; 4] = [0x08, 0xc3, 0x79, 0xa0];
/// `Panic(uint256)` — assert failures, arithmetic overflow, division by zero.
const PANIC_UINT: [u8; 4] = [0x4e, 0x48, 0x7b, 0x71];

/// Custom errors, by signature, mapped to the class the risk engine acts on.
///
/// Selectors are DERIVED from the signature rather than written as literals,
/// the same discipline `cl_load::cl_state_selectors` uses: a mistyped four-byte
/// literal is invisible, and would silently downgrade a real class to
/// `Unknown`. The signatures themselves are checked against the contract source
/// by a test below.
const CUSTOM_ERRORS: &[(&str, RevertClass)] = &[
    // The executor's own guards (contracts/executor).
    ("SlippageExceeded(uint256,uint256,uint256)", RevertClass::MinOutNotMet),
    ("InsufficientFinalBalance(uint256,uint256)", RevertClass::ProfitInvariantViolated),
    ("InvalidDeadline()", RevertClass::Expired),
    ("NotExecutor()", RevertClass::Unauthorized),
    ("NotOwner()", RevertClass::Unauthorized),
    ("NotConfigAdmin()", RevertClass::Unauthorized),
    ("InvalidRoleAccount()", RevertClass::Unauthorized),
    // Uniswap V4 hooks reject by reverting from the hook frame.
    ("HookCallFailed()", RevertClass::HookRejected),
];

/// `Error(string)` messages, matched exactly.
///
/// Exact match, not `contains`: substring matching turns one venue's message
/// into another venue's class the first time two strings overlap, and these
/// come from contracts nobody here controls.
const ERROR_MESSAGES: &[(&str, RevertClass)] = &[
    // Uniswap V3 SwapRouter.
    ("Too little received", RevertClass::MinOutNotMet),
    ("Too much requested", RevertClass::MinOutNotMet),
    ("Transaction too old", RevertClass::Expired),
    // Uniswap V3 core, three-letter codes.
    ("SPL", RevertClass::InsufficientLiquidity),
    ("STF", RevertClass::TokenTransferFailed),
    // Uniswap V2 and its forks, including Aerodrome's volatile pools.
    ("UniswapV2: INSUFFICIENT_OUTPUT_AMOUNT", RevertClass::MinOutNotMet),
    ("UniswapV2: INSUFFICIENT_LIQUIDITY", RevertClass::InsufficientLiquidity),
    ("UniswapV2: INSUFFICIENT_INPUT_AMOUNT", RevertClass::InsufficientLiquidity),
    ("UniswapV2: K", RevertClass::InsufficientLiquidity),
    ("UniswapV2: EXPIRED", RevertClass::Expired),
    // Solmate / solady transfer helpers.
    ("TRANSFER_FROM_FAILED", RevertClass::TokenTransferFailed),
    ("TRANSFER_FAILED", RevertClass::TokenTransferFailed),
    // Balancer.
    ("BAL#507", RevertClass::MinOutNotMet),
    ("BAL#001", RevertClass::InsufficientLiquidity),
];

static SELECTORS: Lazy<HashMap<[u8; 4], RevertClass>> = Lazy::new(|| {
    CUSTOM_ERRORS
        .iter()
        .map(|(sig, class)| {
            let hash = ethers::utils::id(sig);
            ([hash[0], hash[1], hash[2], hash[3]], *class)
        })
        .collect()
});

/// Classify raw revert return data.
///
/// Never panics, never allocates on the hot path unless the data really is an
/// `Error(string)` that needs decoding.
pub fn classify_revert(data: &[u8]) -> RevertClass {
    if data.len() < 4 {
        // Includes the empty case. See the module docs: empty is not OutOfGas.
        return RevertClass::Unknown;
    }
    let selector = [data[0], data[1], data[2], data[3]];

    if selector == ERROR_STRING {
        return decode_error_string(&data[4..])
            .and_then(|msg| {
                ERROR_MESSAGES
                    .iter()
                    .find(|(known, _)| *known == msg)
                    .map(|(_, class)| *class)
            })
            .unwrap_or(RevertClass::Unknown);
    }

    if selector == PANIC_UINT {
        // A panic code says the arithmetic failed, not why the trade did.
        // Overflow inside a swap is usually thin liquidity, but "usually" is
        // how a taxonomy stops meaning anything.
        return RevertClass::Unknown;
    }

    SELECTORS.get(&selector).copied().unwrap_or(RevertClass::Unknown)
}

fn decode_error_string(payload: &[u8]) -> Option<String> {
    ethers::abi::decode(&[ethers::abi::ParamType::String], payload)
        .ok()?
        .into_iter()
        .next()?
        .into_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error_string(msg: &str) -> Vec<u8> {
        let mut out = ERROR_STRING.to_vec();
        out.extend(ethers::abi::encode(&[ethers::abi::Token::String(msg.to_string())]));
        out
    }

    fn custom(sig: &str) -> Vec<u8> {
        ethers::utils::id(sig)[..4].to_vec()
    }

    #[test]
    fn every_known_message_classifies() {
        for (msg, expected) in ERROR_MESSAGES {
            assert_eq!(
                classify_revert(&error_string(msg)),
                *expected,
                "{msg} must classify as {expected:?}"
            );
        }
    }

    #[test]
    fn every_custom_error_classifies() {
        for (sig, expected) in CUSTOM_ERRORS {
            assert_eq!(classify_revert(&custom(sig)), *expected, "{sig}");
        }
    }

    /// Empty return data is `Unknown`, never `OutOfGas`.
    ///
    /// An out-of-gas frame returns nothing — and so does a bare `revert()`, a
    /// call to an address with no code, and several assembly paths. Deciding
    /// out-of-gas needs the gas accounting, which this function does not see.
    /// The wrong answer here would tell the risk engine to raise gas limits in
    /// response to a venue rejecting us.
    #[test]
    fn empty_return_data_is_not_out_of_gas() {
        assert_eq!(classify_revert(&[]), RevertClass::Unknown);
        assert_eq!(classify_revert(&[0x00]), RevertClass::Unknown);
        assert_eq!(classify_revert(&[0xde, 0xad, 0xbe]), RevertClass::Unknown);
        assert_ne!(classify_revert(&[]), RevertClass::OutOfGas);
    }

    /// An unrecognised message is `Unknown`, not the nearest neighbour.
    #[test]
    fn an_unrecognised_message_is_not_guessed_at() {
        assert_eq!(
            classify_revert(&error_string("Too little received by someone else")),
            RevertClass::Unknown,
            "substring matching would have called this MinOutNotMet"
        );
        assert_eq!(classify_revert(&error_string("SPLINTER")), RevertClass::Unknown);
        assert_eq!(classify_revert(&error_string("")), RevertClass::Unknown);
    }

    /// Panic codes describe the arithmetic, not the trade.
    #[test]
    fn a_solidity_panic_is_not_classified_as_thin_liquidity() {
        let mut data = PANIC_UINT.to_vec();
        data.extend([0u8; 31]);
        data.push(0x11); // arithmetic overflow
        assert_eq!(classify_revert(&data), RevertClass::Unknown);
    }

    /// Selectors are derived, so this pins two of them against independently
    /// known values. A wrong derivation would make every custom error look
    /// `Unknown` and nothing else would notice.
    #[test]
    fn derived_selectors_match_their_signatures() {
        // keccak256("NotOwner()")[..4]
        assert_eq!(custom("NotOwner()"), vec![0x30, 0xcd, 0x74, 0x71]);
        // keccak256("Error(string)")[..4]
        assert_eq!(custom("Error(string)"), ERROR_STRING.to_vec());
        assert_eq!(custom("Panic(uint256)"), PANIC_UINT.to_vec());
    }

    /// The signatures this module claims the executor declares must actually
    /// be declared by it. A renamed error in Solidity would otherwise leave a
    /// dead entry here and every one of those reverts would read `Unknown`.
    #[test]
    fn the_executor_really_declares_these_errors() {
        let root = format!("{}/../../contracts", env!("CARGO_MANIFEST_DIR"));
        let mut sources = String::new();
        collect_sol(std::path::Path::new(&root), &mut sources);
        assert!(!sources.is_empty(), "no Solidity found at {root} -- test is broken");

        for (sig, _) in CUSTOM_ERRORS {
            let name = sig.split('(').next().expect("signature has a name");
            // Uniswap's hook error is declared by their contracts, not ours.
            if name == "HookCallFailed" {
                continue;
            }
            assert!(
                sources.contains(&format!("error {name}(")),
                "{name} is not declared anywhere in contracts/ -- either it was \
                 renamed in Solidity or this entry is dead"
            );
        }
    }

    fn collect_sol(dir: &std::path::Path, out: &mut String) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                collect_sol(&p, out);
            } else if p.extension().is_some_and(|x| x == "sol") {
                if let Ok(text) = std::fs::read_to_string(&p) {
                    out.push_str(&text);
                }
            }
        }
    }
}
