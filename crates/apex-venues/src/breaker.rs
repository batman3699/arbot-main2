//! Per-venue circuit breakers (PLAN.md §10.5, blueprint §8.2).
//!
//! "Every venue gets an **independent** circuit breaker." Independence is the
//! whole requirement: one venue's quoter going down must not stop the other
//! five from trading, and a shared breaker — or a shared lock in front of
//! per-venue state — makes that impossible to honour.
//!
//! # Losing a race is not a venue fault
//!
//! The judgement this module is really making is *what counts as a failure*,
//! and getting it wrong disables the venues that work. `MinOutNotMet` means
//! somebody else moved the pool between quote and execution. That is the
//! normal, expected outcome of competing for an opportunity, it happens most
//! often on the venues with the most flow, and counting it against the venue
//! would trip exactly the venues worth trading on. See [`counts_against_venue`],
//! which is exhaustive over `RevertClass` with no catch-all so that a new class
//! forces the decision to be made rather than defaulted.
//!
//! # Time is injected
//!
//! Every method takes `now`. A breaker that reads the clock internally can only
//! be tested by sleeping, which makes its tests slow and flaky, and this
//! repository has already been bitten twice by tests that synchronise on
//! elapsed wall time.

use apex_types::ids::VenueId;
use apex_types::sim::RevertClass;
use apex_types::time::{DurationNanos, UnixNanos};
use dashmap::DashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};

/// Whether a revert is evidence that the VENUE is unhealthy, as opposed to
/// evidence that the market moved.
///
/// Exhaustive, no catch-all: adding a `RevertClass` variant makes this fail to
/// compile until somebody decides which side it falls on.
pub const fn counts_against_venue(class: RevertClass) -> bool {
    match class {
        // We lost the race. The venue did its job; someone was faster.
        // Counting these trips the busiest venues first, which is precisely
        // backwards.
        RevertClass::MinOutNotMet => false,
        // The pool could not fill the size. That is a pricing or sizing error
        // on our side -- the model said it could -- and it is the signal that
        // this venue's state or maths has drifted from the chain.
        RevertClass::InsufficientLiquidity => true,
        // Our deadline was too tight, or we were slow. Not the venue.
        RevertClass::Expired => false,
        // Structural: wrong caller, wrong role, revoked approval.
        RevertClass::Unauthorized => true,
        RevertClass::FlashRepaymentShortfall => true,
        RevertClass::ProfitInvariantViolated => true,
        RevertClass::TokenTransferFailed => true,
        RevertClass::HookRejected => true,
        // A gas ceiling is ours to set. Tripping the venue hides the real fix.
        RevertClass::OutOfGas => false,
        // We do not know what happened. Counting it is the safe direction:
        // an unexplained failure on one venue is exactly when to back off.
        RevertClass::Unknown => true,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BreakerPolicy {
    /// Consecutive counted failures that open the breaker.
    pub trip_after: u32,
    /// How long it stays open before a single probe is admitted.
    pub cooldown: DurationNanos,
}

impl Default for BreakerPolicy {
    fn default() -> Self {
        Self {
            trip_after: 5,
            cooldown: DurationNanos(30_000_000_000), // 30s
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BreakerState {
    Closed,
    Open,
    HalfOpen,
}

const CLOSED: u8 = 0;
const OPEN: u8 = 1;
const HALF_OPEN: u8 = 2;

/// Proof that the breaker admitted this call, and the only way to report how
/// it went.
///
/// Not `Clone` and not `Copy`: one permit is one outcome. Recording two
/// outcomes for one admitted call would let a single success clear a trip that
/// several failures earned, and a permit cannot be constructed except by being
/// admitted.
///
/// **`Drop` resolves it.** A probe permit dropped without a verdict — the
/// caller panicked, or returned early on `?` between `try_admit` and the report
/// — would otherwise leave `probe_in_flight` claimed forever and wedge the
/// venue half-open, refusing every caller for the life of the process. An
/// unresolved probe is treated as a failed one: the conservative reading of
/// "we admitted a call and never heard back".
#[derive(Debug)]
#[must_use = "a permit must be resolved: success(), failure() or revert()"]
pub struct Permit<'a> {
    breaker: &'a VenueBreaker,
    is_probe: bool,
    resolved: bool,
}

impl Permit<'_> {
    pub const fn is_probe(&self) -> bool {
        self.is_probe
    }

    /// The call succeeded.
    pub fn success(mut self) {
        self.resolved = true;
        self.breaker.consecutive_failures.store(0, Ordering::Release);
        if self.is_probe {
            self.breaker.state.store(CLOSED, Ordering::Release);
            self.breaker.probe_in_flight.store(false, Ordering::Release);
        }
    }

    /// The call failed in a way that counts against the venue.
    pub fn failure(mut self, now: UnixNanos) {
        self.resolved = true;
        self.breaker.on_failure(self.is_probe, now);
    }

    /// The call reverted. Whether it counts is [`counts_against_venue`]'s
    /// decision, kept here so the "losing a race is not a venue fault" rule
    /// lives in one place rather than at every call site.
    pub fn revert(mut self, class: RevertClass, now: UnixNanos) {
        self.resolved = true;
        if counts_against_venue(class) {
            self.breaker.on_failure(self.is_probe, now);
        } else if self.is_probe {
            // Not a success either: a lost race must not clear a failure streak
            // the venue earned, and a probe that lost a race has told us
            // nothing about whether the venue recovered.
            self.breaker.reopen(now);
        }
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        if !self.resolved && self.is_probe {
            let opened_at = UnixNanos(self.breaker.opened_at.load(Ordering::Acquire));
            self.breaker.reopen(opened_at);
        }
    }
}

#[derive(Debug)]
pub struct VenueBreaker {
    venue: VenueId,
    policy: BreakerPolicy,
    state: AtomicU8,
    consecutive_failures: AtomicU32,
    opened_at: AtomicU64,
    /// Claimed by the one caller allowed through while half-open.
    probe_in_flight: AtomicBool,
    trips: AtomicU64,
}

impl VenueBreaker {
    pub fn new(venue: VenueId, policy: BreakerPolicy) -> Self {
        Self {
            venue,
            policy,
            state: AtomicU8::new(CLOSED),
            consecutive_failures: AtomicU32::new(0),
            opened_at: AtomicU64::new(0),
            probe_in_flight: AtomicBool::new(false),
            trips: AtomicU64::new(0),
        }
    }

    pub const fn venue(&self) -> VenueId {
        self.venue
    }

    /// Lifetime count of transitions into `Open`. Observability only.
    pub fn trips(&self) -> u64 {
        self.trips.load(Ordering::Relaxed)
    }

    pub fn state(&self, now: UnixNanos) -> BreakerState {
        match self.state.load(Ordering::Acquire) {
            CLOSED => BreakerState::Closed,
            HALF_OPEN => BreakerState::HalfOpen,
            _ if self.cooldown_elapsed(now) => BreakerState::HalfOpen,
            _ => BreakerState::Open,
        }
    }

    fn cooldown_elapsed(&self, now: UnixNanos) -> bool {
        now.0.saturating_sub(self.opened_at.load(Ordering::Acquire)) >= self.policy.cooldown.0
    }

    /// Ask to make a call. `None` means the breaker is open and this caller is
    /// not the probe.
    pub fn try_admit(&self, now: UnixNanos) -> Option<Permit<'_>> {
        match self.state.load(Ordering::Acquire) {
            CLOSED => Some(Permit {
                breaker: self,
                is_probe: false,
                resolved: false,
            }),
            _ => {
                if !self.cooldown_elapsed(now) {
                    return None;
                }
                // Exactly ONE caller crosses into half-open. Without this claim
                // every thread waiting on a tripped venue would fire the
                // instant the cooldown expired, which is the load spike the
                // breaker exists to prevent.
                if self
                    .probe_in_flight
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    return None;
                }
                self.state.store(HALF_OPEN, Ordering::Release);
                Some(Permit {
                    breaker: self,
                    is_probe: true,
                    resolved: false,
                })
            }
        }
    }

    fn on_failure(&self, is_probe: bool, now: UnixNanos) {
        if is_probe {
            // The probe failed: straight back to open, and the cooldown
            // restarts from now rather than from the original trip.
            self.reopen(now);
            return;
        }
        let failures = self.consecutive_failures.fetch_add(1, Ordering::AcqRel) + 1;
        if failures >= self.policy.trip_after {
            self.open(now);
        }
    }

    /// Back to open from half-open, releasing the probe slot.
    fn reopen(&self, now: UnixNanos) {
        self.open(now);
        self.probe_in_flight.store(false, Ordering::Release);
    }

    fn open(&self, now: UnixNanos) {
        if self.state.swap(OPEN, Ordering::AcqRel) != OPEN {
            self.trips.fetch_add(1, Ordering::Relaxed);
        }
        self.opened_at.store(now.0, Ordering::Release);
        self.consecutive_failures.store(0, Ordering::Release);
    }
}

/// One breaker per venue, created on first use.
#[derive(Debug, Default)]
pub struct BreakerRegistry {
    breakers: DashMap<VenueId, std::sync::Arc<VenueBreaker>>,
    policy: BreakerPolicy,
}

impl BreakerRegistry {
    pub fn new(policy: BreakerPolicy) -> Self {
        Self {
            breakers: DashMap::new(),
            policy,
        }
    }

    pub fn for_venue(&self, venue: VenueId) -> std::sync::Arc<VenueBreaker> {
        self.breakers
            .entry(venue)
            .or_insert_with(|| std::sync::Arc::new(VenueBreaker::new(venue, self.policy)))
            .clone()
    }

    /// Venues currently refusing traffic, for the operator surface.
    pub fn open_venues(&self, now: UnixNanos) -> Vec<VenueId> {
        self.breakers
            .iter()
            .filter(|e| e.value().state(now) == BreakerState::Open)
            .map(|e| *e.key())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1: VenueId = VenueId(1);
    const V2: VenueId = VenueId(2);
    fn t(secs: u64) -> UnixNanos {
        UnixNanos(secs * 1_000_000_000)
    }

    fn fail_n(b: &VenueBreaker, n: u32, at: UnixNanos) {
        for _ in 0..n {
            b.try_admit(at).expect("closed breaker admits").failure(at);
        }
    }

    #[test]
    fn a_closed_breaker_admits_everything() {
        let b = VenueBreaker::new(V1, BreakerPolicy::default());
        for _ in 0..100 {
            assert!(b.try_admit(t(0)).is_some());
        }
        assert_eq!(b.state(t(0)), BreakerState::Closed);
        assert_eq!(b.trips(), 0);
    }

    #[test]
    fn consecutive_failures_trip_it_and_it_then_refuses() {
        let b = VenueBreaker::new(V1, BreakerPolicy::default());
        fail_n(&b, 5, t(0));
        assert_eq!(b.state(t(0)), BreakerState::Open);
        assert_eq!(b.trips(), 1);
        assert!(b.try_admit(t(0)).is_none(), "an open breaker refuses traffic");
    }

    /// A success between failures resets the streak — the breaker is measuring
    /// a run of failures, not a lifetime total.
    #[test]
    fn a_success_clears_the_streak() {
        let b = VenueBreaker::new(V1, BreakerPolicy::default());
        fail_n(&b, 4, t(0));
        b.try_admit(t(0)).expect("still closed").success();
        fail_n(&b, 4, t(0));
        assert_eq!(b.state(t(0)), BreakerState::Closed, "8 failures, never 5 in a row");
    }

    /// The point of the whole module: one venue tripping leaves the rest alone.
    #[test]
    fn one_venue_tripping_does_not_touch_another() {
        let reg = BreakerRegistry::new(BreakerPolicy::default());
        let a = reg.for_venue(V1);
        let b = reg.for_venue(V2);
        fail_n(&a, 5, t(0));
        assert_eq!(a.state(t(0)), BreakerState::Open);
        assert_eq!(b.state(t(0)), BreakerState::Closed);
        assert!(b.try_admit(t(0)).is_some(), "V2 must be unaffected by V1");
        assert_eq!(reg.open_venues(t(0)), vec![V1]);
    }

    /// After the cooldown exactly ONE caller gets through, not everybody.
    #[test]
    fn half_open_admits_exactly_one_probe() {
        let b = VenueBreaker::new(V1, BreakerPolicy::default());
        fail_n(&b, 5, t(0));
        assert!(b.try_admit(t(29)).is_none(), "cooldown has not elapsed");

        let probe = b.try_admit(t(31)).expect("one probe is admitted");
        assert!(probe.is_probe());
        for _ in 0..50 {
            assert!(
                b.try_admit(t(31)).is_none(),
                "a second caller must not slip through while a probe is in flight"
            );
        }
        probe.success();
        assert_eq!(b.state(t(31)), BreakerState::Closed);
        assert!(b.try_admit(t(31)).is_some());
    }

    #[test]
    fn a_failed_probe_reopens_and_restarts_the_cooldown() {
        let b = VenueBreaker::new(V1, BreakerPolicy::default());
        fail_n(&b, 5, t(0));
        let probe = b.try_admit(t(31)).expect("probe");
        probe.failure(t(31));
        assert_eq!(b.state(t(31)), BreakerState::Open);
        assert_eq!(b.trips(), 2);
        // Cooldown runs from the failed probe, not from the original trip.
        assert!(b.try_admit(t(50)).is_none());
        assert!(b.try_admit(t(62)).is_some());
    }

    /// Losing a race must never trip a venue.
    ///
    /// `MinOutNotMet` is what happens when somebody else moved the pool between
    /// quote and execution. It happens most on the venues with the most flow,
    /// so counting it would trip the venues most worth trading on.
    #[test]
    fn losing_a_race_does_not_trip_the_venue() {
        let b = VenueBreaker::new(V1, BreakerPolicy::default());
        for _ in 0..500 {
            b.try_admit(t(0)).expect("closed").revert(RevertClass::MinOutNotMet, t(0));
        }
        assert_eq!(b.state(t(0)), BreakerState::Closed);
        assert_eq!(b.trips(), 0);
    }

    /// ...but it does not repair one either.
    #[test]
    fn a_lost_race_does_not_clear_a_real_failure_streak() {
        let b = VenueBreaker::new(V1, BreakerPolicy::default());
        fail_n(&b, 4, t(0));
        b.try_admit(t(0)).expect("closed").revert(RevertClass::MinOutNotMet, t(0));
        fail_n(&b, 1, t(0));
        assert_eq!(
            b.state(t(0)),
            BreakerState::Open,
            "the fifth real failure must still trip it"
        );
    }

    #[test]
    fn structural_reverts_do_trip_the_venue() {
        for class in [
            RevertClass::Unauthorized,
            RevertClass::TokenTransferFailed,
            RevertClass::InsufficientLiquidity,
            RevertClass::HookRejected,
            RevertClass::Unknown,
        ] {
            let b = VenueBreaker::new(V1, BreakerPolicy::default());
            for _ in 0..5 {
                b.try_admit(t(0)).expect("closed").revert(class, t(0));
            }
            assert_eq!(b.state(t(0)), BreakerState::Open, "{class:?} must count");
        }
    }

    #[test]
    fn our_own_gas_ceiling_is_not_the_venues_fault() {
        let b = VenueBreaker::new(V1, BreakerPolicy::default());
        for _ in 0..50 {
            b.try_admit(t(0)).expect("closed").revert(RevertClass::OutOfGas, t(0));
        }
        assert_eq!(b.state(t(0)), BreakerState::Closed);
    }

    /// Concurrent callers see one PROBE admitted, not many.
    ///
    /// Counting admissions would not measure this: a successful probe closes
    /// the breaker and every later caller is then admitted legitimately. What
    /// must be exactly one is the number of permits that crossed into
    /// half-open, which is what `is_probe()` reports.
    #[test]
    fn the_probe_claim_holds_under_contention() {
        use std::sync::Arc;
        let b = Arc::new(VenueBreaker::new(V1, BreakerPolicy::default()));
        fail_n(&b, 5, t(0));
        let probes = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let mut handles = Vec::new();
        for _ in 0..16 {
            let b = Arc::clone(&b);
            let probes = Arc::clone(&probes);
            handles.push(std::thread::spawn(move || {
                for _ in 0..200 {
                    if let Some(p) = b.try_admit(t(31)) {
                        if p.is_probe() {
                            probes.fetch_add(1, Ordering::Relaxed);
                        }
                        // Fail it: the breaker reopens with the cooldown
                        // restarted at t(31), so no further probe is due at
                        // t(31) and the count stays honest.
                        p.failure(t(31));
                    }
                }
            }));
        }
        for h in handles {
            h.join().expect("thread");
        }
        assert_eq!(
            probes.load(Ordering::Relaxed),
            1,
            "exactly one permit may cross into half-open"
        );
    }

    /// A probe permit that is dropped without a verdict must not wedge the
    /// venue.
    ///
    /// The failure this guards: the caller takes the probe, then panics or
    /// returns early on `?` before reporting. `probe_in_flight` would stay
    /// claimed for the life of the process and `try_admit` would refuse every
    /// caller forever — a permanent outage caused by an error path, on the one
    /// venue that had just started recovering.
    #[test]
    fn a_dropped_probe_releases_the_slot_instead_of_wedging_the_breaker() {
        let b = VenueBreaker::new(V1, BreakerPolicy::default());
        fail_n(&b, 5, t(0));
        {
            let probe = b.try_admit(t(31)).expect("probe");
            assert!(probe.is_probe());
            // Dropped here with no verdict, as an error path would.
        }
        let again = b.try_admit(t(62)).expect("the breaker must still be reachable");
        again.success();
        assert_eq!(b.state(t(62)), BreakerState::Closed);
    }

    /// Dropping an ordinary (non-probe) permit changes nothing. Most calls end
    /// this way during shutdown and they must not be counted as failures.
    #[test]
    fn a_dropped_ordinary_permit_is_not_a_failure() {
        let b = VenueBreaker::new(V1, BreakerPolicy::default());
        for _ in 0..100 {
            drop(b.try_admit(t(0)).expect("closed"));
        }
        assert_eq!(b.state(t(0)), BreakerState::Closed);
        assert_eq!(b.trips(), 0);
    }

}
