//! The census's targets (R36): the routes where trigger-block arbitrage our
//! executor can run happens, WETH-in pool first, and what their winners paid
//! per gas, as `scripts/data/arb_census/targets.py` writes them each day.

use alloy_primitives::Address;
use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct TargetRoute {
    /// WETH-in pool first: the first leg of the cycle the route arms.
    pub pools: [Address; 2],
    pub usd: f64,
    pub arbs: u32,
    /// The route's own winners' effective priority fees per gas, when it has
    /// twenty or more.
    #[serde(default)]
    pub bids_wei_per_gas: Option<Vec<u128>>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Targets {
    pub generated_at: String,
    pub days: Vec<String>,
    /// The census's own verdict: enough days, a route, finite values.
    pub ok: bool,
    pub routes: Vec<TargetRoute>,
    /// Every targeted route's winners, ascending.
    pub bids_wei_per_gas: Vec<u128>,
}

impl Targets {
    /// The file at `path`, refused when older than `max_age_s` at `now_unix`,
    /// or when a route is malformed.
    pub fn load(path: &Path, now_unix: u64, max_age_s: u64) -> Result<Self, String> {
        let shown = path.display();
        let text = std::fs::read_to_string(path).map_err(|e| format!("targets file {shown}: {e}"))?;
        let t: Self = serde_json::from_str(&text).map_err(|e| format!("targets file {shown}: {e}"))?;
        let at = unix_of(&t.generated_at)
            .ok_or_else(|| format!("targets file {shown}: generated_at {:?} is not YYYY-MM-DDTHH:MM:SSZ", t.generated_at))?;
        let age = now_unix.saturating_sub(at);
        if age > max_age_s {
            return Err(format!(
                "targets file {shown} is stale: generated {}, {age} s ago, at most {max_age_s} s allowed",
                t.generated_at
            ));
        }
        for r in &t.routes {
            if r.pools[0] == r.pools[1] {
                return Err(format!("targets file {shown}: a route whose two pools are both {:#x}", r.pools[0]));
            }
            if r.usd < 0.0 {
                return Err(format!("targets file {shown}: the route {:#x}, {:#x} is worth {}", r.pools[0], r.pools[1], r.usd));
            }
        }
        Ok(t)
    }

    /// Every pool a targeted route uses.
    pub fn pools(&self) -> BTreeSet<Address> {
        self.routes.iter().flat_map(|r| r.pools).collect()
    }

    /// Whether the route WETH-in `weth_in`, then `other`, is targeted.
    pub fn has(&self, weth_in: Address, other: Address) -> bool {
        self.routes.iter().any(|r| r.pools == [weth_in, other])
    }

    /// The winners' bids an arm on that route bids against: its own when it
    /// has them, every targeted route's otherwise.
    pub fn bids_for(&self, weth_in: Address, other: Address) -> &[u128] {
        self.routes
            .iter()
            .find(|r| r.pools == [weth_in, other])
            .and_then(|r| r.bids_wei_per_gas.as_deref())
            .unwrap_or(&self.bids_wei_per_gas)
    }

    /// Fit to arm live: the census says so, and names a route.
    pub fn live_ready(&self) -> bool {
        self.ok && !self.routes.is_empty()
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ` as Unix seconds.
fn unix_of(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() != 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' || b[19] != b'Z' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| -> Option<u64> {
        let part = s.get(r)?;
        part.bytes().all(|c| c.is_ascii_digit()).then(|| part.parse().ok()).flatten()
    };
    let (y, m, d, hh, mm, ss) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 59 {
        return None;
    }
    let days = u64::try_from(days_from_civil(i64::try_from(y).ok()?, m as i64, d as i64)).ok()?;
    Some(days * 86_400 + hh * 3_600 + mm * 60 + ss)
}

/// Days from 1970-01-01 to a proleptic Gregorian date (H. Hinnant's
/// `days_from_civil`; the inverse of `shadow::ledger`'s `civil_from_days`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}
