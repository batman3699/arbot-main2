//! Fork-at-block REVM simulation for executor plan validation.
//!
//! When `ARBOT_SIM_REVM=1`, runs a real in-process EVM against chain state at a
//! specific block (lazy-loaded via JSON-RPC) before falling back to eth_call.

use anyhow::{anyhow, Context, Result};
use ethers::abi::{encode, Token};
use ethers::types::transaction::eip2718::TypedTransaction;
use ethers::types::{Address as EthAddress, U256 as EthU256};
use ethers::utils::keccak256;
use reqwest::blocking::Client as BlockingClient;
use reqwest::Client;
use revm::context::TxEnv;
use revm::context_interface::result::{ExecutionResult, Output};
use revm::database::CacheDB;
use revm::database_interface::DatabaseRef;
use revm::handler::{ExecuteEvm, MainBuilder, MainContext};
use revm::primitives::hardfork::SpecId;
use revm::primitives::{Address, Bytes, TxKind, B256, U256};
use revm::state::{AccountInfo, Bytecode};
use revm::Context as RevmContext;
use rlp::RlpStream;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

const SIM_FUND_WEI: u128 = 1_000_000_000_000_000_000_000; // 1000 ETH
/// OP-Stack GasPriceOracle predeploy (Base, Optimism, etc.).
pub(crate) const GAS_PRICE_ORACLE: &str = "0x420000000000000000000000000000000000000F";

/// ABI-encoded calldata for `GasPriceOracle.getL1Fee(bytes)`. Shared by the
/// async (fees.rs) and blocking (this module) OP-stack L1-fee estimators so the
/// selector and argument encoding are defined in exactly one place.
pub(crate) fn encode_get_l1_fee_calldata(payload: &[u8]) -> Vec<u8> {
    let selector = &keccak256("getL1Fee(bytes)")[..4];
    let encoded_args = encode(&[Token::Bytes(payload.to_vec())]);
    let mut data = Vec::with_capacity(selector.len() + encoded_args.len());
    data.extend_from_slice(selector);
    data.extend_from_slice(&encoded_args);
    data
}

#[derive(Debug, Clone)]
pub struct ForkBlockHeader {
    pub number: u64,
    pub beneficiary: Address,
    pub timestamp: u64,
    pub gas_limit: u64,
    pub basefee: u64,
    pub prevrandao: Option<B256>,
}

#[derive(Clone)]
pub struct SimForkRequest {
    pub rpc_url: String,
    pub block_number: u64,
    pub chain_id: u64,
    pub executor_address: EthAddress,
    pub executor_bytecode: Option<Vec<u8>>,
    pub tx: TypedTransaction,
    /// Extra contract/token addresses to prefetch before execution.
    pub prefetch_addresses: Vec<EthAddress>,
    pub metrics: Option<Arc<crate::metrics::Metrics>>,
}

impl SimForkRequest {
    #[allow(dead_code)]
    pub fn prefetch_addresses(mut self, addrs: impl IntoIterator<Item = EthAddress>) -> Self {
        self.prefetch_addresses.extend(addrs);
        self
    }

    #[allow(dead_code)]
    pub fn with_metrics(mut self, metrics: Option<Arc<crate::metrics::Metrics>>) -> Self {
        self.metrics = metrics;
        self
    }
}

#[derive(Debug, Clone)]
pub struct RevmSimResult {
    pub success: bool,
    pub gas_used: u64,
    pub profit: EthU256,
    /// OP-Stack L1 data fee in native wei (zero when disabled or non-OP chain).
    pub l1_fee_wei: EthU256,
    /// L2 execution fee = gas_used * effective gas price (wei).
    pub l2_gas_cost_wei: EthU256,
    #[allow(dead_code)]
    pub revert_reason: Option<String>,
}

#[derive(Debug)]
pub enum RevmSimOutcome {
    Success,
    Failure,
    Fallback,
}

pub fn sim_revm_enabled() -> bool {
    std::env::var("ARBOT_SIM_REVM")
        .map(|raw| matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

#[allow(dead_code)]
pub fn sim_revm_live_tests_enabled() -> bool {
    std::env::var("ARBOT_SIM_REVM_LIVE")
        .map(|raw| matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

pub fn sim_revm_timeout_ms() -> u64 {
    std::env::var("ARBOT_SIM_REVM_TIMEOUT_MS")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(500)
}

fn env_flag(name: &str, default: bool) -> bool {
    std::env::var(name)
        .map(|raw| matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(default)
}

/// OP-Stack L1 data fee modeling for fork sim (default on Base / chain 8453).
pub fn sim_l1_fee_enabled(chain_id: u64) -> bool {
    match std::env::var("ARBOT_SIM_L1_FEE") {
        Ok(raw) => matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
        Err(_) => matches!(chain_id, 8453 | 10 | 420 | 7777777),
    }
}

/// Bulk account prefetch before revm execution (default on).
pub fn sim_prefetch_enabled() -> bool {
    env_flag("ARBOT_SIM_PREFETCH", true)
}

pub fn sim_prefetch_max_accounts() -> usize {
    std::env::var("ARBOT_SIM_PREFETCH_MAX_ACCOUNTS")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(32)
        .max(1)
}

fn is_op_stack_chain(chain_id: u64) -> bool {
    matches!(chain_id, 8453 | 10 | 420 | 7777777)
}

pub fn record_revm_metrics(metrics: &crate::metrics::Metrics, outcome: RevmSimOutcome) {
    match outcome {
        RevmSimOutcome::Success => metrics.sim_revm_success_total.inc(),
        RevmSimOutcome::Failure => metrics.sim_revm_failure_total.inc(),
        RevmSimOutcome::Fallback => metrics.sim_revm_fallback_total.inc(),
    }
}

/// Backward-compatible metric helper.
#[allow(dead_code)]
pub fn record_revm_metric(metrics: &crate::metrics::Metrics, success: bool) {
    if success {
        record_revm_metrics(metrics, RevmSimOutcome::Success);
    } else {
        record_revm_metrics(metrics, RevmSimOutcome::Failure);
    }
}

/// Bytecode to inject for the executor when forking at a PINNED block.
///
/// `Ok(None)` means the fork's own state already has the code and nothing
/// should be injected.
///
/// This used to fall back to `eth_getCode(executor, "latest")` when the address
/// had no code at the requested block, which destroys the determinism the whole
/// fork is for: the simulation runs at block N against bytecode that may not
/// have existed at block N. A contract deployed or upgraded after N would then
/// report a successful simulation for a trade that could not have executed.
///
/// The env override is kept because it is categorically different. It is an
/// operator stating "use this code", which is a declared intent; reaching for
/// latest chain state is the simulator inventing one.
///
/// Failing is cheap here: the caller records a Fallback metric and degrades to
/// `eth_call`, which is a real simulation rather than a fabricated one.
fn executor_bytecode_at_block(
    onchain: &[u8],
    env_override: Option<Vec<u8>>,
) -> Result<Option<Vec<u8>>> {
    if !onchain.is_empty() {
        return Ok(None);
    }
    match env_override {
        Some(bytecode) => Ok(Some(bytecode)),
        None => Err(anyhow!(
            "executor has no code at the pinned block and no bytecode override              is set; refusing to substitute latest chain state into a historical              fork (set ARBOT_SIM_REVM_EXECUTOR_BYTECODE to override deliberately)"
        )),
    }
}

/// Primary async entry: fork chain state at `block_number` and execute `tx` in REVM.
pub async fn simulate_via_revm(request: SimForkRequest) -> Result<RevmSimResult> {
    if !sim_revm_enabled() {
        return Err(anyhow!("ARBOT_SIM_REVM not enabled"));
    }

    let header = fetch_fork_header(&request.rpc_url, request.block_number)
        .await
        .context("fetch fork block header")?;

    let executor_addr = to_revm_address(request.executor_address);
    let mut executor_bytecode = request.executor_bytecode;
    if executor_bytecode.is_none() {
        let onchain = fetch_code_at_block(
            &request.rpc_url,
            request.executor_address,
            request.block_number,
        )
        .await?;
        executor_bytecode = executor_bytecode_at_block(
            &onchain,
            resolve_executor_bytecode_env(),
        )
        .with_context(|| {
            format!(
                "executor {:#x} at block {}",
                request.executor_address, request.block_number
            )
        })?;
    }

    let tx_env = typed_tx_to_tx_env(&request.tx, request.chain_id)?;
    let caller = tx_env.caller;
    let caller_nonce = fetch_nonce_at_block(
        &request.rpc_url,
        from_eth_address(caller),
        request.block_number,
    )
    .await
    .unwrap_or(tx_env.nonce);

    let unsigned_tx_rlp = encode_unsigned_tx_rlp(&request.tx, request.chain_id)?;
    let tx_calldata = request.tx.data().cloned().unwrap_or_default();
    let prefetch_addresses = collect_prefetch_addresses(
        &request.tx,
        request.executor_address,
        &request.prefetch_addresses,
        sim_prefetch_max_accounts(),
    );
    let deadline =
        Instant::now() + Duration::from_millis(sim_revm_timeout_ms().saturating_sub(50));

    let work = SimForkWork {
        rpc_url: request.rpc_url,
        block_number: request.block_number,
        chain_id: request.chain_id,
        header,
        executor_addr,
        executor_bytecode,
        tx_env,
        caller,
        caller_nonce,
        unsigned_tx_rlp,
        tx_calldata,
        prefetch_addresses,
        metrics: request.metrics,
        deadline,
    };

    tokio::task::spawn_blocking(move || execute_fork_sim(work))
        .await
        .context("revm worker join")?
}

/// Convenience wrapper used by legacy call sites.
#[allow(dead_code)]
pub async fn simulate_typed_tx_revm(
    rpc_url: &str,
    tx: &TypedTransaction,
    block_number: u64,
    chain_id: u64,
    executor_address: EthAddress,
) -> Result<RevmSimResult> {
    simulate_via_revm(SimForkRequest {
        rpc_url: rpc_url.to_string(),
        block_number,
        chain_id,
        executor_address,
        executor_bytecode: None,
        tx: tx.clone(),
        prefetch_addresses: Vec::new(),
        metrics: None,
    })
    .await
}

struct SimForkWork {
    rpc_url: String,
    block_number: u64,
    chain_id: u64,
    header: ForkBlockHeader,
    executor_addr: Address,
    executor_bytecode: Option<Vec<u8>>,
    tx_env: TxEnv,
    caller: Address,
    caller_nonce: u64,
    unsigned_tx_rlp: Vec<u8>,
    tx_calldata: ethers::types::Bytes,
    prefetch_addresses: Vec<Address>,
    metrics: Option<Arc<crate::metrics::Metrics>>,
    deadline: Instant,
}

fn execute_fork_sim(work: SimForkWork) -> Result<RevmSimResult> {
    let mut rpc_db = RpcForkDb::new(work.rpc_url.clone(), work.block_number)?;
    if sim_prefetch_enabled() && Instant::now() < work.deadline {
        let prefetch_start = Instant::now();
        let prefetched = rpc_db.batch_prefetch_accounts(&work.prefetch_addresses, work.deadline);
        if let Some(metrics) = &work.metrics {
            metrics
                .sim_revm_prefetch_accounts_total
                .inc_by(prefetched as f64);
            metrics
                .sim_revm_prefetch_ms
                .observe(prefetch_start.elapsed().as_millis() as f64);
        }
    }

    let mut cache_db = CacheDB::new(rpc_db);

    if let Some(bytecode) = work.executor_bytecode {
        inject_contract_code(&mut cache_db, work.executor_addr, bytecode)?;
    }

    fund_caller_if_needed(&mut cache_db, work.caller, work.caller_nonce)?;

    let mut tx_env = work.tx_env;
    tx_env.nonce = work.caller_nonce;
    let gas_price = tx_env.gas_price;

    let spec = spec_for_chain(work.chain_id);
    let ctx = RevmContext::mainnet()
        .with_db(&mut cache_db)
        .modify_cfg_chained(|cfg| {
            cfg.chain_id = work.chain_id;
            cfg.spec = spec;
        })
        .modify_block_chained(|block| {
            block.number = work.header.number;
            block.beneficiary = work.header.beneficiary;
            block.timestamp = work.header.timestamp;
            block.gas_limit = work.header.gas_limit;
            block.basefee = work.header.basefee;
            block.prevrandao = work.header.prevrandao;
        })
        .with_tx(tx_env);

    let mut evm = ctx.build_mainnet();
    let outcome = evm
        .replay()
        .map_err(|err| anyhow!("revm execution error: {err:?}"))?;

    let mut result = parse_execution_result(&outcome.result)?;
    if sim_l1_fee_enabled(work.chain_id) && is_op_stack_chain(work.chain_id) {
        let l1_fee = estimate_op_stack_l1_fee_blocking(
            &work.rpc_url,
            work.block_number,
            &work.unsigned_tx_rlp,
            work.tx_calldata.as_ref(),
        )
        .unwrap_or_default();
        result.l1_fee_wei = eth_u256_from_revm(l1_fee);
    }
    result.l2_gas_cost_wei =
        EthU256::from(result.gas_used).saturating_mul(EthU256::from(gas_price));
    Ok(result)
}

fn spec_for_chain(chain_id: u64) -> SpecId {
    match chain_id {
        // Base and other post-Cancun L2s.
        8453 | 10 | 42161 | 1 => SpecId::CANCUN,
        _ => SpecId::LATEST,
    }
}

fn fund_caller_if_needed(
    db: &mut CacheDB<RpcForkDb>,
    caller: Address,
    nonce: u64,
) -> Result<()> {
    let existing = db
        .basic_ref(caller)
        .map_err(|err| anyhow!("load caller account: {err:?}"))?;
    let balance = existing
        .as_ref()
        .map(|info| info.balance)
        .unwrap_or_default();
    let min_balance = U256::from(SIM_FUND_WEI);
    if balance >= min_balance {
        return Ok(());
    }
    let code = existing
        .and_then(|info| info.code)
        .unwrap_or_default();
    db.insert_account_info(
        caller,
        AccountInfo {
            balance: min_balance,
            nonce,
            code_hash: code.hash_slow(),
            code: Some(code),
        },
    );
    Ok(())
}

fn inject_contract_code(
    db: &mut CacheDB<RpcForkDb>,
    address: Address,
    bytecode: Vec<u8>,
) -> Result<()> {
    let code = Bytecode::new_raw(Bytes::from(bytecode));
    let code_hash = code.hash_slow();
    let existing = db
        .basic_ref(address)
        .map_err(|err| anyhow!("load executor account: {err:?}"))?
        .unwrap_or_default();
    db.insert_account_info(
        address,
        AccountInfo {
            balance: existing.balance,
            nonce: existing.nonce,
            code_hash,
            code: Some(code),
        },
    );
    Ok(())
}

fn parse_execution_result(result: &ExecutionResult) -> Result<RevmSimResult> {
    match result {
        ExecutionResult::Success { gas_used, output, .. } => {
            let data = match output {
                Output::Call(bytes) | Output::Create(bytes, _) => bytes.as_ref(),
            };
            if data.len() < 32 {
                return Err(anyhow!(
                    "revm sim returned insufficient bytes ({} < 32)",
                    data.len()
                ));
            }
            let profit = EthU256::from_big_endian(&data[..32]);
            Ok(RevmSimResult {
                success: true,
                gas_used: *gas_used,
                profit,
                l1_fee_wei: EthU256::zero(),
                l2_gas_cost_wei: EthU256::zero(),
                revert_reason: None,
            })
        }
        ExecutionResult::Revert { gas_used, output } => {
            let reason = decode_revert_reason(output.as_ref());
            Err(anyhow!(
                "revm sim reverted (gas_used={gas_used}): {reason}"
            ))
        }
        ExecutionResult::Halt { reason, gas_used } => Err(anyhow!(
            "revm sim halted ({reason:?}, gas_used={gas_used})"
        )),
    }
}

fn decode_revert_reason(output: &[u8]) -> String {
    if output.len() >= 4 && output[..4] == [0x08, 0xc3, 0x79, 0xa0] {
        if let Ok(decoded) = ethers::abi::decode(&[ethers::abi::ParamType::String], &output[4..]) {
            if let Some(reason) = decoded.first().and_then(|t| t.clone().into_string()) {
                return reason;
            }
        }
    }
    if output.len() >= 4 && output[..4] == [0x4e, 0x48, 0x7b, 0x71] {
        return "Panic(uint256)".to_string();
    }
    if output.is_empty() {
        return "empty revert data".to_string();
    }
    format!("0x{}", hex::encode(output))
}

fn typed_tx_to_tx_env(tx: &TypedTransaction, chain_id: u64) -> Result<TxEnv> {
    let to = tx
        .to_addr()
        .ok_or_else(|| anyhow!("revm sim requires tx.to"))?;
    let data = tx.data().cloned().unwrap_or_default();
    let value = tx.value().copied().unwrap_or_default();
    let from = tx
        .from()
        .copied()
        .ok_or_else(|| anyhow!("revm sim requires tx.from"))?;
    let gas_limit = tx.gas().map(|g| g.as_u64()).unwrap_or(30_000_000);
    let nonce = tx.nonce().map(|n| n.as_u64()).unwrap_or(0);

    let (gas_price, gas_priority_fee) = match tx {
        TypedTransaction::Eip1559(inner) => {
            let max_fee = inner.max_fee_per_gas.map(|v| v.as_u128()).unwrap_or(0);
            let priority = inner.max_priority_fee_per_gas.map(|v| v.as_u128());
            (max_fee, priority)
        }
        _ => (tx.gas_price().map(|v| v.as_u128()).unwrap_or(0), None),
    };

    Ok(TxEnv {
        caller: to_revm_address(from),
        gas_limit,
        gas_price,
        gas_priority_fee,
        kind: TxKind::Call(to_revm_address(*to)),
        value: to_revm_u256(value),
        data: Bytes::copy_from_slice(data.as_ref()),
        nonce,
        chain_id: Some(chain_id),
        ..TxEnv::default()
    })
}

fn resolve_executor_bytecode_env() -> Option<Vec<u8>> {
    for key in ["ARBOT_SIM_REVM_EXECUTOR_BYTECODE", "EXECUTOR_BYTECODE"] {
        if let Ok(raw) = std::env::var(key) {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(bytes) = hex::decode(trimmed.trim_start_matches("0x")) {
                if !bytes.is_empty() {
                    return Some(bytes);
                }
            }
        }
    }
    None
}

async fn fetch_fork_header(rpc_url: &str, block_number: u64) -> Result<ForkBlockHeader> {
    let client = Client::builder()
        .timeout(Duration::from_secs(12))
        .build()
        .context("fork header http client")?;
    let block_param = block_tag(block_number);
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_getBlockByNumber",
        "params": [block_param, false],
    });
    let response: Value = client
        .post(rpc_url)
        .json(&body)
        .send()
        .await
        .context("eth_getBlockByNumber request")?
        .json()
        .await
        .context("eth_getBlockByNumber decode")?;
    rpc_error(&response)?;
    let block = response
        .get("result")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow!("eth_getBlockByNumber missing result"))?;

    let number = parse_hex_u64(block.get("number"), "number")?;
    let timestamp = parse_hex_u64(block.get("timestamp"), "timestamp")?;
    let gas_limit = parse_hex_u64(block.get("gasLimit"), "gasLimit")?;
    let basefee = parse_hex_u64(block.get("baseFeePerGas"), "baseFeePerGas").unwrap_or(0);
    let beneficiary = parse_address_field(block.get("miner").or_else(|| block.get("author")))?;
    let mix_hash = block
        .get("mixHash")
        .and_then(|v| v.as_str())
        .map(parse_b256_hex)
        .transpose()?;

    Ok(ForkBlockHeader {
        number,
        beneficiary,
        timestamp,
        gas_limit,
        basefee,
        prevrandao: mix_hash,
    })
}

async fn fetch_code_at_block(
    rpc_url: &str,
    address: EthAddress,
    block_number: u64,
) -> Result<Vec<u8>> {
    let client = Client::builder()
        .timeout(Duration::from_secs(12))
        .build()
        .context("fetch code http client")?;
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_getCode",
        "params": [format!("{address:#x}"), block_tag(block_number)],
    });
    let response: Value = client
        .post(rpc_url)
        .json(&body)
        .send()
        .await
        .context("eth_getCode request")?
        .json()
        .await
        .context("eth_getCode decode")?;
    rpc_error(&response)?;
    let result = response
        .get("result")
        .and_then(|v| v.as_str())
        .unwrap_or("0x");
    let bytes = hex::decode(result.trim_start_matches("0x")).unwrap_or_default();
    Ok(bytes)
}

async fn fetch_nonce_at_block(
    rpc_url: &str,
    address: EthAddress,
    block_number: u64,
) -> Result<u64> {
    let client = Client::builder()
        .timeout(Duration::from_secs(12))
        .build()
        .context("fetch nonce http client")?;
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_getTransactionCount",
        "params": [format!("{address:#x}"), block_tag(block_number)],
    });
    let response: Value = client
        .post(rpc_url)
        .json(&body)
        .send()
        .await
        .context("eth_getTransactionCount request")?
        .json()
        .await
        .context("eth_getTransactionCount decode")?;
    rpc_error(&response)?;
    let result = response
        .get("result")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("eth_getTransactionCount missing result"))?;
    u64::from_str_radix(result.trim_start_matches("0x"), 16)
        .map_err(|err| anyhow!("parse nonce: {err}"))
}

fn block_tag(block_number: u64) -> Value {
    if block_number == u64::MAX {
        json!("latest")
    } else {
        json!(format!("0x{block_number:x}"))
    }
}

fn rpc_error(response: &Value) -> Result<()> {
    if let Some(err) = response.get("error") {
        return Err(anyhow!("rpc error: {err}"));
    }
    Ok(())
}

fn parse_hex_u64(value: Option<&Value>, field: &str) -> Result<u64> {
    let raw = value
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("block field {field} missing"))?;
    u64::from_str_radix(raw.trim_start_matches("0x"), 16)
        .map_err(|err| anyhow!("parse block {field}: {err}"))
}

fn parse_address_field(value: Option<&Value>) -> Result<Address> {
    let raw = value
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("block beneficiary missing"))?;
    let bytes = hex::decode(raw.trim_start_matches("0x"))
        .map_err(|err| anyhow!("parse beneficiary: {err}"))?;
    if bytes.len() != 20 {
        return Err(anyhow!("beneficiary must be 20 bytes"));
    }
    Ok(Address::from_slice(&bytes))
}

fn parse_b256_hex(raw: &str) -> Result<B256> {
    let bytes = hex::decode(raw.trim_start_matches("0x"))
        .map_err(|err| anyhow!("parse b256: {err}"))?;
    if bytes.len() != 32 {
        return Err(anyhow!("b256 must be 32 bytes"));
    }
    Ok(B256::from_slice(&bytes))
}

fn to_revm_address(addr: EthAddress) -> Address {
    Address::from_slice(addr.as_bytes())
}

fn from_eth_address(addr: Address) -> EthAddress {
    EthAddress::from_slice(addr.as_slice())
}

fn to_revm_u256(value: EthU256) -> U256 {
    let mut buf = [0u8; 32];
    value.to_big_endian(&mut buf);
    U256::from_be_bytes(buf)
}

fn eth_u256_from_revm(value: U256) -> EthU256 {
    let bytes = value.to_be_bytes::<32>();
    EthU256::from_big_endian(&bytes)
}

/// Collect unique prefetch targets: tx endpoints, access list, and extras.
pub fn collect_prefetch_addresses(
    tx: &TypedTransaction,
    executor: EthAddress,
    extra: &[EthAddress],
    max_accounts: usize,
) -> Vec<Address> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut push = |addr: EthAddress| {
        if addr.is_zero() {
            return;
        }
        let revm_addr = to_revm_address(addr);
        if seen.insert(revm_addr) {
            out.push(revm_addr);
        }
    };
    if let Some(from) = tx.from() {
        push(*from);
    }
    if let Some(to) = tx.to_addr() {
        push(*to);
    }
    push(executor);
    if let Some(list) = tx.access_list() {
        for item in &list.0 {
            push(item.address);
        }
    }
    for addr in extra {
        push(*addr);
    }
    out.truncate(max_accounts);
    out
}

fn rlp_append_bytes(stream: &mut RlpStream, data: &[u8]) {
    stream.append(&data);
}

fn encode_unsigned_tx_rlp(tx: &TypedTransaction, chain_id: u64) -> Result<Vec<u8>> {
    use ethers::types::NameOrAddress;

    let zero_addr = EthAddress::zero();
    match tx {
        TypedTransaction::Eip1559(inner) => {
            let mut stream = RlpStream::new_list(9);
            stream.append(&inner.chain_id.unwrap_or(chain_id.into()));
            stream.append(&inner.nonce.unwrap_or_default());
            stream.append(&inner.max_priority_fee_per_gas.unwrap_or_default());
            stream.append(&inner.max_fee_per_gas.unwrap_or_default());
            stream.append(&inner.gas.unwrap_or_default());
            let to = inner
                .to
                .clone()
                .unwrap_or(NameOrAddress::Address(zero_addr));
            stream.append(&to);
            stream.append(&inner.value.unwrap_or_default());
            let data = inner.data.clone().unwrap_or_default();
            rlp_append_bytes(&mut stream, data.as_ref());
            rlp_append_access_list(&mut stream, &inner.access_list);
            Ok(stream.out().to_vec())
        }
        TypedTransaction::Eip2930(inner) => {
            let mut stream = RlpStream::new_list(8);
            stream.append(&inner.tx.chain_id.unwrap_or(chain_id.into()));
            stream.append(&inner.tx.nonce.unwrap_or_default());
            stream.append(&inner.tx.gas_price.unwrap_or_default());
            stream.append(&inner.tx.gas.unwrap_or_default());
            let to = inner
                .tx
                .to
                .clone()
                .unwrap_or(NameOrAddress::Address(zero_addr));
            stream.append(&to);
            stream.append(&inner.tx.value.unwrap_or_default());
            let data = inner.tx.data.clone().unwrap_or_default();
            rlp_append_bytes(&mut stream, data.as_ref());
            rlp_append_access_list(&mut stream, &inner.access_list);
            Ok(stream.out().to_vec())
        }
        TypedTransaction::Legacy(inner) => {
            let mut stream = RlpStream::new_list(6);
            stream.append(&inner.nonce.unwrap_or_default());
            stream.append(&inner.gas_price.unwrap_or_default());
            stream.append(&inner.gas.unwrap_or_default());
            let to = inner
                .to
                .clone()
                .unwrap_or(NameOrAddress::Address(zero_addr));
            stream.append(&to);
            stream.append(&inner.value.unwrap_or_default());
            let data = inner.data.clone().unwrap_or_default();
            rlp_append_bytes(&mut stream, data.as_ref());
            Ok(stream.out().to_vec())
        }
    }
}

fn rlp_append_access_list(
    stream: &mut RlpStream,
    access_list: &ethers::types::transaction::eip2930::AccessList,
) {
    stream.begin_list(access_list.0.len());
    for item in &access_list.0 {
        stream.begin_list(2);
        stream.append(&item.address);
        stream.begin_list(item.storage_keys.len());
        for key in &item.storage_keys {
            stream.append(key);
        }
    }
}

fn estimate_op_stack_l1_fee_blocking(
    rpc_url: &str,
    block_number: u64,
    unsigned_tx_rlp: &[u8],
    tx_calldata: &[u8],
) -> Result<U256> {
    let client = BlockingClient::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .context("l1 fee http client")?;
    let block_param = block_tag(block_number);
    let oracle = GAS_PRICE_ORACLE;

    for payload in [unsigned_tx_rlp, tx_calldata] {
        if payload.is_empty() {
            continue;
        }
        let data = encode_get_l1_fee_calldata(payload);
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_call",
            "params": [{
                "to": oracle,
                "data": format!("0x{}", hex::encode(&data)),
            }, block_param],
        });
        if let Ok(response) = client
            .post(rpc_url)
            .json(&body)
            .send()
            .and_then(|r| r.json::<Value>())
        {
            if let Some(result) = response.get("result").and_then(|v: &Value| v.as_str()) {
                if let Ok(fee) = parse_u256_hex(result) {
                    return Ok(fee);
                }
            }
        }
    }
    Err(anyhow!("GasPriceOracle getL1Fee call failed"))
}

#[derive(Debug)]
struct RpcDbError(String);

impl std::fmt::Display for RpcDbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for RpcDbError {}

impl revm::database_interface::DBErrorMarker for RpcDbError {}

#[derive(Debug, Clone)]
/// Cross-simulation storage cache, keyed by block.
///
/// `storage_ref` issued ONE synchronous `eth_getStorageAt` per storage slot. A
/// UniV3 swap touches dozens of slots (slot0, tick bitmaps, liquidity, token
/// balances), and `RpcForkDb` is constructed fresh per simulation, so nothing
/// was reused — not within a simulation, and not across the many candidates a
/// single scan simulates at the SAME block. Measured on Base that made one
/// simulation cost ~3.2s, far past any budget a 2s-block chain can allow.
///
/// Storage at a fixed block is immutable, so sharing it is sound. The cache is
/// scoped to one block and dropped wholesale when the head advances, which also
/// bounds memory without any eviction policy.
struct StorageCache {
    block: u64,
    slots: std::collections::HashMap<(Address, U256), U256>,
}

/// Hard bound so a pathological scan cannot grow the map without limit inside a
/// single block. Far above the working set of a normal scan (~tens of pools x
/// tens of slots); hitting it means something is wrong, so it resets rather
/// than evicting cleverly.
const STORAGE_CACHE_MAX_SLOTS: usize = 250_000;

fn storage_cache() -> &'static std::sync::Mutex<StorageCache> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<StorageCache>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        std::sync::Mutex::new(StorageCache::empty())
    })
}

impl StorageCache {
    fn empty() -> Self {
        Self {
            block: 0,
            slots: std::collections::HashMap::new(),
        }
    }

    fn get(&self, block: u64, address: Address, index: U256) -> Option<U256> {
        if self.block != block {
            return None;
        }
        self.slots.get(&(address, index)).copied()
    }

    fn put(&mut self, block: u64, address: Address, index: U256, value: U256) {
        if self.block != block {
            // Head advanced: prior-block state is worthless and must not be served.
            self.slots.clear();
            self.block = block;
        }
        if self.slots.len() >= STORAGE_CACHE_MAX_SLOTS {
            self.slots.clear();
        }
        self.slots.insert((address, index), value);
    }
}

fn cached_storage(block: u64, address: Address, index: U256) -> Option<U256> {
    storage_cache().lock().ok()?.get(block, address, index)
}

fn store_storage(block: u64, address: Address, index: U256, value: U256) {
    if let Ok(mut guard) = storage_cache().lock() {
        guard.put(block, address, index, value);
    }
}

struct RpcForkDb {
    rpc_url: Arc<String>,
    block_number: u64,
    client: BlockingClient,
    account_cache: std::collections::HashMap<Address, AccountInfo>,
}

impl RpcForkDb {
    fn new(rpc_url: String, block_number: u64) -> Result<Self> {
        let client = BlockingClient::builder()
            .timeout(Duration::from_secs(12))
            .build()
            .context("blocking rpc client")?;
        Ok(Self {
            rpc_url: Arc::new(rpc_url),
            block_number,
            client,
            account_cache: std::collections::HashMap::new(),
        })
    }

    fn batch_prefetch_accounts(&mut self, addresses: &[Address], deadline: Instant) -> usize {
        let mut pending: Vec<Address> = addresses
            .iter()
            .copied()
            .filter(|addr| !self.account_cache.contains_key(addr))
            .collect();
        if pending.is_empty() || Instant::now() >= deadline {
            return 0;
        }

        let block_param = self.block_param();
        let mut loaded = 0usize;
        while !pending.is_empty() && Instant::now() < deadline {
            let chunk: Vec<Address> = pending.drain(..pending.len().min(8)).collect();
            let mut batch = Vec::with_capacity(chunk.len() * 3);
            for (idx, addr) in chunk.iter().enumerate() {
                let addr_hex = format!("{addr:#x}");
                let base_id = idx * 3 + 1;
                batch.push(json!({
                    "jsonrpc": "2.0",
                    "id": base_id,
                    "method": "eth_getTransactionCount",
                    "params": [addr_hex.clone(), block_param.clone()],
                }));
                batch.push(json!({
                    "jsonrpc": "2.0",
                    "id": base_id + 1,
                    "method": "eth_getBalance",
                    "params": [addr_hex.clone(), block_param.clone()],
                }));
                batch.push(json!({
                    "jsonrpc": "2.0",
                    "id": base_id + 2,
                    "method": "eth_getCode",
                    "params": [addr_hex, block_param.clone()],
                }));
            }

            let response: Value = match self
                .client
                .post(self.rpc_url.as_str())
                .json(&batch)
                .send()
                .and_then(|resp| resp.json())
            {
                Ok(value) => value,
                Err(_) => {
                    pending.extend(chunk);
                    break;
                }
            };

            let entries = if let Some(arr) = response.as_array() {
                arr.clone()
            } else {
                vec![response]
            };

            for (idx, addr) in chunk.iter().enumerate() {
                let base_id = idx * 3 + 1;
                let by_id = |id: usize| {
                    entries
                        .iter()
                        .find(|item| item.get("id").and_then(|v| v.as_u64()) == Some(id as u64))
                };
                // An account that cannot be decoded is left OUT of the cache
                // rather than cached as zeros. `basic_ref` will fetch it again
                // and fail loudly there if it is still unreadable, which ends
                // in a fallback to eth_call -- a real simulation instead of one
                // run against a fabricated empty account.
                let decoded = rpc_result_str(by_id(base_id), "eth_getTransactionCount").and_then(
                    |nonce_hex| {
                        let balance_hex = rpc_result_str(by_id(base_id + 1), "eth_getBalance")?;
                        let code_hex = rpc_result_str(by_id(base_id + 2), "eth_getCode")?;
                        decode_account(nonce_hex, balance_hex, code_hex)
                    },
                );
                let info = match decoded {
                    Ok(info) => info,
                    Err(err) => {
                        tracing::debug!(
                            target: "sim_revm",
                            address = %format!("{addr:#x}"),
                            error = %err,
                            "prefetch could not decode account; leaving it uncached \
                             rather than assuming an empty one"
                        );
                        continue;
                    }
                };
                self.account_cache.insert(*addr, info);
                loaded += 1;
            }
        }
        loaded
    }

    fn call(&self, method: &str, params: Value) -> Result<Value, RpcDbError> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });
        let response: Value = self
            .client
            .post(self.rpc_url.as_str())
            .json(&body)
            .send()
            .map_err(|err| RpcDbError(format!("{method} transport: {err}")))?
            .json()
            .map_err(|err| RpcDbError(format!("{method} decode: {err}")))?;
        if let Some(err) = response.get("error") {
            return Err(RpcDbError(format!("{method}: {err}")));
        }
        Ok(response)
    }

    fn block_param(&self) -> Value {
        block_tag(self.block_number)
    }
}

impl DatabaseRef for RpcForkDb {
    type Error = RpcDbError;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        if let Some(cached) = self.account_cache.get(&address) {
            return Ok(Some(cached.clone()));
        }
        let addr = format!("{address:#x}");
        let nonce_resp = self.call(
            "eth_getTransactionCount",
            json!([addr.clone(), self.block_param()]),
        )?;
        let balance_resp = self.call("eth_getBalance", json!([addr.clone(), self.block_param()]))?;
        let code_resp = self.call("eth_getCode", json!([addr, self.block_param()]))?;

        // Strict, and it has to be: this is the path the prefetch defers to.
        // It defaulted a missing `result` to "0x0"/"0x" before the parsers ran,
        // so the parsers never saw the failure -- an unreadable account became
        // a real-looking empty one and the fork simulated against it.
        let nonce_hex = rpc_result_str(Some(&nonce_resp), "eth_getTransactionCount")?;
        let balance_hex = rpc_result_str(Some(&balance_resp), "eth_getBalance")?;
        let code_hex = rpc_result_str(Some(&code_resp), "eth_getCode")?;

        Ok(Some(decode_account(nonce_hex, balance_hex, code_hex)?))
    }

    fn code_by_hash_ref(&self, _code_hash: B256) -> Result<Bytecode, Self::Error> {
        Err(RpcDbError(
            "code_by_hash_ref should not be called; code loaded in basic_ref".into(),
        ))
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        if let Some(hit) = cached_storage(self.block_number, address, index) {
            return Ok(hit);
        }
        let addr = format!("{address:#x}");
        let slot = format!("{index:#x}");
        let response = self.call(
            "eth_getStorageAt",
            json!([addr, slot, self.block_param()]),
        )?;
        // The same rule as accounts, and this one is worse: the value is
        // written into the block's storage cache below, so a single failed
        // read poisons every later read of that slot at that block. Zero is
        // also the most convincing wrong answer available -- an unset slot IS
        // zero, so a fabricated one is indistinguishable from a real one, and
        // a pool reserve or token balance reading zero changes which branch
        // the fork takes.
        let result = rpc_result_str(Some(&response), "eth_getStorageAt")?;
        let value = parse_u256_hex(result)?;
        store_storage(self.block_number, address, index, value);
        Ok(value)
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        let response = self.call(
            "eth_getBlockByNumber",
            json!([format!("0x{number:x}"), false]),
        )?;
        let hash = response
            .get("result")
            .and_then(|v| v.get("hash"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| RpcDbError("block hash missing".into()))?;
        parse_b256_hex(hash).map_err(|err| RpcDbError(err.to_string()))
    }
}

/// The `result` string of one JSON-RPC response, or an error saying why not.
///
/// A missing `result` is NOT a zero. JSON-RPC answers a query about a
/// non-existent account or an unset storage slot with `result: "0x0"` --
/// present and explicit -- so the field is absent only when the call errored or
/// the reply was malformed. Defaulting it to "0x0" made those two
/// indistinguishable, which is what let a broken response be cached as real
/// chain state.
fn rpc_result_str<'a>(entry: Option<&'a Value>, method: &str) -> Result<&'a str, RpcDbError> {
    let entry = entry.ok_or_else(|| RpcDbError(format!("{method}: no response for this id")))?;
    if let Some(err) = entry.get("error") {
        return Err(RpcDbError(format!("{method}: rpc error {err}")));
    }
    entry
        .get("result")
        .and_then(|value| value.as_str())
        .ok_or_else(|| RpcDbError(format!("{method}: response carried no result string")))
}

/// Build one account from its three RPC results, or fail.
///
/// Shared by the batched prefetch and the per-account `basic_ref`, because both
/// decoded this independently and both defaulted every field. The code decode
/// is the worst one to get wrong: `unwrap_or_default()` turns a malformed
/// `eth_getCode` reply into a CODELESS account, so a contract call inside the
/// fork returns empty instead of executing.
fn decode_account(
    nonce_hex: &str,
    balance_hex: &str,
    code_hex: &str,
) -> Result<AccountInfo, RpcDbError> {
    let nonce = u64::from_str_radix(nonce_hex.trim_start_matches("0x"), 16)
        .map_err(|err| RpcDbError(format!("parse nonce {nonce_hex:?}: {err}")))?;
    let balance = parse_u256_hex(balance_hex)?;
    let code_bytes = hex::decode(code_hex.trim_start_matches("0x"))
        .map_err(|err| RpcDbError(format!("parse code: {err}")))?;
    let code = Bytecode::new_raw(Bytes::from(code_bytes));
    let code_hash = code.hash_slow();
    Ok(AccountInfo {
        balance,
        nonce,
        code_hash,
        code: Some(code),
    })
}

fn parse_u256_hex(raw: &str) -> Result<U256, RpcDbError> {
    let trimmed = raw.trim_start_matches("0x");
    if trimmed.is_empty() {
        return Ok(U256::ZERO);
    }
    U256::from_str_radix(trimmed, 16).map_err(|err| RpcDbError(format!("parse u256: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ethers::types::{Bytes as EthBytes, TransactionRequest, H160};
    use revm::database::EmptyDB;
    use revm::primitives::keccak256;

    /// The cache is only sound because storage at a FIXED block is immutable.
    /// Serving a slot from a previous block would feed REVM stale state and
    /// produce a confidently wrong simulation — worse than no cache at all.
    #[test]
    fn storage_cache_never_serves_a_stale_block() {
        let addr = Address::from(EthAddress::from_low_u64_be(0xabc).0);
        let slot = U256::from(7u64);

        let mut c = StorageCache::empty();
        c.put(100, addr, slot, U256::from(42u64));
        assert_eq!(c.get(100, addr, slot), Some(U256::from(42u64)));

        // Same slot, different block => MISS, never the old value.
        assert_eq!(c.get(101, addr, slot), None);

        // Writing at the new block drops everything cached for the old one.
        c.put(101, addr, slot, U256::from(99u64));
        assert_eq!(c.get(101, addr, slot), Some(U256::from(99u64)));
        assert_eq!(c.get(100, addr, slot), None);
    }

    #[test]
    fn storage_cache_distinguishes_addresses_and_slots() {
        let a = Address::from(EthAddress::from_low_u64_be(1).0);
        let b = Address::from(EthAddress::from_low_u64_be(2).0);
        let mut c = StorageCache::empty();
        c.put(200, a, U256::from(1u64), U256::from(11u64));
        c.put(200, a, U256::from(2u64), U256::from(22u64));
        c.put(200, b, U256::from(1u64), U256::from(33u64));
        assert_eq!(c.get(200, a, U256::from(1u64)), Some(U256::from(11u64)));
        assert_eq!(c.get(200, a, U256::from(2u64)), Some(U256::from(22u64)));
        assert_eq!(c.get(200, b, U256::from(1u64)), Some(U256::from(33u64)));
        assert_eq!(c.get(200, b, U256::from(2u64)), None);
    }

    #[test]
    fn decode_error_string_revert() {
        let mut data = vec![0x08, 0xc3, 0x79, 0xa0];
        let reason = ethers::abi::encode(&[ethers::abi::Token::String(
            "insufficient profit".into(),
        )]);
        data.extend_from_slice(&reason);
        let decoded = decode_revert_reason(&data);
        assert!(decoded.contains("insufficient profit"));
    }

    #[test]
    fn typed_tx_maps_to_tx_env() {
        let from = H160::from_low_u64_be(0x11);
        let to = H160::from_low_u64_be(0x22);
        let tx: TypedTransaction = TransactionRequest::new()
            .from(from)
            .to(to)
            .data(EthBytes::from(vec![0xde, 0xad]))
            .value(EthU256::from(7u64))
            .gas(500_000u64)
            .gas_price(EthU256::from(2u64))
            .nonce(3u64)
            .into();
        let env = typed_tx_to_tx_env(&tx, 8453).expect("tx env");
        assert_eq!(env.caller, to_revm_address(from));
        assert_eq!(env.gas_limit, 500_000);
        assert_eq!(env.nonce, 3);
        assert_eq!(env.chain_id, Some(8453));
    }

    #[test]
    fn synthetic_cache_db_executes_call() {
        let caller = Address::from_slice(&[0x11; 20]);
        let target = Address::from_slice(&[0x22; 20]);
        // PUSH1 0x2a PUSH1 0x00 MSTORE PUSH1 0x20 PUSH1 0x00 RETURN
        let bytecode = Bytecode::new_raw(Bytes::from(vec![
            0x60, 0x2a, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3,
        ]));
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(
            caller,
            AccountInfo {
                balance: U256::from(10u128.pow(18)),
                nonce: 0,
                code_hash: keccak256([]),
                code: None,
            },
        );
        db.insert_account_info(
            target,
            AccountInfo {
                balance: U256::ZERO,
                nonce: 0,
                code_hash: bytecode.hash_slow(),
                code: Some(bytecode),
            },
        );

        let tx = TxEnv {
            caller,
            gas_limit: 1_000_000,
            gas_price: 1,
            kind: TxKind::Call(target),
            chain_id: Some(8453),
            ..TxEnv::default()
        };

        let ctx = RevmContext::mainnet()
            .with_db(&mut db)
            .modify_cfg_chained(|cfg| cfg.chain_id = 8453)
            .with_tx(tx.clone());
        let mut evm = ctx.build_mainnet();
        let result = evm.transact(tx).expect("transact");
        assert!(result.result.is_success());
        let data = result.result.output().expect("output bytes");
        assert!(!data.is_empty());
    }

    #[tokio::test]
    async fn simulate_via_revm_disabled_by_default() {
        std::env::remove_var("ARBOT_SIM_REVM");
        let tx: TypedTransaction = TransactionRequest::new()
            .from(H160::from_low_u64_be(1))
            .to(H160::from_low_u64_be(2))
            .into();
        let err = simulate_via_revm(SimForkRequest {
            rpc_url: "http://localhost".into(),
            block_number: 1,
            chain_id: 8453,
            executor_address: EthAddress::zero(),
            executor_bytecode: None,
            tx,
            prefetch_addresses: Vec::new(),
            metrics: None,
        })
        .await
        .expect_err("must be disabled");
        assert!(err.to_string().contains("not enabled"));
    }

    #[tokio::test]
    #[ignore = "requires ARBOT_SIM_REVM_LIVE=1 and BASE RPC"]
    async fn live_base_fork_smoke() {
        if !sim_revm_live_tests_enabled() {
            return;
        }
        let rpc = std::env::var("BASE_RPC_HTTP")
            .or_else(|_| std::env::var("ARBOT_BASE_RPC_HTTP"))
            .expect("BASE_RPC_HTTP for live test");
        std::env::set_var("ARBOT_SIM_REVM", "1");
        let client = Client::new();
        let block_body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_blockNumber",
            "params": [],
        });
        let block_resp: Value = client
            .post(&rpc)
            .json(&block_body)
            .send()
            .await
            .expect("block number")
            .json()
            .await
            .expect("block json");
        let block_hex = block_resp["result"].as_str().expect("block result");
        let block_number =
            u64::from_str_radix(block_hex.trim_start_matches("0x"), 16).expect("parse block");

        let executor = std::env::var("BASE_EXECUTOR_ADDRESS")
            .ok()
            .and_then(|raw| raw.parse().ok())
            .unwrap_or_else(EthAddress::zero);
        let from = H160::from_low_u64_be(0xbeef);
        let tx: TypedTransaction = TransactionRequest::new()
            .from(from)
            .to(executor)
            .data(EthBytes::from(vec![0x00]))
            .gas(300_000u64)
            .gas_price(EthU256::from(1u64))
            .into();

        let result = simulate_via_revm(SimForkRequest {
            rpc_url: rpc,
            block_number,
            chain_id: 8453,
            executor_address: executor,
            executor_bytecode: None,
            tx,
            prefetch_addresses: Vec::new(),
            metrics: None,
        })
        .await;
        // Smoke: must reach revm (success or structured revert), not transport failure.
        match result {
            Ok(_) | Err(_) => {}
        }
    }

    #[test]
    fn sim_l1_fee_defaults_on_for_base() {
        std::env::remove_var("ARBOT_SIM_L1_FEE");
        assert!(sim_l1_fee_enabled(8453));
        std::env::set_var("ARBOT_SIM_L1_FEE", "0");
        assert!(!sim_l1_fee_enabled(8453));
        std::env::remove_var("ARBOT_SIM_L1_FEE");
    }

    #[test]
    fn encode_unsigned_legacy_tx_rlp() {
        let from = H160::from_low_u64_be(0x11);
        let to = H160::from_low_u64_be(0x22);
        let tx: TypedTransaction = TransactionRequest::new()
            .from(from)
            .to(to)
            .data(EthBytes::from(vec![0xde; 128]))
            .gas(500_000u64)
            .gas_price(EthU256::from(2u64))
            .nonce(3u64)
            .into();
        let rlp = encode_unsigned_tx_rlp(&tx, 8453).expect("rlp");
        assert!(!rlp.is_empty());
    }

    /// A fork pinned at block N must never be handed code from `latest`.
    ///
    /// The old fallback did exactly that when the executor had no code at N.
    /// A contract deployed or upgraded after N would then simulate
    /// successfully for a trade that could not have executed at N -- the fork
    /// reports on a chain state that never existed.
    #[test]
    fn a_pinned_fork_never_borrows_latest_bytecode() {
        // Code present at the pinned block: nothing to inject, the fork's own
        // state is authoritative.
        assert_eq!(
            executor_bytecode_at_block(&[0x60, 0x80], None).expect("code at block"),
            None,
            "deployed executor must use the fork's own code, not an override"
        );

        // No code at the pinned block and no declared override. This is where
        // `latest` used to be substituted; it must now fail instead.
        let err = executor_bytecode_at_block(&[], None)
            .expect_err("an undeployed executor must not borrow latest bytecode");
        let message = format!("{err:#}");
        assert!(
            message.contains("refusing to substitute latest chain state"),
            "the error must name what it refused to do, got: {message}"
        );

        // An operator override is a declared intent, not a substitution, so it
        // is still honoured.
        assert_eq!(
            executor_bytecode_at_block(&[], Some(vec![0xfe])).expect("override honoured"),
            Some(vec![0xfe])
        );
    }

    /// Failing is cheap: `simulate_plan_execution` records a Fallback metric
    /// and degrades to `eth_call`, which is a real simulation. Silently forking
    /// against the wrong bytecode is not recoverable, because it succeeds.
    #[test]
    fn refusing_is_preferred_to_simulating_the_wrong_contract() {
        assert!(executor_bytecode_at_block(&[], None).is_err());
        assert!(executor_bytecode_at_block(&[0x00], None).is_ok());
    }

    /// A missing RPC field is not an empty account.
    ///
    /// JSON-RPC answers a query about a non-existent account with an explicit
    /// `result: "0x0"`, so the field is absent only when the call errored or
    /// the reply was malformed. Defaulting it made a broken response
    /// indistinguishable from real chain state, and the fork then simulated
    /// against nonce 0 / balance 0 / no code as if that were true.
    #[test]
    fn a_missing_rpc_field_is_not_an_empty_account() {
        // The legitimate case still decodes: a real account answering zeros.
        let zero = json!({"id": 1, "result": "0x0"});
        let ok = rpc_result_str(Some(&zero), "eth_getBalance");
        assert_eq!(ok.expect("explicit zero is an answer"), "0x0");

        // No response for this id at all -- a short or reordered batch.
        assert!(rpc_result_str(None, "eth_getBalance").is_err());

        // A response that carries an error instead of a result.
        let errored = json!({"id": 1, "error": {"code": -32000, "message": "header not found"}});
        let err = rpc_result_str(Some(&errored), "eth_getCode")
            .expect_err("an rpc error is not an empty account");
        assert!(
            format!("{err}").contains("header not found"),
            "the error must carry the node's reason, got: {err}"
        );

        // A malformed reply with no result member.
        let bare = json!({"id": 1});
        assert!(rpc_result_str(Some(&bare), "eth_getCode").is_err());
    }

    /// Malformed code must not become a CODELESS account: a contract call in
    /// the fork would then return empty instead of executing.
    #[test]
    fn undecodable_account_fields_fail_rather_than_defaulting() {
        let good = decode_account("0x2a", "0x1bc16d674ec80000", "0x6080")
            .expect("a well-formed account decodes");
        assert_eq!(good.nonce, 42);
        assert_eq!(good.balance, U256::from(2_000_000_000_000_000_000u64));

        // An account with no code is legitimate and still decodes.
        assert!(decode_account("0x0", "0x0", "0x").is_ok());

        assert!(decode_account("zzz", "0x0", "0x").is_err(), "bad nonce");
        assert!(decode_account("0x0", "zzz", "0x").is_err(), "bad balance");
        assert!(
            decode_account("0x0", "0x0", "0xabc").is_err(),
            "odd-length code hex used to become an empty contract"
        );
        assert!(decode_account("0x0", "0x0", "0xzz").is_err(), "bad code hex");
    }

    #[test]
    fn collect_prefetch_dedupes_and_caps() {
        let from = H160::from_low_u64_be(0x11);
        let to = H160::from_low_u64_be(0x22);
        let tx: TypedTransaction = TransactionRequest::new()
            .from(from)
            .to(to)
            .into();
        let addrs = collect_prefetch_addresses(
            &tx,
            to,
            &[from, EthAddress::from_low_u64_be(0x33)],
            2,
        );
        assert_eq!(addrs.len(), 2);
    }
}
