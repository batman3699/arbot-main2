//! The armed ledger's lines (R26): one per arm, and one per renewal of an arm
//! whose window ended with nothing changed. Written by the engine, read by
//! `apex armed-eval`. Amounts are decimal strings, words 0x-hex.

use super::predicate::Predicate;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ArmedLine {
    Arm(Box<Arm>),
    Renew { arm_id: u64, to_block: u64, at_ns: u64 },
}

/// One storage word an arm watches, as it read when armed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Word {
    pub address: String,
    pub slot: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Hop 1's output at the threshold: hop 2's fixed input.
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
}
