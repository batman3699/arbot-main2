//! Task 8.5 R8 — the live reader (`apex_runtime::live::reader`).
//!
//! Driven against a scripted node that counts its calls, because the property
//! that matters most is one a result cannot show: a ticket's reading makes
//! **no** RPC call. The view is read once a block, and the capture path reads
//! the view.

mod support;

use alloy_primitives::{address, hex, keccak256, Address, B256, U256};
use apex_chain::rpc::ws::Head;
use apex_chain::rpc::{RpcError, RpcTransport};
use apex_math::cl_math::get_sqrt_ratio_at_tick;
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::TickLadder;
use apex_runtime::live::book::{PoolBook, PoolSnapshot};
use apex_runtime::live::frontier::{BALANCER_VAULT, WETH};
use apex_runtime::live::inventory::{PoolSpec, Venue};
use apex_runtime::live::reader::{ChainReader, ReaderConfig};
use apex_runtime::plane::{Decline, LiveReader};
use apex_types::ids::{PoolId, VenueId};
use apex_types::state::ReconstructionStatus;
use apex_types::ticket::TicketStatus;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const EXECUTOR: Address = address!("7CDB3F91fA5df7c7580cC9D857DCfAaEB8f7A044");
const SIGNER: Address = address!("69d54e5fc0b9325d7250f0d0a11690327a3dd8a3");
const UNI: Address = address!("d0b53D9277642d899DF5C87A3966A349A798F224");
const SLIP: Address = address!("b2cc224c1c9feE385f8ad6a55b4d94E92359DC59");
const ELSEWHERE: Address = address!("00000000000000000000000000000000000000ee");
/// The executor's code: an EIP-1167 clone, 45 bytes.
const CODE: &str = "363d3d373d3d3d363d73b1b8cbf0e1b7f32e054c2bb1091473f564d6f0325af43d82803e903d91602b57fd5bf3";

/// Answers the four reads a view needs, and counts every call.
struct Node {
    code: Mutex<String>,
    balance_fails: Mutex<bool>,
    calls: AtomicUsize,
}

fn aggregate3_answer(word: U256) -> String {
    let w = |v: U256| v.to_be_bytes::<32>().to_vec();
    let u = |v: usize| w(U256::from(v));
    // (bool success, bytes returnData)[] with one element.
    let mut out = [u(32), u(1), u(32), u(1), u(64), u(32), w(word)].concat();
    out.truncate(7 * 32);
    format!("0x{}", hex::encode(out))
}

#[async_trait::async_trait]
impl RpcTransport for Node {
    async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match method {
            "eth_getCode" => {
                assert_eq!(params[0].as_str().unwrap().parse::<Address>().unwrap(), EXECUTOR);
                Ok(json!(format!("0x{}", self.code.lock().unwrap())))
            }
            "eth_getBalance" if *self.balance_fails.lock().unwrap() => {
                Err(RpcError::Exhausted { method: method.into(), endpoints: 1, last: "timed out".into() })
            }
            "eth_getBalance" => {
                assert_eq!(params[0].as_str().unwrap().parse::<Address>().unwrap(), SIGNER);
                Ok(json!("0x5af3107a4000")) // 0.0001 ETH
            }
            "eth_getTransactionCount" => {
                assert_eq!(params[1], json!("pending"));
                Ok(json!("0x7"))
            }
            // The lender's WETH: 29.08 WETH, as Balancer's vault held on Base.
            "eth_call" => Ok(json!(aggregate3_answer(U256::from(29_082_963_936_604_904_861u128)))),
            other => panic!("unexpected {other}"),
        }
    }
}

fn node() -> Arc<Node> {
    Arc::new(Node { code: Mutex::new(CODE.into()), balance_fails: Mutex::new(false), calls: AtomicUsize::new(0) })
}

fn pool(addr: Address, venue: Venue, tick: i32) -> PoolSnapshot {
    PoolSnapshot {
        spec: PoolSpec { pool: addr, venue, token0: WETH, token1: address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"), fee_ppm: 500, depth_usd: 5e6 },
        state: ClPoolState {
            sqrt_price_x96: get_sqrt_ratio_at_tick(tick).unwrap(),
            liquidity: 1_400_000_000_000_000_000,
            tick,
            tick_spacing: 10,
            fee_ppm: 500,
            balance0: None,
            balance1: None,
        },
        ladder: TickLadder::new(vec![(-199_000, 1), (-196_000, -1)], -200_000, -195_000),
        decimals: (18, 6),
        factory: venue.factory(),
        code_hash: B256::ZERO,
        block: 100,
        last_log: None,
        dynamic_fee: None,
        seq: 0,
    }
}

fn book() -> Arc<PoolBook> {
    Arc::new(PoolBook::from_snapshots(
        [pool(UNI, Venue::UniswapV3, -197_350), pool(SLIP, Venue::Slipstream, -197_300), pool(ELSEWHERE, Venue::UniswapV3, -197_340)],
        ReconstructionStatus::Verified,
    ))
}

fn config(max_age: Duration) -> ReaderConfig {
    ReaderConfig {
        executor: EXECUTOR,
        executor_code_hash: keccak256(hex::decode(CODE).unwrap()),
        plan_version: 2,
        signer: SIGNER,
        lender: BALANCER_VAULT,
        loan_token: WETH,
        fee_ceiling_wei: 100_000_000,
        min_profit: 1,
        max_age,
    }
}

fn head(number: u64) -> Head {
    Head { number, hash: B256::repeat_byte(9), timestamp: 1, base_fee_per_gas: Some(6_000_000), gas_used: 1, gas_limit: 400_000_000 }
}

/// A ticket whose route is UNI then SLIP, borrowing 5 WETH.
fn ticket() -> apex_types::ticket::OpportunityTicket {
    let mut t = support::ticket_at(TicketStatus::Simulated);
    let mut hops = t.route_commitment.hops.clone();
    hops[0].pool = PoolId { chain: t.chain_id, address: UNI };
    let mut second = hops[0].clone();
    second.pool = PoolId { chain: t.chain_id, address: SLIP };
    second.venue = VenueId(Venue::Slipstream.id().0);
    hops.push(second);
    t.route_commitment.hops = hops;
    t.exact_input = U256::from(5u128 * 10u128.pow(18));
    t
}

/// **Read once a block, and every check gets its value — without a call.**
#[tokio::test]
async fn a_view_is_read_once_a_block_and_tickets_read_it_without_a_call() {
    let n = node();
    let b = book();
    let r = ChainReader::new(n.clone(), b.clone(), config(Duration::from_secs(60)));
    r.refresh(&head(101), 30_000_000).await.unwrap();
    assert_eq!(n.calls.swap(0, Ordering::SeqCst), 4, "four reads a block");

    let t = ticket();
    let got = r.read(&t).await.unwrap();
    let _ = r.read(&t).await.unwrap();
    assert_eq!(n.calls.load(Ordering::SeqCst), 0, "a ticket's reading made an RPC call");

    assert_eq!(got.executor, EXECUTOR.into_array());
    assert_eq!(got.executor_version, 2);
    assert_eq!((got.observed_fee_wei, got.fee_ceiling_wei), (6_000_000, 100_000_000));
    assert_eq!(got.remaining_block_gas, 30_000_000);
    assert_eq!(got.signer_balance_wei, 100_000_000_000_000);
    assert_eq!((got.flash_required, got.flash_available), (5 * 10u128.pow(18), 29_082_963_936_604_904_861));
    assert_eq!(got.live_hooks, None);
    assert_eq!(got.chain_pending_nonce, 7);
    assert_eq!(got.min_profit, 1);
    assert_eq!(got.live_venue_versions, b.versions_for(&[UNI, SLIP]));
}

/// **The venue versions are live, and the route's own.** A swap on a pool the
/// route does not touch moves nothing; one on a route pool moves its venue.
#[tokio::test]
async fn the_venue_versions_are_the_books_now_over_the_routes_pools() {
    let b = book();
    let r = ChainReader::new(node(), b.clone(), config(Duration::from_secs(60)));
    r.refresh(&head(101), 30_000_000).await.unwrap();
    let t = ticket();
    let before = r.read(&t).await.unwrap().live_venue_versions;
    let sqrt = |tick: i32| {
        let mut w = [0u8; 32];
        get_sqrt_ratio_at_tick(tick).unwrap().to_big_endian(&mut w);
        U256::from_be_bytes(w)
    };
    b.apply_swap(ELSEWHERE, sqrt(-197_341), 1, -197_341, (102, 0));
    assert_eq!(r.read(&t).await.unwrap().live_venue_versions, before);
    b.apply_swap(SLIP, sqrt(-197_301), 1, -197_301, (102, 1));
    assert_ne!(r.read(&t).await.unwrap().live_venue_versions, before);
}

/// **Code other than the configured code is not the executor committed to.**
/// A redeploy or no code at all reads as version 0, which check 2 refuses.
#[tokio::test]
async fn code_other_than_the_configured_code_is_version_zero() {
    let n = node();
    let r = ChainReader::new(n.clone(), book(), config(Duration::from_secs(60)));
    for code in ["", "6080604052"] {
        *n.code.lock().unwrap() = code.into();
        r.refresh(&head(101), 30_000_000).await.unwrap();
        assert_eq!(r.read(&ticket()).await.unwrap().executor_version, 0, "code {code:?}");
    }
}

/// No view yet, or one older than the reader's maximum age, is not a reading:
/// the ticket is refused as stale rather than read against it.
#[tokio::test]
async fn no_view_or_a_stale_one_is_refused() {
    let r = ChainReader::new(node(), book(), config(Duration::from_secs(60)));
    assert!(matches!(r.read(&ticket()).await, Err(Decline::StaleState { .. })), "before the first head");

    let r = ChainReader::new(node(), book(), config(Duration::ZERO));
    r.refresh(&head(101), 30_000_000).await.unwrap();
    std::thread::sleep(Duration::from_millis(2));
    assert!(matches!(r.read(&ticket()).await, Err(Decline::StaleState { .. })), "past its age");
}

/// A failed read leaves the last view in place — to age out, not to be patched
/// with a guess — and a head without a base fee is not a view at all.
#[tokio::test]
async fn a_failed_read_keeps_the_last_view() {
    let n = node();
    let r = ChainReader::new(n.clone(), book(), config(Duration::from_secs(60)));
    r.refresh(&head(101), 30_000_000).await.unwrap();
    *n.balance_fails.lock().unwrap() = true;
    assert!(r.refresh(&head(102), 30_000_000).await.is_err());
    assert_eq!(r.view().unwrap().block, 101);

    let fresh = ChainReader::new(node(), book(), config(Duration::from_secs(60)));
    let no_fee = Head { base_fee_per_gas: None, ..head(101) };
    assert!(fresh.refresh(&no_fee, 30_000_000).await.is_err());
    assert!(fresh.view().is_none());
}
