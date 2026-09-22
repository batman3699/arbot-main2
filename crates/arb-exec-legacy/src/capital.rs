use std::sync::RwLock;

use anyhow::{ensure, Result};
use ethers::types::{Address, U256};

use crate::math::mul_div;

#[derive(Clone, Copy, Debug)]
pub struct CapitalSnapshot {
    pub base_amount: U256,
    pub min_flash_loan: U256,
    pub max_flash_loan: U256,
    pub siphon_buffer: U256,
}

#[derive(Debug)]
pub struct CapitalUpdate {
    pub snapshot: CapitalSnapshot,
    pub siphon_ready: Option<(Address, U256)>,
    pub grew: bool,
}

#[derive(Debug)]
pub struct CapitalManager {
    base_amount: RwLock<U256>,
    min_flash_loan: RwLock<U256>,
    max_flash_loan: RwLock<U256>,
    reinvested_profit: RwLock<U256>,
    siphon_buffer: RwLock<U256>,
    initial_base: U256,
    growth_unit: U256,
    max_base_cap: U256,
    reinvest_bps: u32,
    siphon_bps: u32,
    siphon_threshold: U256,
    siphon_target: Option<Address>,
}

impl CapitalManager {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        initial_base: U256,
        min_flash_loan: U256,
        max_flash_loan: U256,
        reinvest_bps: u32,
        siphon_bps: u32,
        growth_unit: U256,
        max_base_cap: U256,
        siphon_threshold: U256,
        siphon_target: Option<Address>,
    ) -> Result<Self> {
        ensure!(reinvest_bps <= 10_000, "reinvest_bps must be <= 10000");
        ensure!(siphon_bps <= 10_000, "siphon_bps must be <= 10000");
        ensure!(
            reinvest_bps + siphon_bps <= 10_000,
            "profit allocation exceeds 100%"
        );

        let min_flash_loan = min_flash_loan.max(U256::one());
        let max_flash_loan = max_flash_loan.max(min_flash_loan);

        Ok(Self {
            base_amount: RwLock::new(initial_base),
            min_flash_loan: RwLock::new(min_flash_loan),
            max_flash_loan: RwLock::new(max_flash_loan),
            reinvested_profit: RwLock::new(U256::zero()),
            siphon_buffer: RwLock::new(U256::zero()),
            initial_base,
            growth_unit: growth_unit.max(U256::one()),
            max_base_cap: max_base_cap.max(max_flash_loan),
            reinvest_bps,
            siphon_bps,
            siphon_threshold,
            siphon_target,
        })
    }

    pub fn snapshot(&self) -> CapitalSnapshot {
        CapitalSnapshot {
            base_amount: *self.base_amount.read().unwrap_or_else(|e| e.into_inner()),
            min_flash_loan: *self
                .min_flash_loan
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            max_flash_loan: *self
                .max_flash_loan
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            siphon_buffer: *self.siphon_buffer.read().unwrap_or_else(|e| e.into_inner()),
        }
    }

    pub fn apply_profit(&self, net_profit: U256) -> Option<CapitalUpdate> {
        if net_profit.is_zero() {
            return None;
        }

        let reinvest = mul_div(
            net_profit,
            U256::from(self.reinvest_bps as u64),
            U256::from(10_000u64),
        );
        let siphon = mul_div(
            net_profit,
            U256::from(self.siphon_bps as u64),
            U256::from(10_000u64),
        );

        let mut reinvested = self
            .reinvested_profit
            .write()
            .unwrap_or_else(|e| e.into_inner());
        *reinvested = reinvested.saturating_add(reinvest);

        let steps = if self.growth_unit.is_zero() {
            U256::zero()
        } else {
            *reinvested / self.growth_unit
        };
        let mut grew = false;

        if steps > U256::zero() {
            let proposed_base = self
                .initial_base
                .saturating_mul(steps.saturating_add(U256::one()))
                .min(self.max_base_cap);
            {
                let mut base_guard = self.base_amount.write().unwrap_or_else(|e| e.into_inner());
                if proposed_base > *base_guard {
                    *base_guard = proposed_base;
                    grew = true;
                }
            }

            let mut min_guard = self
                .min_flash_loan
                .write()
                .unwrap_or_else(|e| e.into_inner());
            let mut max_guard = self
                .max_flash_loan
                .write()
                .unwrap_or_else(|e| e.into_inner());
            let min_flash = proposed_base
                .checked_div(U256::from(5u64))
                .unwrap_or(U256::zero())
                .max(U256::one());
            let max_flash = proposed_base
                .checked_mul(U256::from(5u64))
                .unwrap_or(U256::MAX)
                .min(self.max_base_cap)
                .max(min_flash);
            if min_flash > *min_guard || max_flash > *max_guard {
                *min_guard = min_flash;
                *max_guard = max_flash;
                grew = true;
            }
        }

        let siphon_ready = {
            let mut buffer = self
                .siphon_buffer
                .write()
                .unwrap_or_else(|e| e.into_inner());
            *buffer = buffer.saturating_add(siphon);
            if let Some(target) = self.siphon_target {
                if *buffer >= self.siphon_threshold && !buffer.is_zero() {
                    let amount = *buffer;
                    *buffer = U256::zero();
                    Some((target, amount))
                } else {
                    None
                }
            } else {
                None
            }
        };

        Some(CapitalUpdate {
            snapshot: self.snapshot(),
            siphon_ready,
            grew,
        })
    }
}
