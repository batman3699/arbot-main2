//! Task 8.5 R37: a v4 pool read into the book from PoolManager's storage. A
//! scripted node answers `extsload(bytes32[])` from a slot map, Multicall3 for
//! balances and decimals, and `eth_getCode` for the hook.

use alloy_primitives::{address, hex, keccak256, Address, B256, U256};
use apex_chain::rpc::{RpcError, RpcTransport};
use apex_math::cl_math::get_sqrt_ratio_at_tick;
use apex_runtime::live::abi::{self, selector, MULTICALL3};
use apex_runtime::live::book::{PoolBook, Unloadable};
use apex_runtime::live::inventory::{PoolSpec, Venue};
use apex_runtime::live::reads::ChainReads;
use apex_runtime::live::v4::{
    bitmap_slot, key_address, liquidity_slot, pool_id_of, state_slot, tick_slot, HookAllowlist, V4Key, POOL_MANAGER,
};
use apex_types::compat::u256_to_alloy;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const AXIS: Address = address!("5be75e3b0aff5d1a042a5363ae36acb1d6d755ea");
const USDC: Address = address!("833589fcd6edb6e08f4c7c32d4f71b54bda02913");
const ALPHA: Address = address!("b0b24b89db0dafbe43c5b40226b63a179f592880");
const HOOK_CODE: [u8; 4] = [0x60, 0x80, 0x60, 0x40];
const L: u128 = 1_400_000_000_000_000_000;
const TICK: i32 = -197_355;
const SPACING: i32 = 10;
const BLOCK: u64 = 53_155_787;

/// `(target, calldata) -> return data`.
type Table = BTreeMap<(Address, Vec<u8>), Vec<u8>>;

/// A node: PoolManager's storage, a Multicall3 table, and one hook's code.
#[derive(Default)]
struct Chain {
    slots: Mutex<BTreeMap<B256, U256>>,
    table: Mutex<Table>,
    hook_code: Mutex<Vec<u8>>,
}

fn word(v: U256) -> Vec<u8> {
    v.to_be_bytes::<32>().to_vec()
}

fn encode_results(results: &[Option<Vec<u8>>]) -> Vec<u8> {
    let w = |v: usize| U256::from(v).to_be_bytes::<32>().to_vec();
    let padded = |n: usize| n.div_ceil(32) * 32;
    let mut out = w(32);
    out.extend(w(results.len()));
    let mut next = results.len() * 32;
    for r in results {
        out.extend(w(next));
        next += 96 + padded(r.as_ref().map_or(0, Vec::len));
    }
    for r in results {
        out.extend(w(usize::from(r.is_some())));
        out.extend(w(64));
        let d = r.clone().unwrap_or_default();
        out.extend(w(d.len()));
        out.extend(&d);
        out.extend(std::iter::repeat_n(0u8, padded(d.len()) - d.len()));
    }
    out
}

fn decode_request(data: &[u8]) -> Vec<(Address, Vec<u8>)> {
    let body = &data[4..];
    let n = U256::from_be_slice(&body[32..64]).to::<usize>();
    (0..n)
        .map(|i| {
            let at = 64 + U256::from_be_slice(&body[64 + i * 32..96 + i * 32]).to::<usize>();
            let target = Address::from_slice(&body[at + 12..at + 32]);
            let len = U256::from_be_slice(&body[at + 96..at + 128]).to::<usize>();
            (target, body[at + 128..at + 128 + len].to_vec())
        })
        .collect()
}

#[async_trait::async_trait]
impl RpcTransport for Chain {
    async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            "eth_getCode" => Ok(json!(format!("0x{}", hex::encode(&*self.hook_code.lock().unwrap())))),
            "eth_call" => {
                let to: Address = params[0]["to"].as_str().unwrap().parse().unwrap();
                let data = hex::decode(params[0]["data"].as_str().unwrap()).unwrap();
                if to == POOL_MANAGER {
                    // extsload(bytes32[]): selector, offset, length, the slots.
                    assert_eq!(data[..4], [0xdb, 0xd0, 0x35, 0xff], "extsload(bytes32[])");
                    let n = U256::from_be_slice(&data[36..68]).to::<usize>();
                    let slots = self.slots.lock().unwrap();
                    let mut out = word(U256::from(32));
                    out.extend(word(U256::from(n)));
                    for i in 0..n {
                        let s = B256::from_slice(&data[68 + 32 * i..100 + 32 * i]);
                        out.extend(word(slots.get(&s).copied().unwrap_or_default()));
                    }
                    return Ok(json!(format!("0x{}", hex::encode(out))));
                }
                assert_eq!(to, MULTICALL3);
                let table = self.table.lock().unwrap();
                let results: Vec<Option<Vec<u8>>> = decode_request(&data).into_iter().map(|k| table.get(&k).cloned()).collect();
                Ok(json!(format!("0x{}", hex::encode(encode_results(&results)))))
            }
            other => panic!("unexpected {other}"),
        }
    }
}

fn key() -> V4Key {
    let mut k = V4Key { pool_id: B256::ZERO, fee: 100, tick_spacing: SPACING, hooks: ALPHA };
    k.pool_id = pool_id_of(AXIS, USDC, &k);
    k
}

fn spec(k: V4Key) -> PoolSpec {
    PoolSpec { pool: key_address(k.pool_id), venue: Venue::UniswapV4, token0: AXIS, token1: USDC, fee_ppm: 100, depth_usd: 1e6, v4: Some(k) }
}

fn slot0(tick: i32, protocol_fee: u32, lp_fee: u32) -> U256 {
    let sqrt = u256_to_alloy(get_sqrt_ratio_at_tick(tick).unwrap());
    let t = U256::from((tick as i64).rem_euclid(1 << 24) as u64);
    sqrt | (t << 160) | (U256::from(protocol_fee) << 184) | (U256::from(lp_fee) << 208)
}

/// Liquidity `L` over [lower, upper], set in the ticks and the bitmap.
fn chain(protocol_fee: u32) -> (Arc<Chain>, V4Key, i32, i32) {
    let k = key();
    let c = Chain::default();
    let (lower, upper) = ((TICK - 2_000).div_euclid(SPACING) * SPACING, (TICK + 2_000).div_euclid(SPACING) * SPACING);
    {
        let mut s = c.slots.lock().unwrap();
        s.insert(state_slot(k.pool_id), slot0(TICK, protocol_fee, 100));
        s.insert(liquidity_slot(k.pool_id), U256::from(L));
        for (t, net) in [(lower, L as i128), (upper, -(L as i128))] {
            let packed = (U256::from(net as u128) << 128) | U256::from(L);
            s.insert(tick_slot(k.pool_id, t), packed);
            let compressed = t.div_euclid(SPACING);
            let (w, bit) = (compressed >> 8, compressed.rem_euclid(256));
            let slot = bitmap_slot(k.pool_id, w as i16);
            let prev = s.get(&slot).copied().unwrap_or_default();
            s.insert(slot, prev | (U256::from(1) << bit as usize));
        }
    }
    {
        let mut t = c.table.lock().unwrap();
        t.insert((AXIS, abi::call_address(selector::BALANCE_OF, POOL_MANAGER)), word(U256::from(10u128.pow(24))));
        t.insert((USDC, abi::call_address(selector::BALANCE_OF, POOL_MANAGER)), word(U256::from(10u128.pow(12))));
        t.insert((AXIS, abi::call0(selector::DECIMALS)), word(U256::from(18)));
        t.insert((USDC, abi::call0(selector::DECIMALS)), word(U256::from(6)));
    }
    *c.hook_code.lock().unwrap() = HOOK_CODE.to_vec();
    (Arc::new(c), k, lower, upper)
}

fn allow() -> HookAllowlist {
    HookAllowlist { hooks: [(ALPHA, keccak256(HOOK_CODE))].into_iter().collect() }
}

async fn load(c: Arc<Chain>, spec: PoolSpec, allow: &HookAllowlist) -> (PoolBook, Vec<apex_runtime::live::book::Unloaded>) {
    let reads = ChainReads::new(c);
    PoolBook::load_with_hooks(&reads, &[spec], BLOCK, allow).await.unwrap()
}

#[tokio::test]
async fn a_v4_pool_loads_its_state_and_ladder() {
    let (c, k, lower, upper) = chain(0);
    let (book, refused) = load(c, spec(k), &allow()).await;
    assert!(refused.is_empty(), "{refused:?}");
    let p = book.get(key_address(k.pool_id)).expect("loaded under its key address");
    assert_eq!(p.state.tick, TICK);
    assert_eq!(p.state.sqrt_price_x96, get_sqrt_ratio_at_tick(TICK).unwrap());
    assert_eq!((p.state.liquidity, p.state.fee_ppm, p.state.tick_spacing), (L, 100, SPACING));
    assert_eq!(p.factory, POOL_MANAGER);
    assert_eq!(p.code_hash, keccak256(HOOK_CODE), "the hook's code");
    assert_eq!(p.decimals, (18, 6));
    assert_eq!(p.state.balance1, Some(ethers_core::types::U256::from(10u128.pow(12))), "PoolManager's holding: an outer cap");
    let ticks: Vec<(i32, i128)> = p.ladder.ticks().to_vec();
    assert!(ticks.contains(&(lower, L as i128)) && ticks.contains(&(upper, -(L as i128))), "{ticks:?}");
}

/// The current tick is negative, so every bitmap word the ladder reads is
/// below zero: read from the wrong slots, they would be empty.
#[tokio::test]
async fn negative_word_positions_read_their_own_slots() {
    let (c, k, lower, _) = chain(0);
    assert!(lower.div_euclid(SPACING) >> 8 < 0);
    let (book, _) = load(c, spec(k), &allow()).await;
    assert!(book.get(key_address(k.pool_id)).unwrap().ladder.ticks().iter().any(|(t, _)| *t == lower));
}

#[tokio::test]
async fn a_v4_pool_whose_protocol_fee_turns_on_is_refused_at_read() {
    let (c, k, _, _) = chain(0x00a00a);
    let (book, refused) = load(c, spec(k), &allow()).await;
    assert!(book.get(key_address(k.pool_id)).is_none());
    assert_eq!(refused[0].why, Unloadable::V4Refused { why: "protocol fee" });
}

#[tokio::test]
async fn a_hook_whose_code_changed_is_refused() {
    let (c, k, _, _) = chain(0);
    *c.hook_code.lock().unwrap() = vec![0xfe];
    let (_, refused) = load(c, spec(k), &allow()).await;
    assert_eq!(refused[0].why, Unloadable::V4Refused { why: "hook code changed" });
}

/// The inventory is not trusted: a key that does not hash to its pool ID is
/// another pool's, or none.
#[tokio::test]
async fn a_record_whose_key_does_not_hash_to_its_id_is_refused() {
    let (c, mut k, _, _) = chain(0);
    k.fee = 500;
    let (_, refused) = load(c, spec(k), &allow()).await;
    assert_eq!(refused[0].why, Unloadable::V4Refused { why: "pool id is not its key's" });
}
