//! Task 8.5 R2 — the live pool book (`apex_runtime::live`).
//!
//! The ABI layer is held to independent references: selectors to the hashes of
//! their signatures, the `aggregate3` encoder to `cast calldata`, and the decoder
//! to a real response recorded from Base. The book itself runs against a scripted
//! Multicall3 node, so its refusals — wrong tokens, wrong factory — and its swap
//! ordering can be driven exactly.

use alloy_primitives::{address, hex, keccak256, Address, U256};
use apex_chain::rpc::{RpcError, RpcTransport};
use apex_runtime::live::abi::{self, selector, MULTICALL3};
use apex_runtime::live::book::{DynamicFee, PoolBook, ReloadError, StateWrite, SwapApplied, SwapWrite, SyncWrite, Unloadable};
use apex_runtime::live::cp::Reserves;
use apex_runtime::live::inventory::{self, PoolSpec, UniverseFilter, Venue};
use apex_runtime::live::reads::ChainReads;
use apex_types::state::ReconstructionStatus;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const WETH_USDC: Address = address!("d0b53D9277642d899DF5C87A3966A349A798F224");
const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const WETH: Address = address!("4200000000000000000000000000000000000006");

fn fixture() -> Value {
    let text = std::fs::read_to_string(format!(
        "{}/tests/fixtures/aggregate3_base.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("fixture");
    serde_json::from_str(&text).unwrap()
}

// ------------------------------------------------------------------ the ABI

#[test]
fn every_selector_is_its_signatures_hash() {
    for (sel, sig) in [
        (selector::AGGREGATE3, "aggregate3((address,bool,bytes)[])"),
        (selector::SLOT0, "slot0()"),
        (selector::LIQUIDITY, "liquidity()"),
        (selector::FEE, "fee()"),
        (selector::TICK_SPACING, "tickSpacing()"),
        (selector::TOKEN0, "token0()"),
        (selector::TOKEN1, "token1()"),
        (selector::FACTORY, "factory()"),
        (selector::DECIMALS, "decimals()"),
        (selector::BALANCE_OF, "balanceOf(address)"),
        (selector::TICK_BITMAP, "tickBitmap(int16)"),
        (selector::TICKS, "ticks(int24)"),
        (selector::OBSERVE, "observe(uint32[])"),
        (selector::SWAP_FEE_MODULE, "swapFeeModule()"),
        (selector::TICK_SPACING_TO_FEE, "tickSpacingToFee(int24)"),
        (selector::DYNAMIC_FEE_CONFIG, "dynamicFeeConfig(address)"),
        (selector::DEFAULT_SCALING_FACTOR, "defaultScalingFactor()"),
        (selector::DEFAULT_FEE_CAP, "defaultFeeCap()"),
        (selector::SECONDS_AGO, "secondsAgo()"),
        (selector::L1_BASE_FEE, "l1BaseFee()"),
        (selector::BLOB_BASE_FEE, "blobBaseFee()"),
        (selector::BASE_FEE_SCALAR, "baseFeeScalar()"),
        (selector::BLOB_BASE_FEE_SCALAR, "blobBaseFeeScalar()"),
        (selector::IS_FJORD, "isFjord()"),
        (selector::ADAPTER_OF, "adapterOf(uint16)"),
        (selector::IS_SELECTOR_ALLOWED, "isSelectorAllowed(uint16,bytes4)"),
        (selector::STABLE, "stable()"),
        (selector::GET_RESERVES, "getReserves()"),
        (selector::GET_FEE, "getFee(address,bool)"),
        (selector::IS_POOL, "isPool(address)"),
    ] {
        assert_eq!(sel, keccak256(sig.as_bytes())[..4], "{sig}");
    }
}

/// Byte-for-byte against `cast calldata` — the independent encoder.
#[test]
fn aggregate3_encoding_matches_cast() {
    let calls = vec![
        (WETH_USDC, abi::call0(selector::SLOT0)),
        (WETH_USDC, abi::call0(selector::LIQUIDITY)),
        (USDC, abi::call_address(selector::BALANCE_OF, WETH_USDC)),
    ];
    let cast = "82ad56cb000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000030000000000000000000000000000000000000000000000000000000000000060000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000001a0000000000000000000000000d0b53d9277642d899df5c87a3966a349a798f2240000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000043850c7bd00000000000000000000000000000000000000000000000000000000000000000000000000000000d0b53d9277642d899df5c87a3966a349a798f2240000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000041a68650200000000000000000000000000000000000000000000000000000000000000000000000000000000833589fcd6edb6e08f4c7c32d4f71b54bda0291300000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000060000000000000000000000000000000000000000000000000000000000000002470a08231000000000000000000000000d0b53d9277642d899df5c87a3966a349a798f22400000000000000000000000000000000000000000000000000000000";
    assert_eq!(hex::encode(abi::encode_aggregate3(&calls)), cast);
}

/// **A real answer from Base.** One `aggregate3` at a pinned block decodes into
/// exactly what each call returned when made on its own at that block — and the
/// fourth, a selector the pool does not have, decodes as a failure rather than
/// as data.
#[test]
fn aggregate3_decoding_reproduces_the_individual_calls() {
    let f = fixture();
    let bytes = hex::decode(f["response"].as_str().unwrap()).unwrap();
    let decoded = abi::decode_aggregate3(&bytes).expect("decodes");
    assert_eq!(decoded.len(), 4);
    let expect = |k: &str| hex::decode(f["expected"][k].as_str().unwrap()).unwrap();
    assert_eq!(decoded[0], (true, expect("slot0")));
    assert_eq!(decoded[1], (true, expect("liquidity")));
    assert_eq!(decoded[2], (true, expect("usdc_balance_of_pool")));
    assert!(!decoded[3].0, "a missing selector is a failure, not an answer");

    // And the words mean what the pool book reads them as.
    let tick = abi::word_int(&decoded[0].1, 1, 24).expect("a tick");
    assert!((-887_272..=887_272).contains(&tick));
    assert!(tick < 0, "WETH/USDC's tick is negative: USDC has 12 fewer decimals");
}

#[test]
fn a_truncated_aggregate3_answer_is_refused_not_shortened() {
    let f = fixture();
    let bytes = hex::decode(f["response"].as_str().unwrap()).unwrap();
    assert!(abi::decode_aggregate3(&bytes[..bytes.len() - 40]).is_none());
}

/// Signed words are sign-extended; a word whose high bytes disagree with its
/// sign is not an `int24`, and an address word with anything in its top twelve
/// bytes is not an address.
#[test]
fn words_are_refused_rather_than_truncated() {
    let neg = abi::call_signed([0; 4], -197_347);
    assert_eq!(abi::word_int(&neg[4..], 0, 24), Some(-197_347));
    let pos = abi::call_signed([0; 4], 197_347);
    assert_eq!(abi::word_int(&pos[4..], 0, 24), Some(197_347));
    let mut bad = neg[4..].to_vec();
    bad[0] = 0x00;
    assert_eq!(abi::word_int(&bad, 0, 24), None, "inconsistent sign extension");
    assert_eq!(abi::word_int(&pos[4..], 0, 8), None, "does not fit int8");

    let mut addr = [0u8; 32];
    addr[12..].copy_from_slice(WETH.as_slice());
    assert_eq!(abi::word_address(&addr, 0), Some(WETH));
    addr[0] = 1;
    assert_eq!(abi::word_address(&addr, 0), None);
}

// ------------------------------------------------------------------ the inventory

fn write_inventory(dir: &std::path::Path, venue: &str, lines: &[Value]) {
    std::fs::create_dir_all(dir.join(venue)).unwrap();
    let text: Vec<String> = lines.iter().map(Value::to_string).collect();
    std::fs::write(dir.join(venue).join("pools.jsonl"), text.join("\n")).unwrap();
}

/// The census's thresholds, and its rule for Slipstream: a record without a
/// measured fee is skipped, because its `fee` field is the tick spacing. The
/// second Slipstream factory's records follow the same rule (R22). PancakeSwap's
/// `fee` is its fee tier, as Uniswap's is, so it stands in.
#[test]
fn the_inventory_applies_the_census_filters() {
    let dir = tempfile::tempdir().unwrap();
    let a = "0x00000000000000000000000000000000000000a1";
    let b = "0x00000000000000000000000000000000000000b2";
    write_inventory(dir.path(), "uniswap_v3", &[
        json!({"pool":"0x0000000000000000000000000000000000000001","token0":a,"token1":b,"fee":500,"hub_usd_liquidity":2e6}),
        json!({"pool":"0x0000000000000000000000000000000000000002","token0":a,"token1":b,"fee":3000,"hub_usd_liquidity":2e6}),
        json!({"pool":"0x0000000000000000000000000000000000000003","token0":a,"token1":b,"fee":100,"hub_usd_liquidity":50_000.0}),
    ]);
    write_inventory(dir.path(), "aerodrome_slipstream", &[
        json!({"pool":"0x0000000000000000000000000000000000000004","token0":a,"token1":b,"fee":1,"fee_ppm_onchain":80,"hub_usd_liquidity":3e5}),
        json!({"pool":"0x0000000000000000000000000000000000000005","token0":a,"token1":b,"fee":100,"hub_usd_liquidity":3e5}),
    ]);
    write_inventory(dir.path(), "pancakeswap_v3", &[
        json!({"pool":"0x0000000000000000000000000000000000000006","token0":a,"token1":b,"fee":100,"hub_usd_liquidity":2e6}),
        json!({"pool":"0x0000000000000000000000000000000000000007","token0":a,"token1":b,"fee":2500,"hub_usd_liquidity":2e6}),
    ]);
    write_inventory(dir.path(), "aerodrome_slipstream_v3", &[
        json!({"pool":"0x0000000000000000000000000000000000000008","token0":a,"token1":b,"fee":50,"fee_ppm_onchain":425,"hub_usd_liquidity":3e6}),
        json!({"pool":"0x0000000000000000000000000000000000000009","token0":a,"token1":b,"fee":200,"hub_usd_liquidity":3e6}),
    ]);
    write_inventory(dir.path(), "aerodrome_v2", &[]);
    let specs = inventory::load(dir.path(), UniverseFilter::default()).unwrap();
    let pools: Vec<u8> = specs.iter().map(|s| s.pool.as_slice()[19]).collect();
    assert_eq!(
        pools,
        vec![1, 4, 6, 8],
        "500ppm uni, the measured slipstreams of both factories and the 100ppm pancake only"
    );
    assert_eq!(specs[1].fee_ppm, 80, "the measured fee, not the tick spacing");
    assert_eq!((specs[2].venue, specs[2].fee_ppm), (Venue::PancakeV3, 100), "the fee tier stands in");
    assert_eq!((specs[3].venue, specs[3].fee_ppm), (Venue::SlipstreamV3, 425), "measured, as the first factory's");

    let pairs = inventory::pairs(&specs);
    assert_eq!(pairs.len(), 1, "one pair, four pools");
}

/// A venue's inventory that is missing is an error, not an empty venue: a
/// universe quietly a venue short looks just like a quiet market.
#[test]
fn a_missing_inventory_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    write_inventory(dir.path(), "uniswap_v3", &[]);
    write_inventory(dir.path(), "aerodrome_slipstream", &[]);
    assert!(matches!(inventory::load(dir.path(), UniverseFilter::default()), Err(inventory::InventoryError::Io { .. })));
}

// ------------------------------------------------------------------ the book

/// `(target, calldata) -> return data`.
type Table = BTreeMap<(Address, Vec<u8>), Vec<u8>>;

/// Something that happens while a read is in flight.
type DuringRead = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

/// A Multicall3 node that answers each sub-call from a table, and reports a
/// failure for anything it does not know. Also answers `eth_getCode`.
#[derive(Default)]
struct Scripted {
    answers: Mutex<Table>,
    calls: Mutex<usize>,
    /// Run once, inside the next `eth_call`, before it is answered.
    during_read: Mutex<Option<DuringRead>>,
}

impl Scripted {
    fn set(&self, to: Address, data: Vec<u8>, answer: Vec<u8>) {
        self.answers.lock().unwrap().insert((to, data), answer);
    }
}

fn encode_results(results: &[Option<Vec<u8>>]) -> Vec<u8> {
    let word = |v: usize| U256::from(v).to_be_bytes::<32>().to_vec();
    let padded = |n: usize| n.div_ceil(32) * 32;
    let mut out = word(32);
    out.extend(word(results.len()));
    let mut next = results.len() * 32;
    for r in results {
        out.extend(word(next));
        next += 96 + padded(r.as_ref().map_or(0, Vec::len));
    }
    for r in results {
        out.extend(word(usize::from(r.is_some())));
        out.extend(word(64));
        let d = r.clone().unwrap_or_default();
        out.extend(word(d.len()));
        out.extend(&d);
        out.extend(std::iter::repeat_n(0u8, padded(d.len()) - d.len()));
    }
    out
}

fn decode_request(data: &[u8]) -> Vec<(Address, Vec<u8>)> {
    // Our own encoder's layout: selector, offset, len, offsets, tuples.
    let body = &data[4..];
    let n = U256::from_be_slice(&body[32..64]).to::<usize>();
    let base = 64;
    (0..n)
        .map(|i| {
            let at = base + U256::from_be_slice(&body[base + i * 32..base + i * 32 + 32]).to::<usize>();
            let target = Address::from_slice(&body[at + 12..at + 32]);
            let len = U256::from_be_slice(&body[at + 96..at + 128]).to::<usize>();
            (target, body[at + 128..at + 128 + len].to_vec())
        })
        .collect()
}

#[async_trait::async_trait]
impl RpcTransport for Scripted {
    async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        *self.calls.lock().unwrap() += 1;
        match method {
            "eth_getCode" => Ok(json!("0x6080")),
            "eth_call" => {
                let during = self.during_read.lock().unwrap().take();
                if let Some(f) = during {
                    f.await;
                }
                assert_eq!(params[0]["to"].as_str().unwrap().parse::<Address>().unwrap(), MULTICALL3);
                let data = hex::decode(params[0]["data"].as_str().unwrap()).unwrap();
                let answers = self.answers.lock().unwrap();
                let results: Vec<Option<Vec<u8>>> = decode_request(&data)
                    .into_iter()
                    .map(|k| answers.get(&k).cloned())
                    .collect();
                Ok(json!(format!("0x{}", hex::encode(encode_results(&results)))))
            }
            other => panic!("unexpected {other}"),
        }
    }
}

fn w(v: U256) -> Vec<u8> {
    v.to_be_bytes::<32>().to_vec()
}
fn wa(a: Address) -> Vec<u8> {
    let mut out = vec![0u8; 12];
    out.extend_from_slice(a.as_slice());
    out
}
fn wi(v: i64) -> Vec<u8> {
    abi::call_signed([0; 4], v)[4..].to_vec()
}

fn spec() -> PoolSpec {
    PoolSpec {
        pool: WETH_USDC,
        venue: Venue::UniswapV3,
        token0: WETH,
        token1: USDC,
        fee_ppm: 500,
        depth_usd: 5_800_000.0,
    }
}

const TICK: i64 = -197_347;
const SPACING: i64 = 10;

/// A healthy WETH/USDC pool, with two initialized ticks either side of the price.
fn healthy(node: &Scripted, factory: Address) {
    healthy_at(node, WETH_USDC, factory);
}

fn healthy_at(node: &Scripted, p: Address, factory: Address) {
    node.set(p, abi::call0(selector::TOKEN0), wa(WETH));
    node.set(p, abi::call0(selector::TOKEN1), wa(USDC));
    node.set(p, abi::call0(selector::FACTORY), wa(factory));
    node.set(p, abi::call0(selector::FEE), w(U256::from(500)));
    node.set(p, abi::call0(selector::TICK_SPACING), wi(SPACING));
    let mut slot0 = w(U256::from(4_109_375_649_317_904_751_454_295u128));
    slot0.extend(wi(TICK));
    node.set(p, abi::call0(selector::SLOT0), slot0);
    node.set(p, abi::call0(selector::LIQUIDITY), w(U256::from(1_405_406_481_779_019_804u128)));
    node.set(WETH, abi::call_address(selector::BALANCE_OF, p), w(U256::from(10u128.pow(21))));
    node.set(USDC, abi::call_address(selector::BALANCE_OF, p), w(U256::from(4_633_841_978_376u128)));
    node.set(WETH, abi::call0(selector::DECIMALS), w(U256::from(18)));
    node.set(USDC, abi::call0(selector::DECIMALS), w(U256::from(6)));
    // Bitmap: the word holding the current tick has bits for -197350 and -197340.
    let compressed = (TICK as f64 / SPACING as f64).floor() as i64;
    let word = compressed >> 8;
    for wp in (word - 2)..=(word + 2) {
        let mut bits = U256::ZERO;
        if wp == word {
            for t in [-197_350i64, -197_340] {
                let c = t / SPACING;
                bits |= U256::from(1) << ((c & 0xff) as usize);
            }
        }
        node.set(p, abi::call_signed(selector::TICK_BITMAP, wp), w(bits));
    }
    for t in [-197_350i64, -197_340] {
        let mut ticks = w(U256::from(1_000u64)); // liquidityGross
        ticks.extend(wi(if t < TICK { 5_000 } else { -5_000 }));
        node.set(p, abi::call_signed(selector::TICKS, t), ticks);
    }
}

async fn book(node: Arc<Scripted>) -> (PoolBook, Vec<apex_runtime::live::book::Unloaded>) {
    PoolBook::load(&ChainReads::new(node), &[spec()], 100).await.expect("reads")
}

#[tokio::test]
async fn a_healthy_pool_loads_with_its_ladder() {
    let node = Arc::new(Scripted::default());
    healthy(&node, Venue::UniswapV3.factory());
    let (book, refused) = book(node).await;
    assert!(refused.is_empty(), "{refused:?}");
    let p = book.get(WETH_USDC).expect("loaded");
    assert_eq!(p.state.tick, TICK as i32);
    assert_eq!(p.state.tick_spacing, SPACING as i32);
    assert_eq!(p.decimals, (18, 6));
    assert_eq!(p.ladder.len(), 2, "both initialized ticks");
    assert!(p.ladder_covers_price());
    assert_eq!(book.status(), ReconstructionStatus::Verified);
}

/// The inventory is not trusted: a pool whose chain tokens differ is refused.
#[tokio::test]
async fn a_pool_whose_tokens_disagree_with_the_inventory_is_refused() {
    let node = Arc::new(Scripted::default());
    healthy(&node, Venue::UniswapV3.factory());
    node.set(WETH_USDC, abi::call0(selector::TOKEN1), wa(address!("00000000000000000000000000000000000000ff")));
    let (book, refused) = book(node).await;
    assert!(book.is_empty());
    assert!(matches!(refused[0].why, Unloadable::WrongTokens { .. }), "{refused:?}");
}

/// A pool deployed by another factory is not this venue's, whatever the
/// inventory filed it under — B-7's and the 33-pool misfiling's shared shape.
#[tokio::test]
async fn a_pool_from_another_factory_is_refused() {
    let node = Arc::new(Scripted::default());
    healthy(&node, Venue::Slipstream.factory());
    let (book, refused) = book(node).await;
    assert!(book.is_empty());
    assert!(matches!(refused[0].why, Unloadable::WrongFactory { .. }), "{refused:?}");
}

#[tokio::test]
async fn a_pool_with_a_failed_read_is_refused_by_name() {
    let node = Arc::new(Scripted::default());
    healthy(&node, Venue::UniswapV3.factory());
    node.answers.lock().unwrap().remove(&(WETH_USDC, abi::call0(selector::SLOT0)));
    let (_, refused) = book(node).await;
    assert_eq!(refused[0].why, Unloadable::Unreadable { read: "slot0" });
}

/// A swap sets the post-swap state exactly; an older one is refused; one that
/// moves the price off the ladder asks for a reload.
#[tokio::test]
async fn swaps_apply_in_order_and_a_price_off_the_ladder_asks_for_a_reload() {
    let node = Arc::new(Scripted::default());
    healthy(&node, Venue::UniswapV3.factory());
    let (book, _) = book(node).await;
    let price = U256::from(4_100_000_000_000_000_000_000_000u128);

    assert_eq!(book.apply_swap(WETH_USDC, price, 7, -197_348, (101, 5)), SwapApplied::Updated);
    assert_eq!(book.get(WETH_USDC).unwrap().state.tick, -197_348);
    assert_eq!(book.get(WETH_USDC).unwrap().state.liquidity, 7);

    // An older log -- the confirmed copy arriving after a later preconfirmed one.
    assert_eq!(book.apply_swap(WETH_USDC, price, 9, -197_300, (101, 4)), SwapApplied::Stale);
    assert_eq!(book.apply_swap(WETH_USDC, price, 9, -197_300, (100, 9)), SwapApplied::Stale);
    assert_eq!(book.get(WETH_USDC).unwrap().state.liquidity, 7, "rolled back");

    // Far off the proven range.
    assert_eq!(book.apply_swap(WETH_USDC, price, 7, -150_000, (102, 0)), SwapApplied::NeedsReload);
    assert!(!book.get(WETH_USDC).unwrap().ladder_covers_price());

    assert_eq!(
        book.apply_swap(address!("0000000000000000000000000000000000000001"), price, 1, 0, (103, 0)),
        SwapApplied::Unknown
    );
}

/// A reader holding a snapshot keeps its view while writes land (INV-11).
#[tokio::test]
async fn a_snapshot_is_stable_while_swaps_land() {
    let node = Arc::new(Scripted::default());
    healthy(&node, Venue::UniswapV3.factory());
    let (book, _) = book(node).await;
    let before = book.snapshot();
    book.apply_swap(WETH_USDC, U256::from(1u64) << 96, 42, -197_346, (101, 0));
    assert_eq!(before[&WETH_USDC].state.tick, TICK as i32, "the held snapshot moved");
    assert_eq!(book.get(WETH_USDC).unwrap().state.tick, -197_346);
}

/// **R17: a burst of swaps is one write.** A reader sees the book before all of
/// them or after all of them, and each is judged newer or not against the
/// swaps before it in the burst.
#[tokio::test]
async fn a_burst_of_swaps_is_one_write() {
    const OTHER: Address = address!("b4cB800910B228ED3d0834cF79D697127BBB00e5");
    let node = Arc::new(Scripted::default());
    healthy(&node, Venue::UniswapV3.factory());
    healthy_at(&node, OTHER, Venue::UniswapV3.factory());
    let other = PoolSpec { pool: OTHER, ..spec() };
    let (book, refused) = PoolBook::load(&ChainReads::new(node), &[spec(), other], 100).await.unwrap();
    assert!(refused.is_empty(), "{refused:?}");
    let before = book.snapshot();
    let price = U256::from(4_100_000_000_000_000_000_000_000u128);
    let swap = |pool, tick, at| SwapWrite { pool, sqrt_price_x96: price, liquidity: 7, tick, at };

    let applied = book.apply_swaps(&[
        swap(WETH_USDC, -197_348, (101, 5)),
        swap(OTHER, -197_349, (101, 6)),
        swap(WETH_USDC, -197_352, (101, 8)),
        // Older than the burst's own swap before it.
        swap(WETH_USDC, -197_300, (101, 7)),
    ]);
    use SwapApplied::{Stale, Updated};
    assert_eq!(applied, vec![Updated, Updated, Updated, Stale]);
    let (a, b) = (book.get(WETH_USDC).unwrap(), book.get(OTHER).unwrap());
    assert_eq!((a.state.tick, b.state.tick), (-197_352, -197_349));
    assert_eq!(a.seq, b.seq, "one write");
    assert!(a.seq > before[&WETH_USDC].seq);
    assert_eq!(before[&WETH_USDC].state.tick, TICK as i32, "a held snapshot saw none of it");
    assert_eq!(before[&OTHER].state.tick, TICK as i32);
}

/// A feed gap makes the book `Rebuilding`; only a full reload makes it
/// `Verified` again (§5.6, INV-08).
#[tokio::test]
async fn a_gap_is_rebuilding_until_a_full_reload() {
    let node = Arc::new(Scripted::default());
    healthy(&node, Venue::UniswapV3.factory());
    let reads = ChainReads::new(node.clone());
    let (book, _) = PoolBook::load(&reads, &[spec()], 100).await.unwrap();
    book.apply_swap(WETH_USDC, U256::from(1u64) << 96, 42, -150_000, (101, 0));

    book.mark_rebuilding();
    assert_eq!(book.status(), ReconstructionStatus::Rebuilding);
    book.apply_swap(WETH_USDC, U256::from(1u64) << 96, 42, -150_000, (102, 0));
    assert_eq!(book.status(), ReconstructionStatus::Rebuilding, "a swap ended the gap");
    book.reload(&reads, &[WETH_USDC], 120).await.unwrap();
    assert_eq!(book.status(), ReconstructionStatus::Rebuilding, "a partial reload is not a full one");
    book.reload(&reads, &[], 121).await.unwrap();
    assert_eq!(book.status(), ReconstructionStatus::Verified);
    let p = book.get(WETH_USDC).unwrap();
    assert_eq!(p.state.tick, TICK as i32, "re-read from the chain");
    assert_eq!(p.block, 121);
    assert!(p.ladder_covers_price());
}

/// A 128-bit signed word is all of `i128`. Computing its range the way narrower
/// widths do shifts a 1 into the sign bit and overflows — found when swap
/// amounts, the first 128-bit signed words decoded, reached it.
#[test]
fn a_full_width_signed_word_decodes_at_both_extremes() {
    for v in [i128::MIN, -1, 0, 1, i128::MAX] {
        let mut w = if v < 0 { [0xffu8; 32] } else { [0u8; 32] };
        w[16..].copy_from_slice(&v.to_be_bytes());
        assert_eq!(abi::word_int(&w, 0, 128), Some(v));
    }
}

/// A read at block `n` is the state after every log in `n`. The confirmed copy
/// of a swap from that block, arriving after the read, is already in it — and
/// applying it would roll the pool back to the middle of the block.
#[tokio::test]
async fn a_log_from_the_block_a_pool_was_read_at_is_already_in_it() {
    let node = Arc::new(Scripted::default());
    healthy(&node, Venue::UniswapV3.factory());
    let (book, _) = book(node).await; // read at block 100
    let price = U256::from(4_100_000_000_000_000_000_000_000u128);

    assert_eq!(book.apply_swap(WETH_USDC, price, 7, -197_348, (100, 9)), SwapApplied::Stale);
    assert_eq!(book.get(WETH_USDC).unwrap().state.tick, TICK as i32, "rolled back into block 100");
    assert_eq!(book.apply_swap(WETH_USDC, price, 7, -197_348, (101, 0)), SwapApplied::Updated);
}

/// **A read never rolls back a swap already applied.** A reload reads at the
/// latest sealed block; a preconfirmed swap from the block after keeps its
/// price and position, and the pool takes the fresh read's ladder and balances.
#[tokio::test]
async fn a_reload_keeps_a_newer_swap_and_takes_the_fresh_read() {
    let node = Arc::new(Scripted::default());
    healthy(&node, Venue::UniswapV3.factory());
    let reads = ChainReads::new(node.clone());
    let (book, _) = PoolBook::load(&reads, &[spec()], 100).await.unwrap();
    let price = U256::from(4_100_000_000_000_000_000_000_000u128);

    // Preconfirmed, in block 121.
    assert_eq!(book.apply_swap(WETH_USDC, price, 7, -197_348, (121, 3)), SwapApplied::Updated);
    // By block 120 the pool's WETH holding had doubled.
    node.set(WETH, abi::call_address(selector::BALANCE_OF, WETH_USDC), w(U256::from(2 * 10u128.pow(21))));
    let before = book.versions_for(&[WETH_USDC]);

    book.reload(&reads, &[WETH_USDC], 120).await.unwrap();
    let p = book.get(WETH_USDC).unwrap();
    assert_eq!((p.state.tick, p.state.liquidity, p.last_log), (-197_348, 7, Some((121, 3))), "the swap was rolled back");
    assert_eq!(p.state.balance0, Some(ethers_core::types::U256::from(2 * 10u128.pow(21))), "the fresh read was not taken");
    // Moved, and forward: a version that went back could fall behind another
    // pool's on the same route and stop moving it.
    let newest = |m: BTreeMap<apex_types::ids::VenueId, u64>| m.into_values().max().unwrap();
    assert!(newest(book.versions_for(&[WETH_USDC])) > newest(before), "the ladder and balances changed");

    // Its confirmed copy is still not newer.
    assert_eq!(book.apply_swap(WETH_USDC, price, 7, -197_348, (121, 3)), SwapApplied::Stale);
}

/// A gap marked **while** a full reload is reading keeps the book `Rebuilding`:
/// the logs it dropped may postdate the read (INV-08). A full reload that begins
/// after the gap clears it.
#[tokio::test]
async fn a_gap_during_a_full_reload_keeps_the_book_rebuilding() {
    let node = Arc::new(Scripted::default());
    healthy(&node, Venue::UniswapV3.factory());
    let reads = ChainReads::new(node.clone());
    let book = Arc::new(PoolBook::load(&reads, &[spec()], 100).await.unwrap().0);

    let b = Arc::clone(&book);
    *node.during_read.lock().unwrap() = Some(Box::pin(async move { b.mark_rebuilding() }));
    book.reload(&reads, &[], 120).await.unwrap();
    assert_eq!(book.status(), ReconstructionStatus::Rebuilding, "a read that may predate the gap cleared it");

    book.reload(&reads, &[], 121).await.unwrap();
    assert_eq!(book.status(), ReconstructionStatus::Verified);
}

/// A read older than one the book holds is refused whole — at the start, and at
/// the write if a newer reload landed while this one was reading.
#[tokio::test]
async fn a_reload_older_than_the_books_newest_read_is_refused() {
    let node = Arc::new(Scripted::default());
    healthy(&node, Venue::UniswapV3.factory());
    let reads = ChainReads::new(node.clone());
    let book = Arc::new(PoolBook::load(&reads, &[spec()], 100).await.unwrap().0);

    book.reload(&reads, &[WETH_USDC], 120).await.unwrap();
    assert_eq!(
        book.reload(&reads, &[WETH_USDC], 110).await.unwrap_err(),
        ReloadError::Older { block: 110, newest: 120 }
    );

    // A newer reload lands while this one reads.
    let (b, n) = (Arc::clone(&book), Arc::clone(&node));
    *node.during_read.lock().unwrap() = Some(Box::pin(async move {
        b.reload(&ChainReads::new(n), &[WETH_USDC], 130).await.unwrap();
    }));
    assert_eq!(
        book.reload(&reads, &[WETH_USDC], 125).await.unwrap_err(),
        ReloadError::Older { block: 125, newest: 130 }
    );
    assert_eq!(book.get(WETH_USDC).unwrap().block, 130, "the newer read stands");
}

/// **A full reload reads the universe.** A pool one failed read removed comes
/// back at the next full reload that reads it; a partial reload, which reads
/// only what the book holds, does not bring it back.
#[tokio::test]
async fn a_pool_a_failed_read_removed_returns_at_the_next_full_reload() {
    let node = Arc::new(Scripted::default());
    healthy(&node, Venue::UniswapV3.factory());
    let reads = ChainReads::new(node.clone());
    let book = PoolBook::load(&reads, &[spec()], 100).await.unwrap().0;

    let key = (WETH_USDC, abi::call0(selector::SLOT0));
    let slot0 = node.answers.lock().unwrap().remove(&key).unwrap();
    assert_eq!(book.reload(&reads, &[], 101).await.unwrap().len(), 1);
    assert!(book.get(WETH_USDC).is_none(), "removed");

    node.answers.lock().unwrap().insert(key, slot0);
    assert!(book.reload(&reads, &[WETH_USDC], 102).await.unwrap().is_empty());
    assert!(book.get(WETH_USDC).is_none(), "a partial reload read it");
    assert!(book.reload(&reads, &[], 103).await.unwrap().is_empty());
    assert_eq!(book.get(WETH_USDC).map(|p| p.block), Some(103));
}

/// And the universe is what the book was loaded with, not what it loaded: a
/// pool that failed its first read is read again by the first full reload.
#[tokio::test]
async fn a_pool_refused_at_load_is_read_by_the_first_full_reload() {
    let node = Arc::new(Scripted::default());
    healthy(&node, Venue::UniswapV3.factory());
    let key = (WETH_USDC, abi::call0(selector::SLOT0));
    let slot0 = node.answers.lock().unwrap().remove(&key).unwrap();
    let reads = ChainReads::new(node.clone());
    let (book, refused) = PoolBook::load(&reads, &[spec()], 100).await.unwrap();
    assert_eq!((book.len(), refused.len()), (0, 1));

    node.answers.lock().unwrap().insert(key, slot0);
    assert!(book.reload(&reads, &[], 101).await.unwrap().is_empty());
    assert_eq!(book.len(), 1);
}

/// A book over snapshots a caller held — a replay — is built over those pools,
/// and a full reload reads them.
#[tokio::test]
async fn a_book_over_held_snapshots_reloads_them() {
    let node = Arc::new(Scripted::default());
    healthy(&node, Venue::UniswapV3.factory());
    let reads = ChainReads::new(node.clone());
    let held = PoolBook::load(&reads, &[spec()], 100).await.unwrap().0.get(WETH_USDC).unwrap();
    let book = PoolBook::from_snapshots([(*held).clone()], ReconstructionStatus::Rebuilding);
    assert!(book.reload(&reads, &[], 101).await.unwrap().is_empty());
    assert_eq!(book.get(WETH_USDC).map(|p| p.block), Some(101));
    assert_eq!(book.status(), ReconstructionStatus::Verified);
}

// ------------------------------------------------------------------ Slipstream's fee

const SLIP: Address = address!("b2cc224c1c9feE385f8ad6a55b4d94E92359DC59");
const MODULE: Address = address!("090b2A6bb475c00e2256e2095A60887cD710803b");

fn slip_spec() -> PoolSpec {
    PoolSpec { pool: SLIP, venue: Venue::Slipstream, ..spec() }
}

fn observe_answer(c0: i64, c1: i64) -> Vec<u8> {
    let word = |v: i128| {
        let mut w = if v < 0 { [0xffu8; 32] } else { [0u8; 32] };
        w[16..].copy_from_slice(&v.to_be_bytes());
        w.to_vec()
    };
    [0x40, 0xa0, 2, i128::from(c0), i128::from(c1), 2, 0, 0].into_iter().flat_map(word).collect()
}

/// Cumulatives whose TWAP over 600 s is `twap`.
fn twap_answer(twap: i64) -> Vec<u8> {
    observe_answer(0, twap * 600)
}

/// A healthy Slipstream pool: the Uniswap shape under Slipstream's factory, an
/// oracle with `cardinality` observations, and the fee module's answers for
/// WETH/USDC CL100's regime with the TWAP 11 ticks above the price.
fn slipstream(node: &Scripted, cardinality: u64) {
    let factory = Venue::Slipstream.factory();
    healthy_at(node, SLIP, factory);
    let mut slot0 = w(U256::from(4_109_375_649_317_904_751_454_295u128));
    slot0.extend(wi(TICK));
    for v in [0u64, cardinality, cardinality, 1] {
        slot0.extend(w(U256::from(v)));
    }
    node.set(SLIP, abi::call0(selector::SLOT0), slot0);
    node.set(factory, abi::call0(selector::SWAP_FEE_MODULE), wa(MODULE));
    node.set(MODULE, abi::call0(selector::DEFAULT_SCALING_FACTOR), w(U256::ZERO));
    node.set(MODULE, abi::call0(selector::DEFAULT_FEE_CAP), w(U256::from(30_000)));
    node.set(MODULE, abi::call0(selector::SECONDS_AGO), w(U256::from(600)));
    let mut cfg = Vec::new();
    for v in [535u64, 2_000, 14_900_000, 1, 150] {
        cfg.extend(w(U256::from(v)));
    }
    node.set(MODULE, abi::call_address(selector::DYNAMIC_FEE_CONFIG, SLIP), cfg);
    node.set(factory, abi::call_signed(selector::TICK_SPACING_TO_FEE, SPACING), w(U256::from(500)));
    node.set(SLIP, abi::call_observe(600), twap_answer(TICK + 11));
}

/// **A Slipstream pool loads with its fee regime**, read from its factory's
/// module at the load block, and its fee is the one its tick implies — not the
/// `fee()` it answered, which in a block with no swap yet is the initial fee.
#[tokio::test]
async fn a_slipstream_pool_loads_with_its_fee_regime() {
    let node = Arc::new(Scripted::default());
    slipstream(&node, 1_000);
    let (book, refused) = PoolBook::load(&ChainReads::new(node), &[slip_spec()], 100).await.unwrap();
    assert!(refused.is_empty(), "{refused:?}");
    let p = book.get(SLIP).unwrap();
    let d = p.dynamic_fee.expect("a Slipstream pool has one");
    assert_eq!(d, DynamicFee { base: 535, cap: 2_000, scaling: 14_900_000, initial: Some(150), seconds_ago: 600, twap_tick: Some(TICK as i32 + 11) });
    assert_eq!(p.state.fee_ppm, 698, "535 + 11 × 14.9, not the 500 `fee()` answered");
}

/// A pool with fewer observations than the window needs — half of it, since
/// Base writes one every two seconds — gets no dynamic fee, as the module gives
/// it none: it is priced at its base. Exactly half is enough.
#[tokio::test]
async fn too_few_observations_is_no_dynamic_fee() {
    for (cardinality, twap, fee) in [(299, None, 535), (300, Some(TICK as i32 + 11), 698)] {
        let node = Arc::new(Scripted::default());
        slipstream(&node, cardinality);
        let (book, _) = PoolBook::load(&ChainReads::new(node), &[slip_spec()], 100).await.unwrap();
        let p = book.get(SLIP).unwrap();
        assert_eq!((p.dynamic_fee.unwrap().twap_tick, p.state.fee_ppm), (twap, fee), "{cardinality}");
    }
}

/// A Slipstream pool whose regime cannot be read is refused, and says which
/// read: priced at a fee nobody read, it would be wrong by up to its cap.
#[tokio::test]
async fn a_slipstream_pool_without_a_readable_fee_is_refused() {
    let node = Arc::new(Scripted::default());
    slipstream(&node, 1_000);
    node.answers.lock().unwrap().remove(&(MODULE, abi::call_address(selector::DYNAMIC_FEE_CONFIG, SLIP)));
    let (book, refused) = PoolBook::load(&ChainReads::new(node), &[slip_spec()], 100).await.unwrap();
    assert!(book.get(SLIP).is_none());
    assert_eq!(refused[0].why, Unloadable::FeeUnreadable { read: "dynamicFeeConfig" });
}

/// **The fee follows the TWAP between swaps**, and the version moves with it.
/// A TWAP the pool can no longer serve leaves it without one — the module's own
/// answer to a reverting `observe` — and a Uniswap pool is not asked.
#[tokio::test]
async fn the_fee_follows_the_twap_between_swaps() {
    let node = Arc::new(Scripted::default());
    slipstream(&node, 1_000);
    healthy(&node, Venue::UniswapV3.factory());
    let reads = ChainReads::new(node.clone());
    let (book, _) = PoolBook::load(&reads, &[slip_spec(), spec()], 100).await.unwrap();
    let before = book.versions_for(&[SLIP]);

    // The TWAP catches up by four ticks: 7 × 14.9.
    node.set(SLIP, abi::call_observe(600), twap_answer(TICK + 7));
    assert_eq!(book.refresh_twaps(&reads, 101).await.unwrap(), 1);
    assert_eq!(book.get(SLIP).unwrap().state.fee_ppm, 639);
    assert_ne!(book.versions_for(&[SLIP]), before);

    // Unchanged: nothing written.
    let now = book.versions_for(&[SLIP]);
    assert_eq!(book.refresh_twaps(&reads, 102).await.unwrap(), 0);
    assert_eq!(book.versions_for(&[SLIP]), now);

    // Pinned at the cap, a TWAP that moves moves no fee: stored, not counted.
    book.apply_swap(SLIP, U256::from(4_100_000_000_000_000_000_000_000u128), 7, TICK as i32 - 200, (104, 0));
    assert_eq!(book.get(SLIP).unwrap().state.fee_ppm, 2_000);
    node.set(SLIP, abi::call_observe(600), twap_answer(TICK + 9));
    let at_cap = book.versions_for(&[SLIP]);
    assert_eq!(book.refresh_twaps(&reads, 105).await.unwrap(), 0, "the fee did not change");
    assert_eq!(book.get(SLIP).unwrap().dynamic_fee.unwrap().twap_tick, Some(TICK as i32 + 9));
    assert_ne!(book.versions_for(&[SLIP]), at_cap, "but the pool did");

    node.answers.lock().unwrap().remove(&(SLIP, abi::call_observe(600)));
    assert_eq!(book.refresh_twaps(&reads, 106).await.unwrap(), 1);
    let p = book.get(SLIP).unwrap();
    assert_eq!((p.dynamic_fee.unwrap().twap_tick, p.state.fee_ppm), (None, 535));
    assert_eq!(book.get(WETH_USDC).unwrap().state.fee_ppm, 500, "a fixed fee is not refreshed");
}

/// A reload that keeps a newer swap's price states the fee at the **kept**
/// tick, under the fresh regime.
#[tokio::test]
async fn a_reload_that_keeps_a_swap_prices_the_fee_at_the_kept_tick() {
    let node = Arc::new(Scripted::default());
    slipstream(&node, 1_000);
    let reads = ChainReads::new(node.clone());
    let (book, _) = PoolBook::load(&reads, &[slip_spec()], 100).await.unwrap();
    let price = U256::from(4_100_000_000_000_000_000_000_000u128);
    // Preconfirmed, in block 121: 30 ticks below the TWAP.
    book.apply_swap(SLIP, price, 7, TICK as i32 - 19, (121, 3));
    assert_eq!(book.get(SLIP).unwrap().state.fee_ppm, 535 + 30 * 149 / 10);
    book.reload(&reads, &[SLIP], 120).await.unwrap();
    let p = book.get(SLIP).unwrap();
    assert_eq!(p.state.tick, TICK as i32 - 19, "the swap is kept");
    assert_eq!(p.state.fee_ppm, 535 + 30 * 149 / 10, "and its fee with it");
}

/// A plain reload states the fee from the fresh regime at the fresh tick —
/// never the `fee()` the pool answered, which a block with no swap yet serves
/// as the initial fee.
#[tokio::test]
async fn a_reload_states_the_fee_from_the_fresh_regime() {
    let node = Arc::new(Scripted::default());
    slipstream(&node, 1_000);
    let reads = ChainReads::new(node.clone());
    let (book, _) = PoolBook::load(&reads, &[slip_spec()], 100).await.unwrap();
    node.set(SLIP, abi::call_observe(600), twap_answer(TICK + 5));
    book.reload(&reads, &[SLIP], 120).await.unwrap();
    assert_eq!(book.get(SLIP).unwrap().state.fee_ppm, 535 + 5 * 149 / 10, "not the 500 `fee()` answered");
}

// ------------------------------------------------------------------ PancakeSwap

const CAKE: Address = address!("72ab388e2E2F6FaceF59E3C3FA2C4E29011c2D38");

/// **A PancakeSwap pool loads as a Uniswap one does**: held to its own factory,
/// its fee from `fee()`, and no fee module asked about — the node answers
/// nothing about one, so a read of it would refuse the pool.
#[tokio::test]
async fn a_pancakeswap_pool_loads_with_its_static_fee() {
    let node = Arc::new(Scripted::default());
    healthy_at(&node, CAKE, Venue::PancakeV3.factory());
    let cake = PoolSpec { pool: CAKE, venue: Venue::PancakeV3, ..spec() };
    let (book, refused) = PoolBook::load(&ChainReads::new(node), &[cake], 100).await.unwrap();
    assert!(refused.is_empty(), "{refused:?}");
    let p = book.get(CAKE).unwrap();
    assert_eq!((p.state.fee_ppm, p.factory), (500, Venue::PancakeV3.factory()));
    assert!(p.dynamic_fee.is_none());
}

/// Filed under PancakeSwap, deployed by Uniswap's factory: refused.
#[tokio::test]
async fn a_pool_filed_under_pancakeswap_from_another_factory_is_refused() {
    let node = Arc::new(Scripted::default());
    healthy_at(&node, CAKE, Venue::UniswapV3.factory());
    let cake = PoolSpec { pool: CAKE, venue: Venue::PancakeV3, ..spec() };
    let (book, refused) = PoolBook::load(&ChainReads::new(node), &[cake], 100).await.unwrap();
    assert!(book.is_empty());
    assert!(matches!(refused[0].why, Unloadable::WrongFactory { .. }), "{refused:?}");
}

// ------------------------------------------------------------------ Aerodrome v2 (R24)

const AERO: Address = address!("cDAC0d6c6C59727a65F871236188350531885C43");
const AERO_R0: u128 = 1_823_383_892_520_317_644_689;
const AERO_R1: u128 = 4_541_987_609_188;

/// `cast calldata "getFee(address,bool)" 0xcdac0d6c6c59727a65f871236188350531885c43 false`
#[test]
fn get_fee_encodes_as_cast_does() {
    assert_eq!(
        hex::encode(abi::call_address_bool(selector::GET_FEE, AERO, false)),
        "cc56b2c5000000000000000000000000cdac0d6c6c59727a65f871236188350531885c430000000000000000000000000000000000000000000000000000000000000000"
    );
}

fn aero_spec() -> PoolSpec {
    PoolSpec { pool: AERO, venue: Venue::AerodromeV2, token0: WETH, token1: USDC, fee_ppm: 3_000, depth_usd: 4_900_000.0 }
}

fn aero_healthy(node: &Scripted) {
    let f = Venue::AerodromeV2.factory();
    node.set(AERO, abi::call0(selector::TOKEN0), wa(WETH));
    node.set(AERO, abi::call0(selector::TOKEN1), wa(USDC));
    node.set(AERO, abi::call0(selector::FACTORY), wa(f));
    node.set(AERO, abi::call0(selector::STABLE), w(U256::ZERO));
    let mut reserves = w(U256::from(AERO_R0));
    reserves.extend(w(U256::from(AERO_R1)));
    reserves.extend(w(U256::from(1_791_000_000u64)));
    node.set(AERO, abi::call0(selector::GET_RESERVES), reserves);
    node.set(WETH, abi::call0(selector::DECIMALS), w(U256::from(18)));
    node.set(USDC, abi::call0(selector::DECIMALS), w(U256::from(6)));
    node.set(f, abi::call_address_bool(selector::GET_FEE, AERO, false), w(U256::from(30)));
    node.set(f, abi::call_address(selector::IS_POOL, AERO), w(U256::from(1)));
}

async fn aero_book(node: Arc<Scripted>) -> (PoolBook, Vec<apex_runtime::live::book::Unloaded>) {
    PoolBook::load(&ChainReads::new(node), &[aero_spec()], 100).await.expect("reads")
}

fn eth(v: u128) -> ethers_core::types::U256 {
    ethers_core::types::U256::from(v)
}

#[tokio::test]
async fn a_volatile_aerodrome_pool_loads_with_its_reserves_and_fee() {
    let node = Arc::new(Scripted::default());
    aero_healthy(&node);
    let (book, refused) = aero_book(node).await;
    assert!(refused.is_empty(), "{refused:?}");
    let p = book.get(AERO).expect("loaded");
    assert_eq!(p.reserves, Some(Reserves { reserve0: eth(AERO_R0), reserve1: eth(AERO_R1) }));
    assert_eq!(p.state.fee_ppm, 3_000, "30 bps");
    assert_eq!(p.decimals, (18, 6));
    assert!(p.ladder_covers_price(), "no ladder to leave");
    // Its tick state is empty: a concentrated-liquidity quote of it fails closed.
    assert_eq!(p.state.liquidity, 0);
    assert!(p.state.sqrt_price_x96.is_zero());
}

#[tokio::test]
async fn a_stable_aerodrome_pool_is_refused() {
    let node = Arc::new(Scripted::default());
    aero_healthy(&node);
    node.set(AERO, abi::call0(selector::STABLE), w(U256::from(1)));
    let (book, refused) = aero_book(node).await;
    assert!(book.is_empty());
    assert_eq!(refused[0].why, Unloadable::NotVolatile);
}

/// A pool with an empty side prices nothing, so it is not held — the same
/// refusal a concentrated-liquidity pool without liquidity gets.
#[tokio::test]
async fn an_empty_aerodrome_pool_is_refused() {
    let node = Arc::new(Scripted::default());
    aero_healthy(&node);
    let mut reserves = w(U256::from(AERO_R0));
    reserves.extend(w(U256::ZERO));
    reserves.extend(w(U256::from(1_791_000_000u64)));
    node.set(AERO, abi::call0(selector::GET_RESERVES), reserves);
    let (book, refused) = aero_book(node).await;
    assert!(book.is_empty());
    assert_eq!(refused[0].why, Unloadable::NoLiquidity);
}

/// The pool's own `factory()` is not enough: the factory must know it.
#[tokio::test]
async fn an_aerodrome_pool_its_factory_does_not_know_is_refused() {
    let node = Arc::new(Scripted::default());
    aero_healthy(&node);
    node.set(Venue::AerodromeV2.factory(), abi::call_address(selector::IS_POOL, AERO), w(U256::ZERO));
    let (book, refused) = aero_book(node).await;
    assert!(book.is_empty());
    assert!(matches!(refused[0].why, Unloadable::WrongFactory { .. }), "{refused:?}");
}

/// A `Sync` replaces the reserves outright; an older one is refused; a write
/// of the other kind of state is refused, whichever way round.
#[tokio::test]
async fn a_sync_replaces_the_reserves_and_an_older_one_is_refused() {
    let node = Arc::new(Scripted::default());
    aero_healthy(&node);
    healthy(&node, Venue::UniswapV3.factory());
    let (book, _) = PoolBook::load(&ChainReads::new(node), &[aero_spec(), spec()], 100).await.unwrap();
    let (a, b) = (U256::from(5u64), U256::from(7u64));
    assert_eq!(book.apply_sync(AERO, a, b, (101, 2)), SwapApplied::Updated);
    assert_eq!(book.get(AERO).unwrap().reserves, Some(Reserves { reserve0: eth(5), reserve1: eth(7) }));
    assert_eq!(book.apply_sync(AERO, b, a, (101, 1)), SwapApplied::Stale);
    assert_eq!(book.apply_sync(AERO, b, a, (100, 9)), SwapApplied::Stale, "already in the read");
    assert_eq!(book.apply_sync(WETH_USDC, a, b, (102, 0)), SwapApplied::Unknown, "not a reserve pool");
    assert_eq!(book.apply_swap(AERO, U256::from(1u64) << 96, 1, 0, (102, 1)), SwapApplied::Unknown, "not a tick pool");
    // Both kinds in one write, in order.
    let writes = [
        StateWrite::Sync(SyncWrite { pool: AERO, reserve0: b, reserve1: a, at: (103, 0) }),
        StateWrite::Swap(SwapWrite {
            pool: WETH_USDC,
            sqrt_price_x96: U256::from(4_109_375_649_317_904_751_454_295u128),
            liquidity: 9,
            tick: TICK as i32,
            at: (103, 1),
        }),
    ];
    assert_eq!(book.apply_writes(&writes), vec![SwapApplied::Updated, SwapApplied::Updated]);
}

/// A reload never rolls back a newer `Sync`.
#[tokio::test]
async fn a_reload_keeps_a_newer_sync() {
    let node = Arc::new(Scripted::default());
    aero_healthy(&node);
    let reads = ChainReads::new(node.clone());
    let (book, _) = PoolBook::load(&reads, &[aero_spec()], 100).await.unwrap();
    book.apply_sync(AERO, U256::from(5u64), U256::from(7u64), (101, 0));
    book.reload(&reads, &[AERO], 100).await.unwrap();
    assert_eq!(book.get(AERO).unwrap().reserves, Some(Reserves { reserve0: eth(5), reserve1: eth(7) }));
}

/// The factory's fee manager can change a pool's fee; each head re-reads it.
#[tokio::test]
async fn a_fee_refresh_follows_the_factory() {
    let node = Arc::new(Scripted::default());
    aero_healthy(&node);
    let reads = ChainReads::new(node.clone());
    let (book, _) = PoolBook::load(&reads, &[aero_spec()], 100).await.unwrap();
    assert_eq!(book.refresh_cp_fees(&reads, 101).await.unwrap(), 0, "unchanged");
    node.set(Venue::AerodromeV2.factory(), abi::call_address_bool(selector::GET_FEE, AERO, false), w(U256::from(50)));
    assert_eq!(book.refresh_cp_fees(&reads, 102).await.unwrap(), 1);
    assert_eq!(book.get(AERO).unwrap().state.fee_ppm, 5_000);
}
