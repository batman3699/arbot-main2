//! Judging an arm against what the chain did (R26): when its predicates first
//! held, which transaction turned them false again (the rival), where each
//! landed, and what each bid would have netted.
//!
//! The words are rebuilt exactly: a Swap carries its pool's post-swap price,
//! tick and liquidity, a Sync its reserves, and a Mint or Burn moves the
//! in-range liquidity by its amount when the pool's tick is inside its range.

use super::lines::{Arm, ArmedLine};
use super::predicate::{Op, Predicate};
use ethers_core::types::U256;
use std::collections::BTreeMap;

/// The bid fractions swept, per mille.
pub const F_PERMILLE: [u32; 7] = [50, 100, 200, 300, 500, 700, 900];
/// Base's last flashblock index in a block; index 0 holds the block's system
/// transactions, so a block's first user flashblock is 1.
const LAST_INDEX: u32 = 10;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Write {
    Set { address: String, slot: u64, value: U256 },
    Tick { address: String, tick: i32 },
    Liquidity { address: String, slot: u64, lower: i32, upper: i32, delta: i128 },
}

/// One transaction's writes to the watched pools, in chain order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxEffect {
    pub block: u64,
    pub tx_index: u64,
    pub tx: String,
    pub writes: Vec<Write>,
}

#[derive(Clone, Debug, Default)]
pub struct Words {
    values: BTreeMap<(String, u64), U256>,
    ticks: BTreeMap<String, i32>,
}

fn parse_hex(s: &str) -> Option<U256> {
    U256::from_str_radix(s.trim_start_matches("0x"), 16).ok()
}

fn parse_slot(s: &str) -> Option<u64> {
    u64::from_str_radix(s.trim_start_matches("0x"), 16).ok()
}

impl Words {
    pub fn from_arm(arm: &Arm) -> Option<Self> {
        let mut w = Self::default();
        for word in &arm.start_words {
            w.values.insert((word.address.clone(), parse_slot(&word.slot)?), parse_hex(&word.value)?);
        }
        w.ticks = arm.start_ticks.iter().cloned().collect();
        Some(w)
    }

    pub fn apply(&mut self, write: &Write) {
        match write {
            Write::Set { address, slot, value } => {
                self.values.insert((address.clone(), *slot), *value);
            }
            Write::Tick { address, tick } => {
                self.ticks.insert(address.clone(), *tick);
            }
            Write::Liquidity { address, slot, lower, upper, delta } => {
                let in_range = self.ticks.get(address).is_some_and(|t| lower <= t && t < upper);
                if in_range {
                    if let Some(v) = self.values.get_mut(&(address.clone(), *slot)) {
                        let d = U256::from(delta.unsigned_abs());
                        *v = if *delta >= 0 { v.saturating_add(d) } else { v.saturating_sub(d) };
                    }
                }
            }
        }
    }

    /// Whether every predicate holds in `block`. A word never seen fails.
    pub fn holds(&self, predicates: &[Predicate], block: u64) -> bool {
        predicates.iter().all(|p| match p {
            Predicate::Storage { address, slot, mask, op, value } => {
                let (Some(slot), Some(want)) = (parse_slot(slot), parse_hex(value)) else { return false };
                let Some(word) = self.values.get(&(address.clone(), slot)) else { return false };
                let masked = match mask.as_deref().and_then(parse_hex) {
                    Some(m) => *word & m,
                    None => *word,
                };
                match op {
                    Op::AtLeast => masked >= want,
                    Op::AtMost => masked <= want,
                }
            }
            Predicate::BlockNumber { op, value } => {
                let want = parse_hex(value).map_or(0, |v| v.low_u64());
                match op {
                    Op::AtLeast => block >= want,
                    Op::AtMost => block <= want,
                }
            }
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Point {
    pub block: u64,
    pub tx_index: u64,
    pub tx: String,
}

impl Point {
    fn of(e: &TxEffect) -> Self {
        Self { block: e.block, tx_index: e.tx_index, tx: e.tx.clone() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Judged {
    pub fire: Option<Point>,
    pub rival: Option<Point>,
    /// The trigger pool's predicates held, alone, after some transaction in
    /// the window. An arm that never fired then missed on its band.
    pub trigger_met: bool,
}

/// The fire point (the first transaction after which every predicate holds)
/// and the rival (the first after that which makes one false). `effects`
/// must hold every transaction that touched the arm's pools, in chain order.
pub fn judge(arm: &Arm, effects: &[TxEffect]) -> Option<Judged> {
    let mut words = Words::from_arm(arm)?;
    let trigger_pool = arm.pools.get(usize::from(arm.trigger_leg))?;
    let trigger: Vec<Predicate> = arm
        .predicates
        .iter()
        .filter(|p| matches!(p, Predicate::Storage { address, .. } if address == trigger_pool))
        .cloned()
        .collect();
    let mut fire = None;
    let mut trigger_met = false;
    for e in effects.iter().filter(|e| e.block > arm.armed_block && e.block <= arm.to_block) {
        for w in &e.writes {
            words.apply(w);
        }
        trigger_met |= words.holds(&trigger, e.block);
        let holds = words.holds(&arm.predicates, e.block);
        match (&fire, holds) {
            (None, true) => fire = Some(Point::of(e)),
            (Some(_), false) => return Some(Judged { fire, rival: Some(Point::of(e)), trigger_met }),
            _ => {}
        }
    }
    Some(Judged { fire, rival: None, trigger_met })
}

/// Arms with their renewals folded in: the window and its block bound extended.
pub fn merge_renewals(lines: Vec<ArmedLine>) -> Vec<Arm> {
    let mut arms: BTreeMap<u64, Arm> = BTreeMap::new();
    for line in lines {
        match line {
            ArmedLine::Arm(a) => {
                arms.insert(a.arm_id, *a);
            }
            ArmedLine::Renew { arm_id, to_block, .. } => {
                if let Some(a) = arms.get_mut(&arm_id) {
                    a.to_block = a.to_block.max(to_block);
                    for p in &mut a.predicates {
                        if let Predicate::BlockNumber { value, .. } = p {
                            *value = format!("{:#x}", a.to_block);
                        }
                    }
                }
            }
        }
    }
    arms.into_values().collect()
}

pub fn next_flashblock((block, index): (u64, u32)) -> (u64, u32) {
    if index >= LAST_INDEX {
        (block + 1, 1)
    } else {
        (block, index + 1)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Landing {
    /// We land in the trigger's flashblock.
    pub optimistic: bool,
    /// We land in the next one.
    pub pessimistic: bool,
}

/// Who lands first. A rival in a later flashblock than ours always loses;
/// in ours, the higher priority fee wins.
pub fn landing(fire: (u64, u32), rival: Option<(u64, u32)>, bid: u128, rival_bid: u128) -> Landing {
    let Some(r) = rival else { return Landing { optimistic: true, pessimistic: true } };
    let next = next_flashblock(fire);
    Landing {
        optimistic: r > fire || bid > rival_bid,
        pessimistic: if r < next { false } else if r == next { bid > rival_bid } else { true },
    }
}

/// `f` (per mille) of the surplus at the threshold, spread over the gas.
pub fn bid_wei_per_gas(surplus: U256, gas: u64, f_permille: u32) -> u128 {
    if gas == 0 {
        return 0;
    }
    (surplus * U256::from(f_permille) / U256::from(1_000u32) / U256::from(gas)).low_u128()
}

/// What a landed arm nets in the start token: gross less gas at base fee plus
/// bid, less the L1 and other costs.
pub fn net_wei(gross: U256, gas: u64, base_fee: u128, bid: u128, other_wei: u128) -> i128 {
    let fees = u128::from(gas).saturating_mul(base_fee.saturating_add(bid)).saturating_add(other_wei);
    (gross.low_u128() as i128).saturating_sub(fees as i128)
}
