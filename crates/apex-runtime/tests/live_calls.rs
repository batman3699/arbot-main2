//! Task 8.5 R6 — the production call builder (`apex_runtime::live::calls`).
//!
//! Pools are real Base shapes with a price gap between them, so which cycle
//! pays is arithmetic the test can state. The encoders are held to `cast` in
//! `apex-exec`; here the question is whether the builder composes them into the
//! plan the candidate was priced as — the right venue's step in the right
//! order, exact amounts, the floor, and one loan from the one wired lender.

mod support;

use alloy_primitives::{address, Address, B256, U256};
use apex_exec::commitment::{plan_commitment, LoanProvider, Op};
use apex_exec::encode::{
    generic_step, slipstream_exact_input_single, univ3_path, univ3_step, v3_router_exact_input_single, PathHop,
    SlipstreamSwap, V3RouterSwap,
};
use apex_math::cl_math::get_sqrt_ratio_at_tick;
use apex_math::cl_state::ClPoolState;
use apex_math::cl_swap::TickLadder;
use apex_math::finite_size::SizedRoute;
use apex_runtime::live::book::{PoolBook, PoolSnapshot};
use apex_runtime::live::calls::LiveCallBuilder;
use apex_runtime::live::frontier::{self, Cycle, BALANCER_FLASH, BALANCER_VAULT, PANCAKE_ADAPTER, SLIPSTREAM_ADAPTER, WETH};
use apex_runtime::live::inventory::{PoolSpec, Venue};
use apex_runtime::live::pricing::LiveCycle;
use apex_runtime::plane::{CallBuilder, Decline};
use apex_types::candidate::{Candidate, DiscreteRefined, DiscreteSize};
use apex_types::commitment::ExecutionCommitment;
use apex_types::compat::{u256_to_alloy, u256_to_ethers};
use apex_types::ids::{ChainId, FlashProviderId};
use apex_types::state::ReconstructionStatus;
use std::collections::BTreeMap;
use std::sync::Arc;

const BASE: ChainId = ChainId(8453);
const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const UNI: Address = address!("d0b53D9277642d899DF5C87A3966A349A798F224");
const SLIP: Address = address!("b2cc224c1c9feE385f8ad6a55b4d94E92359DC59");
const EXECUTOR: Address = address!("1c3d856D29eA2118c8d955070a6AD83C984586f3");
const DEADLINE: u64 = 1_790_000_000;
const L: u128 = 1_400_000_000_000_000_000;

fn pool(addr: Address, venue: Venue, tick: i32, fee_ppm: u32, spacing: i32) -> PoolSnapshot {
    let lower = (tick - 2_000) / spacing * spacing;
    let upper = (tick + 2_000) / spacing * spacing;
    PoolSnapshot {
        spec: PoolSpec { pool: addr, venue, token0: WETH, token1: USDC, fee_ppm, depth_usd: 5e6 },
        state: ClPoolState {
            sqrt_price_x96: get_sqrt_ratio_at_tick(tick).unwrap(),
            liquidity: L,
            tick,
            tick_spacing: spacing,
            fee_ppm,
            balance0: Some(ethers_core::types::U256::from(10u128.pow(21))),
            balance1: Some(ethers_core::types::U256::from(3_000_000_000_000u128)),
        },
        ladder: TickLadder::new(vec![(lower, L as i128), (upper, -(L as i128))], lower - 5_000, upper + 5_000),
        decimals: (18, 6),
        factory: venue.factory(),
        code_hash: B256::ZERO,
        block: 100,
        last_log: None,
        dynamic_fee: None,
        seq: 0,
    }
}

const PANCAKE: Address = address!("72ab388e2E2F6FaceF59E3C3FA2C4E29011c2D38");

/// WETH is cheaper on Uniswap than on Slipstream when `uni_tick` is below `slip_tick`.
///
/// The Uniswap pool's inventory fee (3,000) is not its chain fee (500): the
/// router finds a pool by tokens and fee, so the builder must encode the fee
/// read from the chain.
fn book(uni_tick: i32, slip_tick: i32) -> Arc<PoolBook> {
    let mut uni = pool(UNI, Venue::UniswapV3, uni_tick, 500, 10);
    uni.spec.fee_ppm = 3_000;
    Arc::new(PoolBook::from_snapshots(
        [uni, pool(SLIP, Venue::Slipstream, slip_tick, 400, 100)],
        ReconstructionStatus::Verified,
    ))
}

/// The cycle that pays, and a candidate for it priced against the book.
fn priced(b: &Arc<PoolBook>) -> (Cycle, Candidate) {
    let cycles = frontier::cycles(BASE, WETH, &b.snapshot());
    let input = U256::from(10u128.pow(18));
    let (cycle, out) = cycles
        .into_iter()
        .map(|c| {
            let out = LiveCycle::new(&c, &b.snapshot(), 0).unwrap().output(u256_to_ethers(input)).unwrap();
            (c, u256_to_alloy(out))
        })
        .find(|(_, out)| *out > input)
        .expect("one direction pays");
    let mut c = support::candidate(1, 100, 1);
    c.route = cycle.commitment.clone();
    c.input_amount = DiscreteSize::from_refinement(input, DiscreteRefined::new());
    c.expected_output = out;
    (cycle, c)
}

fn commitment(c: &Candidate, min_profit: u64) -> ExecutionCommitment {
    ExecutionCommitment {
        chain_id: BASE,
        executor_address: EXECUTOR,
        executor_version: 1,
        venue_fingerprints: BTreeMap::new(),
        flash_source: BALANCER_FLASH,
        state_fingerprint_hash: B256::ZERO,
        route_hash: c.route.route_hash,
        exact_inputs: vec![c.input_amount.get()],
        min_profit: U256::from(min_profit),
        slippage_constraints: vec![30, 30],
        deadline: DEADLINE,
        submission_policy: c.submission_policy,
    }
}

/// What the builder must produce for one hop, from the encoders directly.
fn expected_step(b: &PoolBook, pool: Address, token_in: Address, token_out: Address, amount_in: U256, min_out: U256) -> (Op, Vec<u8>) {
    let p = b.get(pool).unwrap();
    match p.spec.venue {
        Venue::UniswapV3 => {
            let path = univ3_path(&[PathHop { token_in, fee: p.state.fee_ppm }], token_out).unwrap();
            (Op::UniV3, univ3_step(&path, amount_in, min_out).unwrap())
        }
        Venue::Slipstream => {
            let call = slipstream_exact_input_single(&SlipstreamSwap {
                token_in,
                token_out,
                tick_spacing: p.state.tick_spacing,
                recipient: EXECUTOR,
                deadline: DEADLINE,
                amount_in,
                min_out,
            })
            .unwrap();
            (Op::Generic, generic_step(SLIPSTREAM_ADAPTER, token_in, amount_in, &call))
        }
        Venue::PancakeV3 => {
            let call = v3_router_exact_input_single(&V3RouterSwap {
                token_in,
                token_out,
                fee: p.state.fee_ppm,
                recipient: EXECUTOR,
                amount_in,
                min_out,
            })
            .unwrap();
            (Op::Generic, generic_step(PANCAKE_ADAPTER, token_in, amount_in, &call))
        }
    }
}

/// **A priced cycle becomes one checked plan**, in both venue orders: one
/// Balancer loan of the input in the start token, each hop as its venue's step,
/// hop 2 spending exactly what hop 1 must return, hop 2 bounded by the larger
/// of its slippage bound and the loan plus the minimum profit, and a commitment
/// the executor will recompute.
#[test]
fn a_priced_cycle_becomes_one_checked_plan_in_either_venue_order() {
    // A lower tick is fewer USDC per WETH. The cycle starts in WETH, so it sells
    // WETH first where WETH is dear — the higher tick — and buys it back cheap.
    for (uni, slip, first) in [(-197_350, -197_300, Venue::Slipstream), (-197_300, -197_350, Venue::UniswapV3)] {
        let b = book(uni, slip);
        let (cycle, c) = priced(&b);
        assert_eq!(b.get(cycle.legs[0].pool).unwrap().spec.venue, first, "WETH is sold first where it is dear");
        let k = commitment(&c, 1);
        let call = LiveCallBuilder::new(b.clone(), [cycle.clone()]).build(&c, &k).expect("builds");

        assert_eq!((call.to(), call.chain_id()), (EXECUTOR, 8453));
        let plan = call.plan();
        assert_eq!(plan_commitment(plan, 8453, EXECUTOR), call.commitment());
        assert_eq!((plan.min_profit, plan.chain_id, plan.deadline), (U256::from(1u64), 8453, DEADLINE));

        let input = c.input_amount.get();
        assert_eq!(plan.loans.len(), 1);
        let loan = plan.loans[0];
        assert_eq!((loan.token, loan.amount, loan.provider, loan.provider_addr), (WETH, input, LoanProvider::Balancer, BALANCER_VAULT));

        let mid = u256_to_alloy(
            LiveCycle::new(&cycle, &b.snapshot(), 0).unwrap().hop_outputs(u256_to_ethers(input)).unwrap()[0],
        );
        let floor = (input + U256::from(1u64)).max(c.expected_output * U256::from(9_970u64) / U256::from(10_000u64));
        let [l0, l1] = &cycle.legs;
        let want = [
            expected_step(&b, l0.pool, l0.token_in, l0.token_out, input, mid),
            expected_step(&b, l1.pool, l1.token_in, l1.token_out, mid, floor),
        ];
        assert_eq!(plan.steps.len(), 2);
        for (got, (op, data)) in plan.steps.iter().zip(want) {
            assert_eq!(got.op, op);
            assert_eq!(got.data, data, "{op:?}");
        }
        assert_eq!(plan.cycle_slippage_bps, 0, "the hops' own bounds are the policy");

        // With a minimum profit of half the gain, what the cycle owes is above
        // the slippage bound, and it is hop 2's minimum.
        let owed = input + (c.expected_output - input) / U256::from(2u64);
        assert!(owed > c.expected_output * U256::from(9_970u64) / U256::from(10_000u64));
        let k = commitment(&c, u64::try_from((c.expected_output - input) / U256::from(2u64)).unwrap());
        let call = LiveCallBuilder::new(b.clone(), [cycle.clone()]).build(&c, &k).expect("builds");
        let want = expected_step(&b, l1.pool, l1.token_in, l1.token_out, mid, owed);
        assert_eq!((call.plan().steps[1].op, call.plan().steps[1].data.clone()), want);
    }
}

/// **PancakeSwap's hop is adapter 2's call**: the `SmartRouter`'s
/// `exactInputSingle` with the chain's fee — here 100 ppm where the inventory
/// says 500 — paying the executor, in either order with Uniswap.
#[test]
fn a_pancakeswap_hop_is_adapter_twos_router_call() {
    for (uni, cake) in [(-197_350, -197_300), (-197_300, -197_350)] {
        let mut p = pool(PANCAKE, Venue::PancakeV3, cake, 100, 1);
        p.spec.fee_ppm = 500;
        let b = Arc::new(PoolBook::from_snapshots(
            [pool(UNI, Venue::UniswapV3, uni, 500, 10), p],
            ReconstructionStatus::Verified,
        ));
        let (cycle, c) = priced(&b);
        let call = LiveCallBuilder::new(b.clone(), [cycle.clone()]).build(&c, &commitment(&c, 1)).expect("builds");
        let input = c.input_amount.get();
        let mid = u256_to_alloy(
            LiveCycle::new(&cycle, &b.snapshot(), 0).unwrap().hop_outputs(u256_to_ethers(input)).unwrap()[0],
        );
        let floor = (input + U256::from(1u64)).max(c.expected_output * U256::from(9_970u64) / U256::from(10_000u64));
        let [l0, l1] = &cycle.legs;
        let want = [
            expected_step(&b, l0.pool, l0.token_in, l0.token_out, input, mid),
            expected_step(&b, l1.pool, l1.token_in, l1.token_out, mid, floor),
        ];
        for (got, (op, data)) in call.plan().steps.iter().zip(want) {
            assert_eq!((got.op, &got.data), (op, &data));
        }
        let cake_step = cycle.legs.iter().position(|l| l.pool == PANCAKE).unwrap();
        assert_eq!(call.plan().steps[cake_step].op, Op::Generic);
    }
}

/// The book moved after pricing: the candidate's price is not the price of any
/// plan buildable now.
#[test]
fn a_book_that_moved_since_pricing_is_stale() {
    let b = book(-197_350, -197_300);
    let (cycle, c) = priced(&b);
    b.apply_swap(cycle.legs[1].pool, U256::from(1u64) << 96, L, -197_310, (101, 0));
    let err = LiveCallBuilder::new(b.clone(), [cycle]).build(&c, &commitment(&c, 1)).unwrap_err();
    assert!(matches!(err, Decline::StaleState { .. }), "{err:?}");
}

/// A pool off its ladder cannot be quoted, so no plan can be built on it.
#[test]
fn a_pool_off_its_ladder_is_stale() {
    let b = book(-197_350, -197_300);
    let (cycle, c) = priced(&b);
    b.apply_swap(cycle.legs[0].pool, U256::from(1u64) << 96, L, -150_000, (101, 0));
    let err = LiveCallBuilder::new(b.clone(), [cycle]).build(&c, &commitment(&c, 1)).unwrap_err();
    assert!(matches!(err, Decline::StaleState { .. }), "{err:?}");
}

/// A cycle that cannot return the loan plus its minimum profit is refused
/// here, not built into a plan the executor reverts.
#[test]
fn a_cycle_below_its_floor_is_refused() {
    let b = book(-197_350, -197_300);
    let (cycle, c) = priced(&b);
    let gain = c.expected_output - c.input_amount.get();
    let k = commitment(&c, u64::try_from(gain).unwrap() + 1);
    let err = LiveCallBuilder::new(b, [cycle]).build(&c, &k).unwrap_err();
    assert!(matches!(&err, Decline::Uncommittable { detail } if detail.contains("minimum profit")), "{err:?}");
}

/// The executor takes exactly one loan, and only Balancer's is wired.
#[test]
fn only_balancers_loan_is_wired() {
    let b = book(-197_350, -197_300);
    let (cycle, c) = priced(&b);
    for source in [FlashProviderId(0), FlashProviderId(2)] {
        let k = ExecutionCommitment { flash_source: source, ..commitment(&c, 1) };
        let err = LiveCallBuilder::new(b.clone(), [cycle.clone()]).build(&c, &k).unwrap_err();
        assert!(matches!(err, Decline::Uncommittable { .. }), "{source:?}: {err:?}");
    }
}

/// A commitment for another route, an unknown route, and shapes a two-hop
/// cycle does not have are all refused before anything is encoded.
#[test]
fn a_commitment_that_does_not_describe_this_cycle_is_refused() {
    let b = book(-197_350, -197_300);
    let (cycle, c) = priced(&b);
    let builder = LiveCallBuilder::new(b.clone(), [cycle.clone()]);
    let k = commitment(&c, 1);
    // The other direction's cycle is one the builder knows: a commitment for it
    // must not be built against this candidate's price.
    let other = frontier::cycles(BASE, WETH, &b.snapshot()).into_iter().find(|o| o.id != cycle.id).unwrap();
    let both = LiveCallBuilder::new(b.clone(), [cycle.clone(), other.clone()]);
    let err = both.build(&c, &ExecutionCommitment { route_hash: other.commitment.route_hash, ..k.clone() }).unwrap_err();
    assert!(matches!(&err, Decline::Uncommittable { detail } if detail.contains("another route")), "{err:?}");
    for bad in [
        ExecutionCommitment { exact_inputs: vec![], ..k.clone() },
        ExecutionCommitment { exact_inputs: vec![U256::from(1u64); 2], ..k.clone() },
        ExecutionCommitment { slippage_constraints: vec![30], ..k.clone() },
    ] {
        let err = builder.build(&c, &bad).unwrap_err();
        assert!(matches!(err, Decline::Uncommittable { .. }), "{err:?}");
    }
    // A route the builder holds no cycle for.
    let err = LiveCallBuilder::new(b, []).build(&c, &k).unwrap_err();
    assert!(matches!(&err, Decline::Uncommittable { detail } if detail.contains("no live cycle")), "{err:?}");
}

// ------------------------------------------------------------------ reachability

use apex_chain::rpc::{RpcError, RpcTransport};
use apex_runtime::live::abi::{self, selector, MULTICALL3};
use apex_runtime::live::calls::{binding, reachable_venues};
use apex_runtime::live::frontier::{PANCAKE_SMART_ROUTER, SLIPSTREAM_ROUTER};
use apex_runtime::live::reads::ChainReads;
use serde_json::{json, Value};
use std::sync::Mutex;

fn word(v: U256) -> Vec<u8> {
    v.to_be_bytes::<32>().to_vec()
}

fn address_word(a: Address) -> Vec<u8> {
    [vec![0u8; 12], a.to_vec()].concat()
}

/// `aggregate3`'s answer: `(bool success, bytes returnData)[]`.
fn encode_results(results: &[Option<Vec<u8>>]) -> Vec<u8> {
    let w = |v: usize| word(U256::from(v));
    let padded = |n: usize| n.div_ceil(32) * 32;
    let mut out = [w(32), w(results.len())].concat();
    let mut next = results.len() * 32;
    for r in results {
        out.extend(w(next));
        next += 96 + padded(r.as_ref().map_or(0, Vec::len));
    }
    for r in results {
        let d = r.clone().unwrap_or_default();
        out.extend([w(usize::from(r.is_some())), w(64), w(d.len())].concat());
        out.extend(&d);
        out.extend(std::iter::repeat_n(0u8, padded(d.len()) - d.len()));
    }
    out
}

/// The sub-calls of one `aggregate3` request, in order.
fn decode_request(data: &[u8]) -> Vec<(Address, Vec<u8>)> {
    let body = &data[4..];
    let at = |i: usize| U256::from_be_slice(&body[i..i + 32]).to::<usize>();
    (0..at(32))
        .map(|i| {
            let t = 64 + at(64 + i * 32);
            let len = at(t + 96);
            (Address::from_slice(&body[t + 12..t + 32]), body[t + 128..t + 128 + len].to_vec())
        })
        .collect()
}

/// `(target, calldata) -> return data`.
type Table = BTreeMap<(Address, Vec<u8>), Vec<u8>>;

/// An executor's registry: answers what it was told, fails anything else.
#[derive(Default)]
struct Registry(Mutex<Table>);

impl Registry {
    fn holds(&self, id: u16, router: Address, sel: [u8; 4], allowed: bool) {
        let mut m = self.0.lock().unwrap();
        m.insert((EXECUTOR, abi::call_u16(selector::ADAPTER_OF, id)), address_word(router));
        m.insert(
            (EXECUTOR, abi::call_u16_bytes4(selector::IS_SELECTOR_ALLOWED, id, sel)),
            word(U256::from(u8::from(allowed))),
        );
    }
}

#[async_trait::async_trait]
impl RpcTransport for Registry {
    async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        assert_eq!(method, "eth_call");
        assert_eq!(params[0]["to"].as_str().unwrap().parse::<Address>().unwrap(), MULTICALL3);
        let data = alloy_primitives::hex::decode(params[0]["data"].as_str().unwrap()).unwrap();
        let m = self.0.lock().unwrap();
        let answers: Vec<Option<Vec<u8>>> = decode_request(&data).into_iter().map(|k| m.get(&k).cloned()).collect();
        Ok(json!(format!("0x{}", alloy_primitives::hex::encode(encode_results(&answers)))))
    }
}

/// Slipstream as deployed, and PancakeSwap as the owner would register it.
fn deployed(pancake: bool) -> Arc<Registry> {
    let r = Arc::new(Registry::default());
    let slip = binding(Venue::Slipstream).unwrap();
    r.holds(slip.id, SLIPSTREAM_ROUTER, slip.selector, true);
    if pancake {
        let cake = binding(Venue::PancakeV3).unwrap();
        r.holds(cake.id, PANCAKE_SMART_ROUTER, cake.selector, true);
    }
    r
}

async fn reach(r: Arc<Registry>) -> Vec<Venue> {
    reachable_venues(&ChainReads::new(r), EXECUTOR, 100).await.unwrap()
}

/// **A venue is priced only once the executor can reach it.** Uniswap always —
/// its op is built in; PancakeSwap once adapter 2 holds its `SmartRouter`.
#[tokio::test]
async fn a_venue_is_reachable_only_once_its_adapter_is_registered() {
    assert_eq!(reach(deployed(false)).await, vec![Venue::UniswapV3, Venue::Slipstream]);
    assert_eq!(reach(deployed(true)).await, vec![Venue::UniswapV3, Venue::Slipstream, Venue::PancakeV3]);
}

/// Registered is not enough: the adapter must be **this** router, with **this**
/// selector allowed. Anything else would revert the hop.
#[tokio::test]
async fn an_adapter_that_is_not_this_router_or_this_selector_is_unreachable() {
    let cake = binding(Venue::PancakeV3).unwrap();
    let elsewhere = deployed(false);
    elsewhere.holds(cake.id, Address::repeat_byte(0xee), cake.selector, true);
    let disallowed = deployed(false);
    disallowed.holds(cake.id, PANCAKE_SMART_ROUTER, cake.selector, false);
    let other_selector = deployed(false);
    other_selector.holds(cake.id, PANCAKE_SMART_ROUTER, [0x41, 0x4b, 0xf3, 0x89], true);
    for r in [elsewhere, disallowed, other_selector] {
        assert_eq!(reach(r).await, vec![Venue::UniswapV3, Venue::Slipstream]);
    }
}

/// An executor that answers nothing about its registry — no code, or no
/// registry — reaches Uniswap alone; a read that fails whole is an error, not
/// an empty universe.
#[tokio::test]
async fn an_unanswered_registry_reaches_uniswap_alone() {
    assert_eq!(reach(Arc::new(Registry::default())).await, vec![Venue::UniswapV3]);

    struct Down;
    #[async_trait::async_trait]
    impl RpcTransport for Down {
        async fn call(&self, method: &str, _: Value) -> Result<Value, RpcError> {
            Err(RpcError::Exhausted { method: method.into(), endpoints: 1, last: "timed out".into() })
        }
    }
    assert!(reachable_venues(&ChainReads::new(Arc::new(Down)), EXECUTOR, 100).await.is_err());
}

/// The registry calls as `cast calldata` writes them.
#[test]
fn the_registry_calls_encode_as_cast_does() {
    assert_eq!(
        alloy_primitives::hex::encode(abi::call_u16(selector::ADAPTER_OF, 2)),
        "b969a5300000000000000000000000000000000000000000000000000000000000000002"
    );
    assert_eq!(
        alloy_primitives::hex::encode(abi::call_u16_bytes4(selector::IS_SELECTOR_ALLOWED, 2, [0x04, 0xe4, 0x5a, 0xaf])),
        "284aea3f000000000000000000000000000000000000000000000000000000000000000204e45aaf00000000000000000000000000000000000000000000000000000000"
    );
}
