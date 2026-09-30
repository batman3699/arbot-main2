//! Key custody and signing (§18.4, §43, INV-46) — the `Signer` port (Task 8.5).
//!
//! # Where the key lives, and why here
//!
//! §18.4: "Phase 6 ships in-process keys with a `Secret<LocalWallet>` wrapper
//! (INV-46); an out-of-process signer is a Phase 15+ option gated on measured
//! latency cost." `apex-capture::signer` owns lanes, nonces and per-lane health;
//! the key itself is an operational input, and Phase 6 deliberately held none —
//! "holding a key in a type that never uses it would be an unnecessary copy of
//! the most dangerous value in the system." This is the code that uses it, so
//! this is where it lands: next to the port it serves, in the wiring crate,
//! rather than adding `ethers-signers` to `apex-capture`'s lean core.
//!
//! # What INV-46 asks of this module
//!
//! The key is parsed once, into a [`Secret`], and the string it came from is not
//! kept. [`LaneKey`]'s `Debug` prints the address and nothing else. A malformed
//! key is refused with [`KeyError::NotAKey`], which carries no part of the input —
//! a key with one typo in it is still nearly a key, and an error that echoed it
//! would put it in a log line.
//!
//! # What the port refuses
//!
//! - **A lane it holds no key for.** The authorization names the lane its nonce
//!   was reserved on; signing with a different key would sign a nonce that lane
//!   does not own.
//! - **A call committed for another chain.** The executor would revert it at
//!   best, and `wrong_chain_submission` is a hard-zero counter.
//! - **A zero fee cap.** No block includes it; signing it can only mean the
//!   readings were wrong, and it would spend a reservation on nothing.

use crate::plane::{Decline, FeeCaps, Signer};
use alloy_primitives::{keccak256, Address, B256};
use apex_capture::revalidate::SigningAuthorization;
use apex_chain::adapter::SignedPayload;
use apex_config::Secret;
use apex_exec::call::ExecutorCall;
use apex_types::cost::GasLimit;
use apex_types::ids::{ChainId, SignerLaneId};
use ethers_core::types::transaction::eip2718::TypedTransaction;
use ethers_core::types::{Bytes, Eip1559TransactionRequest, H160, U256 as EthersU256};
use ethers_signers::{LocalWallet, Signer as _};
use std::collections::BTreeMap;

/// Why a key was refused. **Carries nothing from the input.**
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyError {
    /// Not 32 bytes of hex, or not a valid secp256k1 scalar.
    NotAKey,
    /// The signature could not be produced. With a valid key this is RFC 6979
    /// landing on a degenerate nonce — astronomically unlikely, and reported
    /// rather than unwrapped.
    Unsignable,
}

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAKey => f.write_str("not a secp256k1 private key (value withheld)"),
            Self::Unsignable => f.write_str("the key could not sign this transaction"),
        }
    }
}

impl std::error::Error for KeyError {}

/// One signer lane's key. `ExecutionSigner` in §18.1's terms: it signs
/// arbitrage transactions and nothing else.
pub struct LaneKey {
    wallet: Secret<LocalWallet>,
    address: Address,
}

impl std::fmt::Debug for LaneKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaneKey").field("address", &self.address).finish_non_exhaustive()
    }
}

/// The fields of one EIP-1559 transaction, as this system builds them.
///
/// No `value`: `startV2` is not payable, and a field for value would be a place
/// to put some. No access list: nothing here has measured what one saves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Eip1559Tx {
    pub chain_id: u64,
    pub nonce: u64,
    pub max_priority_fee_per_gas: u128,
    pub max_fee_per_gas: u128,
    pub gas_limit: u64,
    pub to: Address,
    pub data: Vec<u8>,
}

/// A signed transaction: the exact bytes that would be broadcast, and their
/// hash — which is the transaction hash, and what settlement is looked up by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedTx {
    pub raw: Vec<u8>,
    pub hash: B256,
}

impl LaneKey {
    /// Parse a hex private key, with or without `0x`.
    ///
    /// Takes a `Secret` so the call site reads as a deliberate act and the value
    /// arrived already wrapped.
    ///
    /// **The parser is the one validator**: 32 bytes of hex, and a scalar in
    /// range — zero and values at or above the group order are refused. Its error
    /// is discarded rather than wrapped, because some of its messages quote the
    /// offending character and position, which for a key with one typo is most of
    /// a key. A first draft pre-checked "exactly 64 hex digits" as well; mutation
    /// removed it and nothing failed, because the parser already refuses all of
    /// it — a guard that reads as load-bearing and is not.
    ///
    /// `trim` only: a key pasted with a trailing newline is the same key.
    pub fn from_hex(secret: &Secret<String>) -> Result<Self, KeyError> {
        let wallet: LocalWallet =
            secret.expose().trim().parse().map_err(|_| KeyError::NotAKey)?;
        let address = Address::from(wallet.address().0);
        Ok(Self { wallet: Secret::new(wallet), address })
    }

    /// The address this key signs as — derived, never declared.
    pub const fn address(&self) -> Address {
        self.address
    }

    /// Sign one EIP-1559 transaction.
    ///
    /// Held byte-for-byte to an independent implementation (`cast mktx`) by
    /// `tests/local_signer.rs`: ECDSA under RFC 6979 is deterministic, so two
    /// correct signers must agree exactly.
    pub fn sign_eip1559(&self, tx: &Eip1559Tx) -> Result<SignedTx, KeyError> {
        let request = Eip1559TransactionRequest::new()
            .chain_id(tx.chain_id)
            .nonce(tx.nonce)
            .max_priority_fee_per_gas(EthersU256::from(tx.max_priority_fee_per_gas))
            .max_fee_per_gas(EthersU256::from(tx.max_fee_per_gas))
            .gas(tx.gas_limit)
            .to(H160(tx.to.into_array()))
            .data(Bytes::from(tx.data.clone()));
        let typed = TypedTransaction::Eip1559(request);
        let signature = self
            .wallet
            .expose()
            .sign_transaction_sync(&typed)
            .map_err(|_| KeyError::Unsignable)?;
        let raw = typed.rlp_signed(&signature).to_vec();
        let hash = keccak256(&raw);
        Ok(SignedTx { raw, hash })
    }
}

/// The `Signer` port over in-process lane keys.
pub struct LocalSigner {
    chain: ChainId,
    keys: BTreeMap<SignerLaneId, LaneKey>,
}

impl std::fmt::Debug for LocalSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalSigner")
            .field("chain", &self.chain.0)
            .field("lanes", &self.keys)
            .finish()
    }
}

impl LocalSigner {
    pub fn new(chain: ChainId) -> Self {
        Self { chain, keys: BTreeMap::new() }
    }

    pub fn with_lane(mut self, lane: SignerLaneId, key: LaneKey) -> Self {
        self.keys.insert(lane, key);
        self
    }

    /// The address a lane signs as, for building the pool's `LaneConfig` from the
    /// key rather than from a second, independently configured value.
    pub fn address(&self, lane: SignerLaneId) -> Option<Address> {
        self.keys.get(&lane).map(LaneKey::address)
    }

    pub fn lanes(&self) -> Vec<(SignerLaneId, Address)> {
        self.keys.iter().map(|(id, k)| (*id, k.address())).collect()
    }
}

impl Signer for LocalSigner {
    fn sign(
        &self,
        auth: &SigningAuthorization,
        call: &ExecutorCall,
        gas_limit: GasLimit,
        fees: FeeCaps,
    ) -> Result<SignedPayload, Decline> {
        let reserved = auth.nonce();
        let Some(key) = self.keys.get(&reserved.lane()) else {
            return Err(Decline::Unsigned {
                detail: format!("no key is held for signer lane {}", reserved.lane().0),
            });
        };
        if call.chain_id() != self.chain.0 {
            return Err(Decline::Unsigned {
                detail: format!(
                    "the call is committed for chain {} and this signer signs for chain {}",
                    call.chain_id(),
                    self.chain.0
                ),
            });
        }
        if fees.max_fee_per_gas == 0 {
            return Err(Decline::Unsigned {
                detail: "a zero fee cap is a transaction no block includes".to_string(),
            });
        }

        let signed = key
            .sign_eip1559(&Eip1559Tx {
                chain_id: self.chain.0,
                nonce: reserved.get(),
                max_priority_fee_per_gas: fees.max_priority_fee_per_gas,
                max_fee_per_gas: fees.max_fee_per_gas,
                gas_limit: gas_limit.0,
                to: call.to(),
                data: call.data().to_vec(),
            })
            .map_err(|e| Decline::Unsigned { detail: e.to_string() })?;

        Ok(SignedPayload {
            chain: self.chain,
            hash: signed.hash,
            nonce: reserved.get(),
            gas_limit,
            raw: signed.raw,
        })
    }
}
