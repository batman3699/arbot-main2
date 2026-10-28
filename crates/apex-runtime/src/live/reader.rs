//! The live side of §24.6's eleven checks (Task 8.5 R8), read from a view
//! refreshed once a block rather than once a ticket.
//!
//! # Read once a block, not once a ticket
//!
//! Last-mile runs immediately before signing, on the capture path; a reader
//! that made four RPC calls there would spend a good part of a two-second block
//! on them. So the head task refreshes a [`ChainView`] once a block — the
//! executor's code, the signer's balance and pending nonce, the lender's holding
//! of the loan token, the base fee and the measured block gas — and a ticket
//! reads it without a call. It is at most a block old, and a view older than
//! the reader's maximum age is refused as stale: one the head task stopped
//! refreshing is not a reading.
//!
//! # What each check reads
//!
//! | Check | Live value | From |
//! |---|---|---|
//! | executor fingerprint | the configured executor, at the plan version it speaks **only while its code hash is the configured one**, else 0 | `eth_getCode` |
//! | min profit | policy | configuration |
//! | fee ceiling | the head's base fee — the plane bids no priority fee — against policy | the head |
//! | gas eligibility | the capacity model's largest measured window; **0** while it is `Unknown`, so the check fails closed | R5's sampler |
//! | signer balance | `eth_getBalance` | per block |
//! | flash availability | the lender's `balanceOf` in the loan token, against the ticket's input | per block |
//! | hooks | none: no venue on these routes has one | — |
//! | critical pool state | the book's versions over **the route's own pools** | the book, live |
//! | nonce | `eth_getTransactionCount(…, pending)` | per block |
//!
//! The executor's version is §26.2's, and the chain has no function that states
//! it; its code is what says which executor runs. A code hash other than the
//! configured one — a redeploy, an upgrade, no code at all — reads as version 0,
//! and check 2 refuses every ticket until the configuration says otherwise.

use crate::live::abi::{self, selector};
use crate::live::book::PoolBook;
use crate::live::reads::{ChainReads, ReadError};
use crate::plane::{Decline, LiveReader, LiveReadings};
use alloy_primitives::{Address, B256};
use apex_chain::rpc::ws::Head;
use apex_chain::rpc::RpcTransport;
use apex_state::Versioned;
use apex_types::state::ReconstructionStatus;
use apex_types::ticket::OpportunityTicket;
use apex_types::time::DurationNanos;
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What the chain looked like at one head, for every ticket until the next.
#[derive(Clone, Debug, PartialEq)]
pub struct ChainView {
    pub block: u64,
    pub base_fee_wei: u128,
    pub executor_code_hash: B256,
    pub signer_balance_wei: u128,
    pub signer_pending_nonce: u64,
    /// The lender's holding of the loan token.
    pub lender_holding: u128,
    /// Morpho Blue's holding of the loan token, which the Morpho lender lends
    /// (R27); 0 when the Morpho lender is not configured.
    pub morpho_holding: u128,
    /// R37: Morpho Blue's USDC, and PoolManager's WETH and USDC: the armed
    /// lane's USDC and v4 lenders. 0 when unread; nothing else depends on
    /// them.
    pub morpho_usdc_holding: u128,
    pub pool_manager_holdings: (u128, u128),
    /// The capacity model's largest measured window; 0 while it is `Unknown`.
    pub block_gas: u64,
    /// R28: `executors(signer)` and `isCircuitOpen()`, the owner's two stops.
    pub executor_allows_signer: bool,
    pub circuit_open: bool,
    pub refreshed_at: Instant,
}

/// Who and what the readings are about. Configuration, stated once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReaderConfig {
    pub executor: Address,
    /// The code the executor must be running to be the executor committed to.
    pub executor_code_hash: B256,
    /// The plan version that executor speaks (`PLAN_VERSION_V2`).
    pub plan_version: u32,
    pub signer: Address,
    pub lender: Address,
    /// Morpho Blue, when the Morpho lender is configured: its holding is that
    /// lender's (R27).
    pub morpho: Option<Address>,
    pub loan_token: Address,
    pub fee_ceiling_wei: u128,
    pub min_profit: i128,
    /// A view older than this is stale.
    pub max_age: Duration,
}

pub struct ChainReader {
    rpc: Arc<dyn RpcTransport>,
    book: Arc<PoolBook>,
    config: ReaderConfig,
    view: Versioned<Option<ChainView>>,
}

fn quantity_u128(v: &serde_json::Value) -> Option<u128> {
    u128::from_str_radix(v.as_str()?.strip_prefix("0x")?, 16).ok()
}

impl ChainReader {
    pub fn new(rpc: Arc<dyn RpcTransport>, book: Arc<PoolBook>, config: ReaderConfig) -> Self {
        Self { rpc, book, config, view: Versioned::new(None, ReconstructionStatus::Rebuilding) }
    }

    pub fn view(&self) -> Option<ChainView> {
        (*self.view.load().value).clone()
    }

    /// Read the view at `head`: four reads, concurrently. `block_gas` is the
    /// capacity model's largest measured window, or 0 while it is `Unknown`.
    /// A failed read leaves the last view in place — to age out, not to be
    /// patched with a guess.
    pub async fn refresh(&self, head: &Head, block_gas: u64) -> Result<(), ReadError> {
        let reads = ChainReads::new(Arc::clone(&self.rpc));
        let c = &self.config;
        let at = format!("{:#x}", head.number);
        let base_fee = head.base_fee_per_gas.ok_or_else(|| ReadError::Malformed("a head without a base fee".into()))?;
        let lender = [(c.loan_token, abi::call_address(selector::BALANCE_OF, c.lender))];
        // R28: the owner's two stops, read with everything else.
        let switches = [
            (c.executor, abi::call_address(selector::EXECUTORS, c.signer)),
            (c.executor, abi::call0(selector::IS_CIRCUIT_OPEN)),
        ];
        let (code, balance, nonce, holding, switched) = tokio::join!(
            reads.code_hash(c.executor, head.number),
            self.rpc.call("eth_getBalance", json!([c.signer, at])),
            self.rpc.call("eth_getTransactionCount", json!([c.signer, "pending"])),
            reads.multicall(&lender, head.number),
            reads.multicall(&switches, head.number),
        );
        let malformed = |what: &str| ReadError::Malformed(format!("{what} did not decode"));
        let switched = switched?;
        let flag = |i: usize, what: &str| {
            switched
                .get(i)
                .cloned()
                .flatten()
                .and_then(|d| abi::word_uint(&d, 0, 8))
                .map(|v| v != 0)
                .ok_or_else(|| malformed(what))
        };
        let (morpho_holding, morpho_usdc_holding, pool_manager_holdings) = match c.morpho {
            None => (0, 0, (0, 0)),
            Some(m) => {
                let pm = crate::live::v4::POOL_MANAGER;
                let usdc = apex_exec::lean2::USDC;
                let got = reads
                    .multicall(
                        &[
                            (c.loan_token, abi::call_address(selector::BALANCE_OF, m)),
                            (usdc, abi::call_address(selector::BALANCE_OF, m)),
                            (crate::live::frontier::WETH, abi::call_address(selector::BALANCE_OF, pm)),
                            (usdc, abi::call_address(selector::BALANCE_OF, pm)),
                        ],
                        head.number,
                    )
                    .await?;
                let at = |i: usize| got.get(i).cloned().flatten().and_then(|d| abi::word_uint(&d, 0, 128));
                (at(0).ok_or_else(|| malformed("Morpho's balance"))?, at(1).unwrap_or(0), (at(2).unwrap_or(0), at(3).unwrap_or(0)))
            }
        };
        let view = ChainView {
            block: head.number,
            base_fee_wei: base_fee,
            executor_code_hash: code?,
            signer_balance_wei: quantity_u128(&balance.map_err(ReadError::Rpc)?).ok_or_else(|| malformed("balance"))?,
            signer_pending_nonce: u64::try_from(
                quantity_u128(&nonce.map_err(ReadError::Rpc)?).ok_or_else(|| malformed("nonce"))?,
            )
            .map_err(|_| malformed("nonce"))?,
            lender_holding: holding?
                .first()
                .cloned()
                .flatten()
                .and_then(|d| abi::word_uint(&d, 0, 128))
                .ok_or_else(|| malformed("the lender's balance"))?,
            morpho_holding,
            morpho_usdc_holding,
            pool_manager_holdings,
            block_gas,
            executor_allows_signer: flag(0, "executors(signer)")?,
            circuit_open: flag(1, "isCircuitOpen()")?,
            refreshed_at: Instant::now(),
        };
        self.view.store(Some(view), ReconstructionStatus::Verified);
        Ok(())
    }
}

#[async_trait::async_trait]
impl LiveReader for ChainReader {
    async fn read(&self, t: &OpportunityTicket) -> Result<LiveReadings, Decline> {
        let stale = |age: Duration| Decline::StaleState {
            age: DurationNanos(u64::try_from(age.as_nanos()).unwrap_or(u64::MAX)),
        };
        let Some(v) = self.view() else { return Err(stale(Duration::MAX)) };
        let age = v.refreshed_at.elapsed();
        if age > self.config.max_age {
            return Err(stale(age));
        }
        let c = &self.config;
        let pools: Vec<Address> = t.route_commitment.hops.iter().map(|h| h.pool.address).collect();
        Ok(LiveReadings {
            executor: c.executor.into_array(),
            // Version 0, which check 2 refuses, unless this is the configured
            // code, it still lets the signer trade, and the owner has not opened
            // its circuit (R28).
            executor_version: if v.executor_code_hash == c.executor_code_hash
                && v.executor_allows_signer
                && !v.circuit_open
            {
                c.plan_version
            } else {
                0
            },
            observed_fee_wei: v.base_fee_wei,
            fee_ceiling_wei: c.fee_ceiling_wei,
            remaining_block_gas: v.block_gas,
            signer_balance_wei: v.signer_balance_wei,
            flash_required: u128::try_from(t.exact_input).unwrap_or(u128::MAX),
            // The holding of the lender the ticket borrows from (R27).
            flash_available: if t.flash_source.as_ref().is_some_and(|f| f.provider == crate::live::lenders::MORPHO_FLASH) {
                v.morpho_holding
            } else {
                v.lender_holding
            },
            live_hooks: None,
            live_venue_versions: self.book.versions_for(&pools),
            chain_pending_nonce: v.signer_pending_nonce,
            min_profit: c.min_profit,
        })
    }
}
