//! An armed transaction's bid (R36): a priority fee per gas, against what the
//! winners on its route paid per gas.
//!
//! Base orders eligible transactions by priority fee per gas, so the bid that
//! competes is per gas, not a share of value (R35's winners bid a median 22%
//! of value, but on 190-740k gas: a share says nothing about the price per
//! unit we must beat on ours). A validity transaction that loses is never
//! included — its predicates turn false once the winner closes the gap — so
//! its tip is paid only on a win, unlike the reactive lane's (R31).

use ethers_core::types::U256;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bid {
    /// Wei per gas over the base fee.
    pub per_gas: u128,
    /// The share of measured winners it outbids.
    pub p_win: f64,
}

/// The bid `b` maximising `F(b) × (net − b × gas)`, `F(b)` the share of
/// `sorted_bids` strictly below `b`: over bidding nothing and every measured
/// bid + 1 wei up to `cap_wei_per_gas`. Integer: `below × (net − b × gas)`
/// over the same count. A tie keeps the lower bid.
pub fn best_bid(sorted_bids: &[u128], net_wei: U256, gas: u64, cap_wei_per_gas: u128) -> Bid {
    let n = sorted_bids.len();
    if n == 0 || gas == 0 {
        return Bid { per_gas: 0, p_win: 0.0 };
    }
    let net = if net_wei > U256::from(u128::MAX) { u128::MAX } else { net_wei.as_u128() };
    let (mut best_b, mut best_below, mut best) = (0u128, 0usize, 0u128);
    let mut i = 0;
    while i < n {
        let v = sorted_bids[i];
        while i < n && sorted_bids[i] == v {
            i += 1;
        }
        let b = v.saturating_add(1);
        if b > cap_wei_per_gas {
            break;
        }
        let Some(kept) = b.checked_mul(u128::from(gas)).and_then(|tip| net.checked_sub(tip)) else { continue };
        let value = (i as u128).saturating_mul(kept);
        if value > best {
            (best_b, best_below, best) = (b, i, value);
        }
    }
    Bid { per_gas: best_b, p_win: best_below as f64 / n as f64 }
}

/// What an arm is worth to hold: the chance its trigger moves past the rung
/// within the window, times the chance its bid wins, times what the win keeps
/// after the tip. Negative when the tip outweighs the surplus.
pub fn rank_value(p_trigger: f64, bid: &Bid, surplus_wei: U256, gas: u64) -> f64 {
    let surplus = surplus_wei.as_u128() as f64;
    let tip = bid.per_gas as f64 * gas as f64;
    p_trigger * bid.p_win * (surplus - tip)
}

/// R38: a share of an arm's surplus, in parts per [`SHARE_SCALE`].
pub const SHARE_SCALE: u64 = 1_000_000_000;
/// R38: the fewest effective rows (independent market moves) a share sample
/// needs before an arm bids on it, or carries its bid to be judged.
pub const MIN_EFFECTIVE: u64 = 300;

/// R38: the census's contest sample: at each judged fire, the share of the
/// arm's surplus a bid had to exceed to win, weighted so one market move
/// counts once. Rows ascending by share.
#[derive(Clone, Debug, PartialEq)]
pub struct ShareSample {
    rows: Vec<(u128, u64)>,
    total: u128,
    pub effective: u64,
    /// The census's gates passed: fit to send on, not only to judge.
    pub fit_ok: bool,
    pub model_version: String,
}

impl ShareSample {
    pub fn new(rows: Vec<(u128, u64)>, effective: u64, fit_ok: bool, model_version: String) -> Result<Self, String> {
        if rows.windows(2).any(|w| w[0].0 > w[1].0) {
            return Err("contest rows are not ascending".into());
        }
        if rows.iter().any(|(_, w)| *w == 0) {
            return Err("a contest row has no weight".into());
        }
        let total = rows.iter().map(|(_, w)| u128::from(*w)).sum();
        Ok(Self { rows, total, effective, fit_ok, model_version })
    }

    pub fn total(&self) -> u128 {
        self.total
    }
}

/// R38: the bid `b` maximising `F(b) × (surplus − b × gas)`, `F(b)` the
/// weight of rows whose share is strictly below `b`'s
/// (`x × surplus < b × gas × SHARE_SCALE`). Each row's candidate is the
/// smallest integer bid past it; `p_win` is `F` at that integer bid. A bid
/// over `cap`, or whose gas leaves no surplus, is not a candidate. A tie keeps
/// the lower bid. Exact integers throughout; an overflow fails closed.
pub fn best_share(sample: &ShareSample, surplus_wei: U256, gas: u64, cap: u128) -> Result<Bid, &'static str> {
    if gas == 0 {
        return Err("zero gas");
    }
    if surplus_wei.is_zero() {
        return Err("zero surplus");
    }
    if sample.rows.is_empty() {
        return Err("empty sample");
    }
    let per = U256::from(gas).checked_mul(U256::from(SHARE_SCALE)).ok_or("overflow")?;
    let (mut best_b, mut best_below, mut best) = (0u128, 0u128, U256::zero());
    let (mut below, mut i) = (0u128, 0usize);
    for &(x, _) in &sample.rows {
        let xs = U256::from(x).checked_mul(surplus_wei).ok_or("overflow")?;
        let q = xs / per + U256::one();
        if q > U256::from(cap) {
            break;
        }
        let b = q.as_u128();
        let tip = U256::from(b).checked_mul(U256::from(gas)).ok_or("overflow")?;
        if tip >= surplus_wei {
            break;
        }
        let bar = U256::from(b).checked_mul(per).ok_or("overflow")?;
        while i < sample.rows.len() {
            let (xi, w) = sample.rows[i];
            if U256::from(xi).checked_mul(surplus_wei).ok_or("overflow")? >= bar {
                break;
            }
            below += u128::from(w);
            i += 1;
        }
        let value = U256::from(below).checked_mul(surplus_wei - tip).ok_or("overflow")?;
        if value > best {
            (best_b, best_below, best) = (b, below, value);
        }
    }
    Ok(Bid { per_gas: best_b, p_win: best_below as f64 / sample.total as f64 })
}

/// R38: which rule's bid an arm sends (`armed.bid_model`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BidModel {
    /// R36's: against the census's measured winners on the route.
    #[default]
    BestBid,
    /// R38's: against our own fires' contests, as shares of the surplus.
    BestShare,
}

/// R38: the rule an arm's bid came from, or why `best_share` fell back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BidChoice {
    BestBid,
    BestShare,
    /// `best_share` was asked for and could not be used: `best_bid`, the
    /// known-miscalibrated rule, was sent.
    Fallback(&'static str),
}

impl BidChoice {
    /// As an arm records it.
    pub fn label(&self) -> String {
        match self {
            Self::BestBid => "best_bid".into(),
            Self::BestShare => "best_share".into(),
            Self::Fallback(why) => format!("best_bid (fallback: {why})"),
        }
    }
}

/// R38: an arm's bid, and the other rule's, judged beside it on every fire.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ArmBid {
    pub sent: Bid,
    pub alt: Option<Bid>,
    pub choice: BidChoice,
}

/// R38: the bid `model` asks for. `best_bid` against the route's measured
/// `winners` (none measured: nothing, ranked as R26's); `best_share` against
/// `sample` when it has [`MIN_EFFECTIVE`] rows and passed the census's gates,
/// else `best_bid` with the reason. The other rule's bid rides along whenever
/// it can be computed.
pub fn arm_bid(winners: &[u128], sample: Option<&ShareSample>, model: BidModel, surplus_wei: U256, gas: u64, cap: u128) -> ArmBid {
    let by_winners = if winners.is_empty() { Bid { per_gas: 0, p_win: 1.0 } } else { best_bid(winners, surplus_wei, gas, cap) };
    let by_share = match sample {
        None => Err("no contest sample"),
        Some(s) if s.effective < MIN_EFFECTIVE => Err("too few effective rows"),
        Some(s) => best_share(s, surplus_wei, gas, cap),
    };
    match (model, by_share) {
        (BidModel::BestShare, Ok(share)) if sample.is_some_and(|s| s.fit_ok) => {
            ArmBid { sent: share, alt: Some(by_winners), choice: BidChoice::BestShare }
        }
        (BidModel::BestShare, Ok(share)) => {
            ArmBid { sent: by_winners, alt: Some(share), choice: BidChoice::Fallback("contest sample unfit") }
        }
        (BidModel::BestShare, Err(why)) => ArmBid { sent: by_winners, alt: None, choice: BidChoice::Fallback(why) },
        (BidModel::BestBid, share) => ArmBid { sent: by_winners, alt: share.ok(), choice: BidChoice::BestBid },
    }
}
