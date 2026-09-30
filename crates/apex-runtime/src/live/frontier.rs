//! The live frontier: 2-hop cycles over the pool book (Task 8.5 R4).
//!
//! # Which cycles
//!
//! For every pair in the book carrying two or more pools, every ordered pair of
//! distinct pools `(A, B)` is a cycle `start → A → other → B → start`. The event
//! census priced exactly this shape and nothing deeper, and this repository's
//! ~1,200-sample census found 3- and 4-hop routes strictly worse.
//!
//! **Native-token cycles only, for now.** `LiveEconomics` compares the gross —
//! output minus input, in the cycle's own token — against costs in wei, so a
//! cycle is priced exactly only when it starts and ends in WETH. A cycle through
//! USDC/cbBTC would need a token price in the economics, which it does not take
//! yet; pricing it anyway would compare USDC against wei.
//!
//! # Which capital
//!
//! The executor holds no inventory, so every cycle borrows its input from the
//! Balancer vault — zero-fee on Base. The template names it; the call builder
//! encodes it; last-mile revalidation checks the vault can actually lend it.

use crate::live::book::PoolSnapshot;
use crate::live::inventory::Venue;
use alloy_primitives::{address, keccak256, Address, B256};
use apex_search::frontier::{
    FeeVariant, Frontier, GasClass, RouteId, RouteTemplate, TickNeighborhood,
};
use apex_types::ids::{ChainId, FlashProviderId, PoolId, TokenId};
use apex_types::route::{ComplexityCost, RouteCommitment, RouteHop};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Wrapped ether on Base.
pub const WETH: Address = address!("4200000000000000000000000000000000000006");

/// The Balancer vault's flash loans. `FlashProviderId(0)` is "borrows nothing",
/// so the first real provider is 1.
pub const BALANCER_FLASH: FlashProviderId = FlashProviderId(1);

/// Domain separator for [`route_hash`].
pub const ROUTE_HASH_DOMAIN: &[u8] = b"apex.route.v1";

/// One hop of a cycle, as the pricer and the call builder both need it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leg {
    pub pool: Address,
    pub venue: Venue,
    pub token_in: Address,
    pub token_out: Address,
    /// Selling token0 for token1, which moves the price down.
    pub zero_for_one: bool,
    pub fee_ppm: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cycle {
    pub id: RouteId,
    pub start: Address,
    pub legs: [Leg; 2],
    pub commitment: RouteCommitment,
}

/// keccak over the chain and the normalized hop list — §25's dedup key. The
/// domain separator keeps it from colliding with any other hash in the system,
/// and every field of every hop is in it: two cycles over the same pools in
/// opposite directions are different trades.
pub fn route_hash(chain: ChainId, legs: &[Leg]) -> B256 {
    let mut buf = Vec::with_capacity(ROUTE_HASH_DOMAIN.len() + 8 + legs.len() * 66);
    buf.extend_from_slice(ROUTE_HASH_DOMAIN);
    buf.extend_from_slice(&chain.0.to_be_bytes());
    for l in legs {
        buf.extend_from_slice(&l.venue.id().0.to_be_bytes());
        buf.extend_from_slice(l.pool.as_slice());
        buf.extend_from_slice(l.token_in.as_slice());
        buf.extend_from_slice(l.token_out.as_slice());
        buf.extend_from_slice(&l.fee_ppm.to_be_bytes());
    }
    keccak256(&buf)
}

fn leg(p: &PoolSnapshot, token_in: Address) -> Leg {
    let zero_for_one = token_in == p.spec.token0;
    Leg {
        pool: p.spec.pool,
        venue: p.spec.venue,
        token_in,
        token_out: if zero_for_one { p.spec.token1 } else { p.spec.token0 },
        zero_for_one,
        fee_ppm: p.state.fee_ppm,
    }
}

/// Every 2-hop cycle from `start` over the pools in `snapshot`.
pub fn cycles(
    chain: ChainId,
    start: Address,
    snapshot: &BTreeMap<Address, Arc<PoolSnapshot>>,
) -> Vec<Cycle> {
    let mut by_pair: BTreeMap<(Address, Address), Vec<&Arc<PoolSnapshot>>> = BTreeMap::new();
    for p in snapshot.values() {
        if p.spec.token0 == start || p.spec.token1 == start {
            by_pair.entry(p.spec.pair()).or_default().push(p);
        }
    }
    let mut out = Vec::new();
    for pools in by_pair.values().filter(|v| v.len() >= 2) {
        for a in pools {
            for b in pools {
                if a.spec.pool == b.spec.pool {
                    continue;
                }
                let first = leg(a, start);
                let second = leg(b, first.token_out);
                let legs = [first, second];
                let hash = route_hash(chain, &legs);
                let id = RouteId(u64::from_be_bytes(hash.0[..8].try_into().unwrap_or([0; 8])));
                let commitment = RouteCommitment {
                    hops: legs
                        .iter()
                        .map(|l| RouteHop {
                            venue: l.venue.id(),
                            pool: PoolId { chain, address: l.pool },
                            token_in: TokenId { chain, address: l.token_in },
                            token_out: TokenId { chain, address: l.token_out },
                            fee_ppm: l.fee_ppm,
                        })
                        .collect(),
                    complexity_cost: ComplexityCost {
                        hops: 2,
                        external_calls: 2,
                        calldata_bytes: 132 + 2 * 196,
                        state_deps: 2,
                        tick_crossings: 0,
                        hooks: 0,
                        gas_estimate: 455_000,
                        failure_surface: 0.0,
                    },
                    route_hash: hash,
                };
                out.push(Cycle { id, start, legs, commitment });
            }
        }
    }
    out
}

/// The frontier's view of a cycle.
pub fn template(chain: ChainId, c: &Cycle, snapshot: &BTreeMap<Address, Arc<PoolSnapshot>>) -> RouteTemplate {
    RouteTemplate {
        id: c.id,
        chain,
        topology: vec![
            TokenId { chain, address: c.start },
            TokenId { chain, address: c.legs[0].token_out },
            TokenId { chain, address: c.start },
        ],
        venue_sequence: c.legs.iter().map(|l| l.venue.id()).collect(),
        fee_variants: c
            .legs
            .iter()
            .map(|l| FeeVariant {
                venue: l.venue.id(),
                pool: PoolId { chain, address: l.pool },
                fee_ppm: l.fee_ppm,
            })
            .collect(),
        tick_neighborhood: c
            .legs
            .iter()
            .filter_map(|l| {
                snapshot.get(&l.pool).map(|p| {
                    (
                        PoolId { chain, address: l.pool },
                        TickNeighborhood { lower: p.ladder.lower_bound(), upper: p.ladder.upper_bound() },
                    )
                })
            })
            .collect(),
        hook_fingerprint: None,
        flash_source: BALANCER_FLASH,
        expected_gas_class: GasClass::TwoHopConcentrated,
        last_profitable: None,
    }
}

/// The frontier and the cycles behind it, keyed by the same id.
pub fn build(
    chain: ChainId,
    start: Address,
    snapshot: &BTreeMap<Address, Arc<PoolSnapshot>>,
) -> (Frontier, BTreeMap<RouteId, Cycle>) {
    let mut frontier = Frontier::new();
    let mut by_id = BTreeMap::new();
    for c in cycles(chain, start, snapshot) {
        // A template the frontier refuses as inconsistent is not priced either.
        if frontier.insert(template(chain, &c, snapshot)).is_ok() {
            by_id.insert(c.id, c);
        }
    }
    (frontier, by_id)
}
