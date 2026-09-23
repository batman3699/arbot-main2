//! §18.2's seventh hard requirement: **no cross-lane nonce reuse**, under every
//! interleaving rather than under the ones a test author happened to think of.
//!
//! Run with:
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test -p apex-capture --test nonce_loom
//! ```
//!
//! Empty without that cfg, on purpose. loom replaces the concurrency primitives
//! in `src/sync.rs` with its own and then explores the schedule space
//! exhaustively; that is minutes of work, so it gates in CI rather than on every
//! save. A `#[test]` that silently did nothing in the default build would be
//! worse than no test, which is why this file says so in its name and its
//! module docs and CI runs it explicitly.
#![cfg(loom)]

use apex_capture::signer::{ExecutorAuth, LaneConfig, LaneRequirements, SignerPool};
use apex_types::ids::{ChainId, SignerLaneId};
use apex_types::time::UnixNanos;
use loom::sync::{Arc, Mutex};

const EXECUTOR: [u8; 20] = [0xEE; 20];
const VERSION: [u8; 32] = [0x01; 32];

fn requirements() -> LaneRequirements {
    LaneRequirements {
        chain: ChainId::BASE,
        executor: EXECUTOR,
        executor_version: VERSION,
        min_gas_reserve_wei: 0,
    }
}

fn pool(lanes: u16) -> SignerPool {
    SignerPool::new(
        ExecutorAuth { chain: ChainId::BASE, executor: EXECUTOR, executor_version: VERSION },
        (0..lanes)
            .map(|i| LaneConfig {
                id: SignerLaneId(i),
                address: [i as u8; 20],
                gas_reserve_wei: 1_000_000,
            })
            .collect(),
    )
}

/// **INV-04**, named as §8 names it: `capture::nonce_never_reused`, a loom
/// model over the lane state machine.
///
/// Two threads, two lanes, one reservation each. The hazard is both threads
/// surveying the pool, both seeing lane 0 free, and both taking it -- after
/// which they hold one nonce stream between them and hand out the same number
/// twice. `assign` re-checks `busy` under the lock for exactly this.
#[test]
fn nonce_never_reused() {
    loom::model(|| {
        let pool = Arc::new(pool(2));
        let seen: Arc<Mutex<Vec<(SignerLaneId, u64)>>> = Arc::new(Mutex::new(Vec::new()));

        let threads: Vec<_> = (0..2)
            .map(|_| {
                let pool = Arc::clone(&pool);
                let seen = Arc::clone(&seen);
                loom::thread::spawn(move || {
                    if let Ok(lane) = pool.assign(&requirements()) {
                        let n = lane.reserve_nonce(0, UnixNanos(0));
                        if let Ok(mut s) = seen.lock() {
                            s.push((n.lane(), n.get()));
                        }
                        assert_eq!(n.lane(), lane.lane(), "a reservation escaped its lane");
                    }
                })
            })
            .collect();
        for t in threads {
            let _ = t.join();
        }

        let mut s = seen.lock().expect("recording mutex");
        let before = s.len();
        s.sort_unstable();
        s.dedup();
        assert_eq!(s.len(), before, "the same (lane, nonce) was handed out twice");
    });
}

/// **Lane exclusivity, which is a different property from nonce uniqueness and
/// needs its own assertion.**
///
/// Found by mutation: deleting `assign`'s re-check under the lock -- the line
/// that stops two threads both claiming a lane they both surveyed as free --
/// does **not** make `no_cross_lane_nonce_reuse` fail. It cannot, because
/// `NonceLane::reserve` is itself atomic: two holders of one lane still get
/// distinct numbers out of it. The re-check buys exclusivity, not uniqueness,
/// and a model that only asserted uniqueness would have let it be deleted.
///
/// So this asserts exclusivity directly: at most one holder of a lane at a
/// time, observed from inside the critical section.
#[test]
fn a_lane_has_at_most_one_holder() {
    loom::model(|| {
        let pool = Arc::new(pool(2));
        let holders: Arc<Mutex<[u32; 2]>> = Arc::new(Mutex::new([0; 2]));

        let threads: Vec<_> = (0..2)
            .map(|_| {
                let pool = Arc::clone(&pool);
                let holders = Arc::clone(&holders);
                loom::thread::spawn(move || {
                    if let Ok(lane) = pool.assign(&requirements()) {
                        let i = lane.lane().0 as usize;
                        if let Ok(mut h) = holders.lock() {
                            h[i] += 1;
                            assert_eq!(h[i], 1, "lane {i} has two holders at once");
                        }
                        if let Ok(mut h) = holders.lock() {
                            h[i] -= 1;
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            let _ = t.join();
        }
    });
}

/// One lane, taken and released. The hazard here is the release becoming
/// visible before the reservation that preceded it, so the second holder reads
/// a stale `reserved_nonce` and reissues the first holder's number.
#[test]
fn a_released_lane_does_not_reissue_its_last_nonce() {
    loom::model(|| {
        let pool = Arc::new(pool(1));
        let seen: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));

        let threads: Vec<_> = (0..2)
            .map(|_| {
                let pool = Arc::clone(&pool);
                let seen = Arc::clone(&seen);
                loom::thread::spawn(move || {
                    if let Ok(lane) = pool.assign(&requirements()) {
                        let n = lane.reserve_nonce(0, UnixNanos(0));
                        if let Ok(mut s) = seen.lock() {
                            s.push(n.get());
                        }
                        // Dropped here, inside the thread, so the release races
                        // the other thread's assign.
                    }
                })
            })
            .collect();
        for t in threads {
            let _ = t.join();
        }

        let mut s = seen.lock().expect("recording mutex");
        let before = s.len();
        s.sort_unstable();
        s.dedup();
        assert_eq!(s.len(), before, "one lane issued the same nonce twice");
    });
}
