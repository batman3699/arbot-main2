//! Independent cross-checking of pre-broadcast simulations.
//!
//! The execution path simulates plans with `eth_call` through the primary
//! (failover) RPC transport. A compromised or stale primary endpoint could
//! fabricate profit and trigger a live dispatch. When more than one HTTP
//! endpoint is configured for a chain, this module re-runs the same `eth_call`
//! against every *other* endpoint and refuses to confirm when any independent
//! endpoint contradicts the primary result.
//!
//! "The same" includes the BLOCK. Verifiers are pinned to the block the primary
//! simulated at, because a call against a different state is a second opinion
//! about a different question -- see `verify`.
//!
//! Modes (env `ARBOT_SIM_QUORUM_MODE`):
//!   * `best_effort` (default): contradictions veto the trade; verifier
//!     transport failures are tolerated (logged + surfaced) so trading does
//!     not halt on a flaky secondary endpoint. The on-chain `min_profit`
//!     revert guard remains the hard backstop.
//!   * `strict`: additionally requires at least one independent confirmation;
//!     if no verifier responds, the trade is rejected (fail closed).
//!   * `off`: verification disabled (single-endpoint deployments).

use std::time::Duration;

use anyhow::{anyhow, Result};
use ethers::providers::{Http, Middleware, Provider};
use ethers::types::transaction::eip2718::TypedTransaction;
use ethers::types::{BlockId, BlockNumber, U256, U64};
use tracing::{info, warn};

const QUORUM_MODE_ENV: &str = "ARBOT_SIM_QUORUM_MODE";
const QUORUM_TIMEOUT_ENV: &str = "ARBOT_SIM_QUORUM_TIMEOUT_MS";
const DEFAULT_QUORUM_TIMEOUT_MS: u64 = 1_500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuorumMode {
    Off,
    BestEffort,
    Strict,
}

impl QuorumMode {
    fn from_env() -> Self {
        match std::env::var(QUORUM_MODE_ENV)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "off" | "disabled" | "0" => QuorumMode::Off,
            "strict" => QuorumMode::Strict,
            _ => QuorumMode::BestEffort,
        }
    }
}

#[derive(Debug)]
enum VerifierVerdict {
    /// Call succeeded and the returned profit meets the plan threshold.
    Confirmed { profit: U256 },
    /// Call succeeded (or reverted) with a result that contradicts the
    /// primary simulation: revert, malformed return, or profit below the
    /// threshold the primary claimed to clear.
    Contradicted { detail: String },
    /// Endpoint did not produce a usable answer (transport error / timeout).
    Unavailable { detail: String },
}

pub struct SimQuorum {
    chain: String,
    mode: QuorumMode,
    timeout: Duration,
    verifiers: Vec<(String, Provider<Http>)>,
}

impl SimQuorum {
    /// Build a quorum checker over every distinct configured endpoint.
    /// With fewer than two endpoints there is nothing independent to ask, so
    /// verification is disabled with a loud warning.
    pub fn from_endpoints(chain: &str, endpoints: &[String]) -> Self {
        let mode = QuorumMode::from_env();
        let timeout_ms = std::env::var(QUORUM_TIMEOUT_ENV)
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
            .filter(|ms| *ms > 0)
            .unwrap_or(DEFAULT_QUORUM_TIMEOUT_MS);

        let mut seen = std::collections::HashSet::new();
        let mut verifiers = Vec::new();
        for url in endpoints {
            let trimmed = url.trim();
            if trimmed.is_empty() || !seen.insert(trimmed.to_string()) {
                continue;
            }
            match Provider::<Http>::try_from(trimmed) {
                Ok(provider) => verifiers.push((trimmed.to_string(), provider)),
                Err(err) => warn!(
                    target: "sim_quorum",
                    chain,
                    endpoint = trimmed,
                    error = %err,
                    "failed to build simulation quorum verifier; skipping endpoint"
                ),
            }
        }

        if verifiers.len() < 2 && mode != QuorumMode::Off {
            warn!(
                target: "sim_quorum",
                chain,
                endpoints = verifiers.len(),
                "fewer than two distinct RPC endpoints configured; simulation results cannot be \
independently cross-checked (single-RPC trust). Add fallback RPC URLs to enable quorum verification."
            );
        }

        Self {
            chain: chain.to_string(),
            mode,
            timeout: Duration::from_millis(timeout_ms),
            verifiers,
        }
    }

    /// Quorum verification explicitly off (tests / single-endpoint tooling).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn disabled(chain: &str) -> Self {
        Self {
            chain: chain.to_string(),
            mode: QuorumMode::Off,
            timeout: Duration::from_millis(DEFAULT_QUORUM_TIMEOUT_MS),
            verifiers: Vec::new(),
        }
    }

    pub fn is_active(&self) -> bool {
        self.mode != QuorumMode::Off && self.verifiers.len() >= 2
    }

    /// Cross-check a primary simulation that claimed `profit >= min_profit`.
    ///
    /// `block_number` MUST be the block the primary simulated at. This used to
    /// ask every verifier about `pending`, which is a different state and
    /// therefore not evidence about the candidate at hand: the primary pins to
    /// the block the plan was QUOTED against, and `pending` resolves to head+1.
    /// A scan takes ~8.4s -- about four Base blocks -- so the verifier was
    /// judging the plan against state roughly five blocks newer than the one it
    /// was priced on. The drift measured on that pair is 2 blocks +6.63 bps, 10
    /// blocks -23.70 bps, 20 blocks -45.70 bps, which is more than enough to
    /// flip a verdict in either direction.
    ///
    /// The dangerous direction is the false CONTRADICTION: a contradiction
    /// vetoes the trade even in `best_effort`, so drift on a secondary endpoint
    /// could block a candidate the primary had correctly confirmed.
    ///
    /// A verifier that cannot serve the pinned block answers with a transport
    /// error, which is classified `Unavailable` and tolerated, not
    /// `Contradicted` -- only revert-shaped errors contradict.
    ///
    /// Returns `Ok(confirmations)` when the quorum policy is satisfied and an
    /// error when any independent endpoint contradicts the primary result (or,
    /// in strict mode, when no endpoint could confirm it).
    pub async fn verify(
        &self,
        tx: &TypedTransaction,
        min_profit: U256,
        block_number: U64,
    ) -> Result<usize> {
        if !self.is_active() {
            return Ok(0);
        }

        let futures = self.verifiers.iter().map(|(url, provider)| {
            let url = url.clone();
            async move {
                let verdict = match tokio::time::timeout(
                    self.timeout,
                    provider.call(
                        tx,
                        Some(BlockId::Number(BlockNumber::Number(block_number))),
                    ),
                )
                .await
                {
                    Ok(Ok(raw)) => {
                        if raw.len() < 32 {
                            VerifierVerdict::Contradicted {
                                detail: format!(
                                    "returned {} bytes; expected 32-byte profit",
                                    raw.len()
                                ),
                            }
                        } else {
                            let profit = U256::from_big_endian(&raw[..32]);
                            if profit >= min_profit {
                                VerifierVerdict::Confirmed { profit }
                            } else {
                                VerifierVerdict::Contradicted {
                                    detail: format!(
                                        "profit {profit} below required {min_profit}"
                                    ),
                                }
                            }
                        }
                    }
                    Ok(Err(err)) => {
                        let text = err.to_string();
                        // JSON-RPC execution reverts come back as call errors;
                        // they are a substantive contradiction of the primary
                        // simulation, unlike transport failures.
                        if text.contains("revert")
                            || text.contains("execution reverted")
                            || text.contains("VM execution error")
                        {
                            VerifierVerdict::Contradicted { detail: text }
                        } else {
                            VerifierVerdict::Unavailable { detail: text }
                        }
                    }
                    Err(_) => VerifierVerdict::Unavailable {
                        detail: format!("timeout after {:?}", self.timeout),
                    },
                };
                (url, verdict)
            }
        });

        let results = futures_util::future::join_all(futures).await;

        let mut confirmations = 0usize;
        let mut unavailable = 0usize;
        for (url, verdict) in results {
            // Verifier URLs are provider endpoints with embedded credentials;
            // this value reaches both the log stream and a returned error.
            let safe_url = crate::util::redact_endpoint(&url);
            match verdict {
                VerifierVerdict::Confirmed { profit } => {
                    confirmations += 1;
                    info!(
                        target: "sim_quorum",
                        chain = %self.chain,
                        endpoint = %safe_url,
                        profit = %profit,
                        "independent endpoint confirmed simulation profit"
                    );
                }
                VerifierVerdict::Contradicted { detail } => {
                    warn!(
                        target: "sim_quorum",
                        chain = %self.chain,
                        endpoint = %safe_url,
                        detail = %detail,
                        "independent endpoint CONTRADICTED primary simulation; vetoing dispatch"
                    );
                    return Err(anyhow!(
                        "simulation quorum veto: endpoint {safe_url} contradicted primary result ({detail})"
                    ));
                }
                VerifierVerdict::Unavailable { detail } => {
                    unavailable += 1;
                    warn!(
                        target: "sim_quorum",
                        chain = %self.chain,
                        endpoint = %safe_url,
                        detail = %detail,
                        "simulation quorum verifier unavailable"
                    );
                }
            }
        }

        if self.mode == QuorumMode::Strict && confirmations == 0 {
            return Err(anyhow!(
                "simulation quorum (strict): no independent endpoint confirmed the result \
({unavailable} unavailable); refusing to dispatch on single-RPC trust"
            ));
        }

        Ok(confirmations)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fewer_than_two_endpoints_is_inactive() {
        let quorum =
            SimQuorum::from_endpoints("base", &["http://127.0.0.1:8545".to_string()]);
        assert!(!quorum.is_active());
    }

    #[test]
    fn duplicate_endpoints_are_deduplicated() {
        let quorum = SimQuorum::from_endpoints(
            "base",
            &[
                "http://127.0.0.1:8545".to_string(),
                "http://127.0.0.1:8545".to_string(),
            ],
        );
        assert!(!quorum.is_active());
    }

    #[test]
    fn distinct_endpoints_activate_quorum() {
        let quorum = SimQuorum::from_endpoints(
            "base",
            &[
                "http://127.0.0.1:8545".to_string(),
                "http://127.0.0.1:8546".to_string(),
            ],
        );
        assert!(quorum.is_active());
    }

    #[tokio::test]
    async fn inactive_quorum_verifies_vacuously() {
        let quorum = SimQuorum::disabled("base");
        let tx = TypedTransaction::default();
        let confirmations = quorum
            .verify(&tx, U256::from(1u64), U64::from(123u64))
            .await
            .expect("vacuous pass");
        assert_eq!(confirmations, 0);
    }

    /// The verifier must ask about the same block the primary simulated at.
    ///
    /// It used to ask `pending`, which resolves to head+1 while the primary
    /// pins to the block the plan was quoted against -- roughly five blocks
    /// apart after an ~8.4s scan. A contradiction vetoes the trade even in
    /// `best_effort`, so a verifier judging newer state could block a candidate
    /// the primary had correctly confirmed.
    ///
    /// This pins the signature rather than the wire call, which needs a live
    /// endpoint: `verify` cannot be invoked without naming a block, so the
    /// `pending` default cannot come back by omission.
    #[tokio::test]
    async fn verification_is_pinned_to_a_caller_supplied_block() {
        let quorum = SimQuorum::disabled("base");
        let tx = TypedTransaction::default();
        for block in [1u64, 5_000_000, u64::from(u32::MAX)] {
            assert_eq!(
                quorum
                    .verify(&tx, U256::from(1u64), U64::from(block))
                    .await
                    .expect("disabled quorum passes vacuously at any block"),
                0
            );
        }
    }
}
