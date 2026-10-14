//! R28's fork proof. Anvil forks Base's latest block from the public endpoint
//! (never the shadow's BlockPI key); nothing touches mainnet.
//!
//! Needs `anvil` and `cast` on PATH. In any tree:
//!   cargo test -j 3 -p apex-runtime --test canary_fork -- --ignored --nocapture

use alloy_primitives::{address, Address, U256};
use apex_chain::base::send::{HttpSubmitter, RawTransport};
use apex_chain::rpc::failover::{FailoverSettings, FailoverTransport};
use apex_chain::rpc::RpcTransport;
use apex_config::Secret;
use apex_exec::call::ExecutorCall;
use apex_exec::commitment::{plan_commitment, Loan, LoanProvider};
use apex_exec::encode::PlanEncoder;
use apex_exec::sign::SignedPlan;
use apex_runtime::canary::chain::{CanaryChain, ChainSettings};
use apex_runtime::canary::limits::{Canary, Limits};
use apex_runtime::canary::send::{CanaryDispatcher, InFlight};
use apex_runtime::live::adapter::Attribution;
use apex_runtime::live::book::{PoolBook, PoolSnapshot};
use apex_runtime::live::calls::encode_step;
use apex_runtime::live::frontier::{self, BALANCER_VAULT, WETH};
use apex_runtime::live::inventory::{PoolSpec, Venue};
use apex_runtime::live::pricing::LiveCycle;
use apex_runtime::live::reads::ChainReads;
use apex_runtime::plane::{FeeCaps, SendContext, SettlementFeed};
use apex_runtime::sign::{Eip1559Tx, LaneKey};
use apex_types::compat::{u256_to_alloy, u256_to_ethers};
use apex_types::cost::GasLimit;
use apex_types::ids::{ChainId, StrategyId, VenueId};
use std::collections::BTreeMap;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

const FORK: &str = "https://mainnet.base.org";
const PORT: &str = "8547";
const LOCAL: &str = "http://127.0.0.1:8547";
const EXECUTOR: Address = address!("8940B565D050974b2b589B70De43bCb753b1F93B");
const ROUTER: Address = address!("9ecde68C269EbbaFD44bbB4DfC9D6716B47952C2");
const OWNER: Address = address!("69D54e5fC0b9325D7250f0D0A11690327A3dd8A3");
const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const UNI_500: Address = address!("d0b53D9277642d899DF5C87A3966A349A798F224");
const SLIP_100: Address = address!("b2cc224c1c9feE385f8ad6a55b4d94E92359DC59");
const SWAP_ROUTER02: Address = address!("2626664c2603336E57B271c5C0b26F421741e481");
/// Anvil's first dev account: public, test-only.
const DEV_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const DEV: Address = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

/// Anvil does not model OP Stack's L1 data fee, so its receipts carry no
/// `l1Fee`, and the canary refuses to reconcile a receipt without one (review
/// focus 2: a zero would understate every trade's cost). The proof states the
/// fork's L1 fee as zero, here and nowhere else; the gross it checks comes from
/// the executor's transfers and is untouched by it.
struct AnvilReceipts(Arc<dyn RpcTransport>);

#[async_trait::async_trait]
impl RpcTransport for AnvilReceipts {
    async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, apex_chain::rpc::RpcError> {
        let mut v = self.0.call(method, params).await?;
        if method == "eth_getTransactionReceipt" {
            if let Some(r) = v.as_object_mut() {
                r.entry("l1Fee").or_insert_with(|| serde_json::json!("0x0"));
            }
        }
        Ok(v)
    }
}

struct Anvil(Child);

impl Drop for Anvil {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

fn cast(args: &[&str]) -> String {
    let out = Command::new("cast").args(args).output().expect("cast on PATH");
    assert!(out.status.success(), "cast {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn send_as(from: Address, to: Address, sig: &str, args: &[&str], value: Option<&str>) {
    let (f, t) = (format!("{from:#x}"), format!("{to:#x}"));
    let mut a = vec!["send", "--unlocked", "--from", &f, &t, sig];
    a.extend_from_slice(args);
    if let Some(v) = value {
        a.extend_from_slice(&["--value", v]);
    }
    a.extend_from_slice(&["--rpc-url", LOCAL]);
    cast(&a);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "network: forks Base from the public endpoint; needs anvil and cast"]
async fn a_real_trade_is_sent_settled_and_reconciled_to_the_wei() {
    // Pinned: the book is read at this block from the public endpoint (step 3).
    let fork_block: u64 = cast(&["block-number", "--rpc-url", FORK]).parse().expect("a block number");
    let _anvil = Anvil(
        Command::new("anvil")
            .args(["--fork-url", FORK, "--fork-block-number", &fork_block.to_string(), "--port", PORT, "--silent"])
            .stdout(Stdio::null())
            .spawn()
            .expect("anvil on PATH"),
    );
    let mut up = false;
    for _ in 0..60 {
        if Command::new("cast").args(["chain-id", "--rpc-url", LOCAL]).output().is_ok_and(|o| o.status.success()) {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(up, "anvil did not come up");

    // 1. The owner, impersonated: authorize the dev key and point profit at the owner.
    cast(&["rpc", "anvil_impersonateAccount", &format!("{OWNER:#x}"), "--rpc-url", LOCAL]);
    cast(&["rpc", "anvil_setBalance", &format!("{OWNER:#x}"), "0xde0b6b3a7640000", "--rpc-url", LOCAL]);
    let e = format!("{EXECUTOR:#x}");
    let calls = [
        cast(&["calldata", "setExecutor(address,bool)", &format!("{DEV:#x}"), "true"]),
        cast(&["calldata", "setProfitRecipient(address)", &format!("{OWNER:#x}")]),
        cast(&["calldata", "setFeeRecipient(address)", &format!("{OWNER:#x}")]),
    ];
    send_as(OWNER, ROUTER, "multicall(address[],bytes[])", &[&format!("[{e},{e},{e}]"), &format!("[{}]", calls.join(","))], None);
    assert_eq!(
        cast(&["call", &e, "profitRecipient()(address)", "--rpc-url", LOCAL]).to_lowercase(),
        format!("{OWNER:#x}").to_lowercase()
    );

    // 2. Open a gap: the dev account sells 50 WETH into Uniswap's 0.05% pool.
    let w = format!("{WETH:#x}");
    send_as(DEV, WETH, "deposit()", &[], Some("60ether"));
    send_as(DEV, WETH, "approve(address,uint256)", &[&format!("{SWAP_ROUTER02:#x}"), &U256::MAX.to_string()], None);
    send_as(
        DEV,
        SWAP_ROUTER02,
        "exactInputSingle((address,address,uint24,address,uint256,uint256,uint160))",
        &[&format!("({w},{USDC:#x},500,{DEV:#x},50000000000000000000,0,0)")],
        None,
    );

    // 3. The book at the fork block, read from the public endpoint: a multicall
    // there runs where the state is, where the fork would fetch every slot a
    // round trip at a time (a deep ladder is thousands of them). The whale's
    // swap moved one pool's price, tick and liquidity, and no ladder: those
    // three are read back from the fork.
    let public: Arc<dyn RpcTransport> = Arc::new(
        FailoverTransport::connect(&[FORK.to_string()], 8453, FailoverSettings { request_timeout: Duration::from_secs(30), ..FailoverSettings::default() })
            .await
            .unwrap(),
    );
    let specs = vec![
        PoolSpec { pool: UNI_500, venue: Venue::UniswapV3, token0: WETH, token1: USDC, fee_ppm: 500, depth_usd: 1e7 },
        PoolSpec { pool: SLIP_100, venue: Venue::Slipstream, token0: WETH, token1: USDC, fee_ppm: 600, depth_usd: 1e7 },
    ];
    let (book, unloaded) = PoolBook::load(&ChainReads::new(Arc::clone(&public)), &specs, fork_block).await.unwrap();
    assert!(unloaded.is_empty(), "{unloaded:?}");
    let mut pools: BTreeMap<Address, Arc<PoolSnapshot>> = (*book.snapshot()).clone();
    let mut uni = (*pools[&UNI_500]).clone();
    let u = format!("{UNI_500:#x}");
    let first = |text: String| text.split_whitespace().next().expect("a value").to_string();
    let slot0 = cast(&["call", &u, "slot0()(uint160,int24,uint16,uint16,uint16,uint8,bool)", "--rpc-url", LOCAL]);
    let mut words = slot0.lines().map(|l| first(l.to_string()));
    uni.state.sqrt_price_x96 = ethers_core::types::U256::from_dec_str(&words.next().unwrap()).unwrap();
    uni.state.tick = words.next().unwrap().parse().unwrap();
    uni.state.liquidity = first(cast(&["call", &u, "liquidity()(uint128)", "--rpc-url", LOCAL])).parse().unwrap();
    pools.insert(UNI_500, Arc::new(uni));
    let snap = Arc::new(pools);
    // The fork's own transport, for the trade's receipts.
    let rpc: Arc<dyn RpcTransport> = Arc::new(AnvilReceipts(Arc::new(
        FailoverTransport::connect(&[LOCAL.to_string()], 8453, FailoverSettings { request_timeout: Duration::from_secs(120), ..FailoverSettings::default() })
            .await
            .unwrap(),
    )));
    let (_, cycles) = frontier::build(ChainId::BASE, WETH, &snap);
    let cycle = cycles.values().find(|c| c.legs[0].pool == SLIP_100).expect("the Slipstream-first cycle");
    let live = LiveCycle::new(cycle, &snap).unwrap();
    let one = U256::from(10u64).pow(U256::from(18u64));
    let (size, mid, out) = [1u64, 2, 5, 10, 20]
        .into_iter()
        .filter_map(|n| {
            let s = U256::from(n) * one;
            let [m, o] = live.hop_outputs(u256_to_ethers(s))?.map(u256_to_alloy);
            Some((s, m, o))
        })
        .max_by_key(|(s, _, o)| o.saturating_sub(*s))
        .unwrap();
    assert!(out > size, "the gap pays: {out} for {size}");
    let predicted_gross = out - size;

    // 4. The call, built as the plane builds it, and signed by the dev key.
    let block = cast(&["block", "latest", "--field", "timestamp", "--rpc-url", LOCAL]).parse::<u64>().unwrap();
    let deadline = block + 120;
    let steps = vec![
        encode_step(&cycle.legs[0], &snap[&cycle.legs[0].pool], size, mid, EXECUTOR, deadline).unwrap(),
        encode_step(&cycle.legs[1], &snap[&cycle.legs[1].pool], mid, size + U256::from(1), EXECUTOR, deadline).unwrap(),
    ];
    let loan = Loan { token: WETH, amount: size, provider: LoanProvider::Balancer, provider_addr: BALANCER_VAULT };
    let plan = PlanEncoder::build(loan, steps, 0, U256::from(1), 8453, deadline);
    let declared = plan_commitment(&plan, 8453, EXECUTOR);
    let call = ExecutorCall::from_checked(SignedPlan::check(plan, declared, 8453, EXECUTOR).unwrap());
    let key = LaneKey::from_hex(&Secret::new(DEV_KEY.to_string())).unwrap();
    let nonce = cast(&["nonce", &format!("{DEV:#x}"), "--rpc-url", LOCAL]).parse::<u64>().expect("a nonce");
    let signed = key
        .sign_eip1559(&Eip1559Tx {
            chain_id: 8453,
            nonce,
            max_priority_fee_per_gas: 1_000_000,
            max_fee_per_gas: 2_000_000_000,
            gas_limit: 1_500_000,
            to: call.to(),
            data: call.data().to_vec(),
        })
        .unwrap();

    // 5. Through the canary's own pieces, pointed at the fork.
    let dir = tempfile::tempdir().unwrap();
    let canary = Arc::new(Canary::open(&dir.path().join("canary.jsonl"), &dir.path().join("canary.stop"), Limits {
        loss_cap_wei: 1_000_000_000_000_000,
        max_consecutive_reverts: 5,
        max_consecutive_refusals: 2,
        max_landed: 100,
    }).unwrap());
    let in_flight = Arc::new(InFlight::default());
    let submitter = Arc::new(HttpSubmitter::new(LOCAL, Duration::from_secs(5)).unwrap());
    let sender = CanaryDispatcher::new(Arc::clone(&canary), submitter as Arc<dyn RawTransport>, Arc::clone(&in_flight));
    let ctx = SendContext {
        chain: ChainId::BASE,
        strategy: StrategyId(1),
        route_hash: cycle.commitment.route_hash,
        venues: vec![VenueId(1)],
        gas_limit: GasLimit(1_500_000),
        fees: FeeCaps { max_fee_per_gas: 2_000_000_000, max_priority_fee_per_gas: 1_000_000 },
        expected_gross_wei: predicted_gross,
        expected_net_wei: 0,
        waived: vec!["route_authorization_valid".into()],
    };
    let tx = sender.send_raw(1, nonce, &signed.raw, &ctx).await.expect("anvil took it");
    assert_eq!(tx, signed.hash);
    let chain = CanaryChain::new(Arc::clone(&rpc), Arc::clone(&canary), Arc::clone(&in_flight), ChainSettings {
        weth: WETH,
        executor: EXECUTOR,
        trader: DEV,
        recipients: vec![OWNER],
        poll: Duration::from_millis(200),
        timeout: Duration::from_secs(20),
    });
    let seen = chain.observe(tx).await.unwrap();
    assert!(!seen.is_empty(), "included");
    let pnl = chain.attribute(tx).await.expect("reconciled");
    println!("FORK PROOF: gross {} predicted {predicted_gross}, net {}", pnl.gross_profit, pnl.net_profit_token);
    assert_eq!(U256::from(u128::try_from(pnl.gross_profit).unwrap()), predicted_gross, "realized gross = prediction, to the wei");
    assert_eq!(canary.report().totals.landed, 1);

    // 6. A stale plan (deadline past) reverts early and is counted.
    let stale_plan = PlanEncoder::build(
        Loan { token: WETH, amount: size, provider: LoanProvider::Balancer, provider_addr: BALANCER_VAULT },
        vec![],
        0,
        U256::from(1),
        8453,
        block - 1_000,
    );
    let d2 = plan_commitment(&stale_plan, 8453, EXECUTOR);
    let stale = ExecutorCall::from_checked(SignedPlan::check(stale_plan, d2, 8453, EXECUTOR).unwrap());
    let s2 = key.sign_eip1559(&Eip1559Tx { chain_id: 8453, nonce: nonce + 1, max_priority_fee_per_gas: 1_000_000, max_fee_per_gas: 2_000_000_000, gas_limit: 1_500_000, to: stale.to(), data: stale.data().to_vec() }).unwrap();
    let tx2 = sender.send_raw(2, nonce + 1, &s2.raw, &ctx).await.expect("sent");
    chain.observe(tx2).await.unwrap();
    assert!(chain.attribute(tx2).await.is_err(), "the stale plan reverts");
    assert_eq!(canary.report().totals.reverted_early, 1, "the deadline is checked before the loan");

    // 7. With the stop file present, nothing is sent.
    std::fs::write(dir.path().join("canary.stop"), b"").unwrap();
    assert!(sender.send_raw(3, nonce + 2, &s2.raw, &ctx).await.is_err());
    assert_eq!(canary.report().totals.sent, 2);
}
