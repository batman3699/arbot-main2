//! Task 8.5 R40.1: `contracts/lean/LeanSwapMath3.sol` is held to
//! `apex_math::cl_math` through `apex-exec/tests/fixtures/swap_steps3.json`:
//! seeded swap steps, sqrt prices at ticks, and integer roots. Rust writes the
//! file (`cargo test -p apex-math --test swap_steps3 -- --ignored`), and this
//! test and `test/lean/LeanSwapMath3.t.sol` both check every case in it.

use apex_math::cl_math::{compute_swap_step, get_sqrt_ratio_at_tick, MAX_TICK, MIN_TICK};
use ethers_core::types::U256;
use serde_json::{json, Value};
use std::path::PathBuf;

fn path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../apex-exec/tests/fixtures/swap_steps3.json")
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.next() % (hi - lo + 1) as u64) as i64
    }
    fn magnitude(&mut self, lo: u32, hi: u32) -> U256 {
        let digits = self.range(i64::from(lo), i64::from(hi)) as usize;
        let mut v = U256::from(self.next() % 9 + 1);
        for _ in 1..digits {
            v = v * 10 + U256::from(self.next() % 10);
        }
        v
    }
}

fn s(v: U256) -> Value {
    Value::String(v.to_string())
}

fn cases() -> Value {
    let mut rng = Rng(0x5EED0F57E95);
    let fees = [1u32, 100, 500, 3_000, 10_000, 100_000];
    let mut steps = Vec::new();
    while steps.len() < 200 {
        let tick = rng.range(-200_000, 200_000) as i32;
        let offset = rng.range(1, 5_000) as i32;
        let down = rng.next().is_multiple_of(2);
        let target = if down { tick - offset } else { tick + offset };
        let (current, target) = (get_sqrt_ratio_at_tick(tick).unwrap(), get_sqrt_ratio_at_tick(target).unwrap());
        let liquidity = rng.magnitude(4, 30).low_u128();
        // A tenth of the cases spend one unit: the step that rounds the most.
        let remaining = if steps.len().is_multiple_of(10) { U256::one() } else { rng.magnitude(1, 32) };
        let fee = fees[(rng.next() % fees.len() as u64) as usize];
        let Some(step) = compute_swap_step(current, target, liquidity, remaining, fee) else { continue };
        steps.push(json!({
            "current": s(current), "target": s(target), "liquidity": s(U256::from(liquidity)),
            "remaining": s(remaining), "fee": fee,
            "next": s(step.sqrt_price_next), "amount_in": s(step.amount_in),
            "amount_out": s(step.amount_out), "fee_amount": s(step.fee_amount),
        }));
    }
    let mut ticks: Vec<i32> = vec![MIN_TICK, MIN_TICK + 1, -1, 0, 1, MAX_TICK - 1, MAX_TICK];
    while ticks.len() < 40 {
        ticks.push(rng.range(i64::from(MIN_TICK), i64::from(MAX_TICK)) as i32);
    }
    let ticks: Vec<Value> =
        ticks.iter().map(|t| json!({"tick": t, "sqrt": s(get_sqrt_ratio_at_tick(*t).unwrap())})).collect();
    let mut xs = vec![U256::zero(), U256::one(), U256::from(2), U256::from(3), U256::from(4), U256::MAX, U256::MAX - 1];
    while xs.len() < 30 {
        xs.push(rng.magnitude(1, 77));
    }
    let roots: Vec<Value> = xs.iter().map(|x| json!({"x": s(*x), "root": s(x.integer_sqrt())})).collect();
    json!({
        "_provenance": "Task 8.5 R40.1. Written by crates/apex-math/tests/swap_steps3.rs from apex_math::cl_math; \
            that test and test/lean/LeanSwapMath3.t.sol check every case. Regenerate only when the Rust is right.",
        "steps": steps, "ticks": ticks, "roots": roots,
    })
}

#[test]
#[ignore = "writes the fixture"]
fn regenerate_swap_steps3() {
    std::fs::write(path(), serde_json::to_string_pretty(&cases()).unwrap() + "\n").unwrap();
}

#[test]
fn the_fixture_is_still_what_rust_computes() {
    let on_disk: Value = serde_json::from_str(&std::fs::read_to_string(path()).unwrap()).unwrap();
    assert_eq!(on_disk, cases());
}
