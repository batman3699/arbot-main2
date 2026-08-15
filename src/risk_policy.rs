//! Runtime enforcement of the per-chain risk policy declared in `ops/inputs.yaml`.
//!
//! Historically `risk.per_chain` was parsed and validated but never consulted by
//! the trading path ("safety theater"). This module resolves the declared policy
//! into concrete on-path limits that the runner enforces:
//!   * `min_net_profit_usd` / `min_net_profit_native` -> hard floor on net profit
//!     (converted to start-token units per candidate via the live native price),
//!   * `max_gas_units_per_tx` -> rejects candidates whose gas limit exceeds the cap,
//!   * `max_fee_per_gas_cap` -> rejects scans/candidates priced above the cap,
//!   * `max_slippage_bps` / `max_price_impact_bps` -> rejects candidates whose
//!     planned slippage or sized price impact exceed the declared tolerance,
//!   * `must_simulate_before_send` -> dispatch refuses unsimulated plans,
//!   * `revert_penalty_model` -> adds a revert-risk premium (bps of the gas cost)
//!     to the profit threshold so expected revert losses are priced in.

use anyhow::{anyhow, Result};
use ethers::types::U256;

use crate::ops_inputs::{OpsInputs, RevertPenaltyKind, RiskChainConfig};

/// Fully-resolved, enforceable risk limits for one chain.
#[derive(Debug, Clone, Default)]
pub struct RuntimeRiskPolicy {
    /// Hard floor on net profit, in native wei. Converted to start-token units
    /// at candidate evaluation time using the live native price.
    pub min_net_profit_wei: Option<U256>,
    pub max_gas_units_per_tx: Option<u64>,
    pub max_fee_per_gas_cap: Option<U256>,
    pub max_slippage_bps: Option<u32>,
    pub max_price_impact_bps: Option<u32>,
    pub must_simulate_before_send: bool,
    pub revert_penalty: Option<RevertPenalty>,
}

#[derive(Debug, Clone)]
pub enum RevertPenalty {
    Flat { bps: u32 },
    Linear { base_bps: u32, per_gas_bps: u32 },
    Exponential { base_bps: u32, factor: u32 },
}

/// Gas units per "step" used by the linear and exponential revert penalty
/// models. A simple 2-3 hop cycle lands in the 300k-600k range, so a step of
/// 100k gives the linear model meaningful resolution without exploding.
const PENALTY_GAS_STEP: u64 = 100_000;
/// Cap the premium at 100% of the gas cost: the worst realistic revert loss is
/// the full gas spend, so pricing in more than that is pure over-rejection.
const PENALTY_MAX_BPS: u32 = 10_000;

impl RevertPenalty {
    /// Premium in bps of the expected gas cost charged against the profit
    /// threshold to price in revert risk.
    pub fn premium_bps(&self, gas_units: u64) -> u32 {
        let steps = gas_units / PENALTY_GAS_STEP;
        let raw = match self {
            RevertPenalty::Flat { bps } => *bps as u64,
            RevertPenalty::Linear { base_bps, per_gas_bps } => {
                (*base_bps as u64).saturating_add((*per_gas_bps as u64).saturating_mul(steps))
            }
            RevertPenalty::Exponential { base_bps, factor } => {
                let factor = (*factor).max(1) as u64;
                let mut value = *base_bps as u64;
                // One exponent step per 250k gas keeps the curve sane for
                // realistic cycle sizes while still punishing huge plans.
                let exp_steps = (gas_units / (PENALTY_GAS_STEP * 25 / 10)).min(8);
                for _ in 0..exp_steps {
                    value = value.saturating_mul(factor);
                    if value >= PENALTY_MAX_BPS as u64 {
                        break;
                    }
                }
                value
            }
        };
        raw.min(PENALTY_MAX_BPS as u64) as u32
    }
}

impl RuntimeRiskPolicy {
    /// Resolve the declared risk policy for `chain_name`.
    ///
    /// Fails closed: if the policy declares a USD profit floor but no native
    /// USD price is available (env `{PREFIX}_NATIVE_USD_PRICE` or
    /// `NATIVE_USD_PRICE`), startup aborts rather than silently trading
    /// without the declared floor.
    pub fn resolve(
        chain_name: &str,
        ops_inputs: &OpsInputs,
        native_usd_price: Option<f64>,
    ) -> Result<Option<Self>> {
        let Some(cfg) = ops_inputs
            .risk
            .per_chain
            .iter()
            .find(|cfg| cfg.chain_name.eq_ignore_ascii_case(chain_name))
        else {
            return Ok(None);
        };
        Self::from_chain_config(cfg, native_usd_price).map(Some)
    }

    fn from_chain_config(cfg: &RiskChainConfig, native_usd_price: Option<f64>) -> Result<Self> {
        let mut min_net_profit_wei: Option<U256> = None;

        if let Some(native) = cfg.min_net_profit_native {
            if native > 0.0 {
                min_net_profit_wei = Some(f64_to_wei(native)?);
            }
        }
        if let Some(usd) = cfg.min_net_profit_usd {
            if usd > 0.0 {
                let price = native_usd_price.filter(|price| *price > 0.0).ok_or_else(|| {
                    anyhow!(
                        "risk policy for {} declares min_net_profit_usd={} but no native USD price \
is configured; set {{PREFIX}}_NATIVE_USD_PRICE / NATIVE_USD_PRICE or use min_net_profit_native",
                        cfg.chain_name,
                        usd
                    )
                })?;
                let usd_floor_wei = f64_to_wei(usd / price)?;
                min_net_profit_wei = Some(match min_net_profit_wei {
                    Some(existing) => existing.max(usd_floor_wei),
                    None => usd_floor_wei,
                });
            }
        }

        let max_fee_per_gas_cap = cfg
            .max_fee_per_gas_cap
            .as_ref()
            .map(|raw| {
                U256::from_dec_str(raw.trim()).map_err(|err| {
                    anyhow!(
                        "risk policy for {}: invalid max_fee_per_gas_cap `{raw}`: {err}",
                        cfg.chain_name
                    )
                })
            })
            .transpose()?
            .filter(|cap| !cap.is_zero());

        let revert_penalty = cfg.revert_penalty_model.as_ref().and_then(|model| {
            match model.kind.as_ref()? {
                RevertPenaltyKind::Flat => Some(RevertPenalty::Flat {
                    bps: model.flat_bps.unwrap_or(0),
                }),
                RevertPenaltyKind::Linear => Some(RevertPenalty::Linear {
                    base_bps: model.linear_base_bps.unwrap_or(0),
                    per_gas_bps: model.linear_per_gas_bps.unwrap_or(0),
                }),
                RevertPenaltyKind::Exponential => Some(RevertPenalty::Exponential {
                    base_bps: model.exponential_base_bps.unwrap_or(0),
                    factor: model.exponential_factor.unwrap_or(1),
                }),
            }
        });

        Ok(Self {
            min_net_profit_wei,
            max_gas_units_per_tx: cfg.max_gas_units_per_tx.filter(|cap| *cap > 0),
            max_fee_per_gas_cap,
            max_slippage_bps: cfg.max_slippage_bps.filter(|cap| *cap > 0),
            max_price_impact_bps: cfg.max_price_impact_bps.filter(|cap| *cap > 0),
            must_simulate_before_send: cfg.must_simulate_before_send.unwrap_or(true),
            revert_penalty,
        })
    }

    /// Revert-risk premium in bps for a plan of `gas_units`.
    pub fn revert_penalty_bps(&self, gas_units: u64) -> u32 {
        self.revert_penalty
            .as_ref()
            .map(|model| model.premium_bps(gas_units))
            .unwrap_or(0)
    }
}

fn f64_to_wei(value: f64) -> Result<U256> {
    if !value.is_finite() || value < 0.0 {
        return Err(anyhow!("cannot convert {value} to wei"));
    }
    // Split to keep f64 precision loss bounded for realistic floor values
    // (fractions of a native token up to a few thousand).
    let scaled = value * 1e18;
    if scaled >= u128::MAX as f64 {
        return Err(anyhow!("value {value} too large to convert to wei"));
    }
    Ok(U256::from(scaled as u128))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops_inputs::{RevertPenaltyModel, RiskConfig};

    fn ops_with_risk(cfg: RiskChainConfig) -> OpsInputs {
        OpsInputs {
            chains: Vec::new(),
            universe: Default::default(),
            risk: RiskConfig {
                per_chain: vec![cfg],
            },
            features: Default::default(),
        }
    }

    fn base_cfg() -> RiskChainConfig {
        RiskChainConfig {
            chain_name: "base".to_string(),
            min_net_profit_usd: Some(15.0),
            min_net_profit_native: None,
            max_gas_units_per_tx: Some(500_000),
            max_fee_per_gas_cap: Some("200000000000".to_string()),
            max_slippage_bps: Some(50),
            max_price_impact_bps: Some(70),
            must_simulate_before_send: Some(true),
            revert_penalty_model: Some(RevertPenaltyModel {
                kind: Some(RevertPenaltyKind::Linear),
                flat_bps: None,
                linear_base_bps: Some(10),
                linear_per_gas_bps: Some(1),
                exponential_base_bps: None,
                exponential_factor: None,
            }),
        }
    }

    #[test]
    fn resolves_usd_floor_into_native_wei() {
        let ops = ops_with_risk(base_cfg());
        let policy = RuntimeRiskPolicy::resolve("base", &ops, Some(3_000.0))
            .expect("resolve")
            .expect("policy present");
        // 15 USD at 3000 USD/native = 0.005 native = 5e15 wei
        assert_eq!(
            policy.min_net_profit_wei,
            Some(U256::from(5_000_000_000_000_000u64))
        );
        assert_eq!(policy.max_gas_units_per_tx, Some(500_000));
        assert_eq!(
            policy.max_fee_per_gas_cap,
            Some(U256::from(200_000_000_000u64))
        );
        assert_eq!(policy.max_slippage_bps, Some(50));
        assert_eq!(policy.max_price_impact_bps, Some(70));
        assert!(policy.must_simulate_before_send);
    }

    #[test]
    fn usd_floor_without_price_fails_closed() {
        let ops = ops_with_risk(base_cfg());
        let err = RuntimeRiskPolicy::resolve("base", &ops, None).expect_err("must fail closed");
        assert!(err.to_string().contains("min_net_profit_usd"));
    }

    #[test]
    fn native_floor_used_when_usd_absent() {
        let mut cfg = base_cfg();
        cfg.min_net_profit_usd = None;
        cfg.min_net_profit_native = Some(0.01);
        let ops = ops_with_risk(cfg);
        let policy = RuntimeRiskPolicy::resolve("base", &ops, None)
            .expect("resolve")
            .expect("policy present");
        assert_eq!(
            policy.min_net_profit_wei,
            Some(U256::from(10_000_000_000_000_000u64))
        );
    }

    #[test]
    fn takes_max_of_usd_and_native_floors() {
        let mut cfg = base_cfg();
        cfg.min_net_profit_native = Some(0.5);
        let ops = ops_with_risk(cfg);
        let policy = RuntimeRiskPolicy::resolve("base", &ops, Some(3_000.0))
            .expect("resolve")
            .expect("policy present");
        // native floor 0.5 > usd floor 0.005
        assert_eq!(
            policy.min_net_profit_wei,
            Some(U256::from(500_000_000_000_000_000u64))
        );
    }

    #[test]
    fn unknown_chain_resolves_to_none() {
        let ops = ops_with_risk(base_cfg());
        assert!(RuntimeRiskPolicy::resolve("ink", &ops, Some(3_000.0))
            .expect("resolve")
            .is_none());
    }

    #[test]
    fn linear_penalty_scales_with_gas() {
        let penalty = RevertPenalty::Linear {
            base_bps: 10,
            per_gas_bps: 1,
        };
        assert_eq!(penalty.premium_bps(0), 10);
        assert_eq!(penalty.premium_bps(500_000), 15);
        assert_eq!(penalty.premium_bps(1_000_000), 20);
    }

    #[test]
    fn flat_penalty_is_constant() {
        let penalty = RevertPenalty::Flat { bps: 25 };
        assert_eq!(penalty.premium_bps(0), 25);
        assert_eq!(penalty.premium_bps(10_000_000), 25);
    }

    #[test]
    fn exponential_penalty_caps_at_full_gas_cost() {
        let penalty = RevertPenalty::Exponential {
            base_bps: 10,
            factor: 2,
        };
        assert_eq!(penalty.premium_bps(0), 10);
        // 250k gas -> one doubling
        assert_eq!(penalty.premium_bps(250_000), 20);
        // enormous plans cap at 10_000 bps (100% of gas cost)
        assert!(penalty.premium_bps(100_000_000) <= 10_000);
    }
}
