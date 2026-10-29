//! The armed ledger's lines (R26): one per arm, and one per renewal of an arm
//! whose window ended with nothing changed. Written by the engine, read by
//! `apex armed-eval`. Amounts are decimal strings, words 0x-hex.

use super::predicate::Predicate;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ArmedLine {
    Arm(Box<Arm>),
    /// The arm's window extended. A renewal is a new transaction, re-signed
    /// with a fresh deadline: the arm's own lapses with its first window.
    /// Lines written before R26.1 carry neither.
    Renew {
        arm_id: u64,
        to_block: u64,
        at_ns: u64,
        #[serde(default)]
        deadline: Option<u64>,
        #[serde(default)]
        calldata: Option<String>,
    },
}

/// A renewal folded into its arm by `merge_renewals`: its own transaction,
/// valid in the blocks after `after_block` (the window before it).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Renewal {
    pub after_block: u64,
    pub to_block: u64,
    pub deadline: u64,
    pub calldata: String,
}

/// One storage word an arm watches, as it read when armed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Word {
    pub address: String,
    pub slot: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Arm {
    pub arm_id: u64,
    pub at_ns: u64,
    pub armed_block: u64,
    pub to_block: u64,
    pub route_hash: String,
    pub trigger_leg: u8,
    pub rung: u8,
    pub step: u32,
    /// The cycle's two pools, in leg order, lowercase 0x.
    pub pools: [String; 2],
    pub venues: [String; 2],
    pub predicates: Vec<Predicate>,
    /// Every watched word's value when armed: the evaluator's starting state.
    pub start_words: Vec<Word>,
    /// The concentrated pools' ticks when armed, for Mint and Burn ranges.
    pub start_ticks: Vec<(String, i32)>,
    pub size_wei: String,
    /// Hop 1's output at the threshold: its minimum. Hop 2's fixed input
    /// under the R26 executor; the lean executor (R29) sells all hop 1 returns.
    pub mid: String,
    pub output_wei: String,
    pub surplus_wei: String,
    pub cost_wei: String,
    pub gas_expected: u64,
    pub gas_limit: u64,
    pub base_fee_wei: String,
    pub other_wei: String,
    pub deadline: u64,
    pub calldata: String,
    pub calldata_hash: String,
    pub sender_slot: u16,
    /// Who lends the size: "balancer" or "morpho" (R27). Lines before R27
    /// borrowed from Balancer.
    #[serde(default = "balancer")]
    pub lender: String,
    /// R36: the priority fee per gas the arm bids, and the share of the
    /// route's measured winners it outbids. Absent from earlier ledgers.
    #[serde(default)]
    pub bid_wei_per_gas: String,
    #[serde(default)]
    pub p_win: f64,
    /// Folded in by `merge_renewals`; never written with the arm.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub renewals: Vec<Renewal>,
    /// R37: the cycle's start token when it is not WETH (USDC): its size,
    /// mid, output, surplus and cost are in that token's units. Absent: WETH.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<alloy_primitives::Address>,
    /// R37: the executor the arm was built for, and the only one it is sent
    /// to. Absent from lines written before R37: the configured v1 executor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor: Option<alloy_primitives::Address>,
    /// Decision 4 (2026-10-29): a USDC arm's rate, the book's USDC units a
    /// wei as `[num, den]` in decimal, which its costs were converted at and
    /// its landing is valued at. Absent for WETH, and before decision 4.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_per_eth: Option<[String; 2]>,
    /// R38: the rule the bid came from: `best_bid`, `best_share`, or
    /// `best_bid (fallback: why)`. Absent before R38.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bid_model: Option<String>,
    /// R38: the other rule's bid and its chance, judged beside the sent one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alt_bid_wei_per_gas: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alt_p_win: Option<f64>,
    /// R38: the census's contest sample the arm was priced with, when loaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_version: Option<String>,
}

fn balancer() -> String {
    "balancer".into()
}

impl Arm {
    /// The calldata that would execute in `block`: the latest renewal's whose
    /// window it falls past, else the arm's own.
    pub fn calldata_at(&self, block: u64) -> &str {
        self.renewals.iter().rev().find(|r| block > r.after_block).map_or(&self.calldata, |r| &r.calldata)
    }
}
