//! Task 8.5 R28.1: a `Mint` or `Burn` applied to a tick ladder in place
//! (`TickLadder::apply_position`), so the live book stops reloading a pool's
//! whole ladder for every position change.
//!
//! The rule it must keep is v3-core's `Tick.update`: at both of a position's
//! ticks `liquidityGross` moves by the position's liquidity, and `liquidityNet`
//! rises by it at the lower tick and falls by it at the upper. A tick is
//! initialized while its gross is above zero.
//!
//! A ladder read with gross (`TickLadder::with_gross`) applies every change
//! exactly: a tick whose gross reaches zero is gone, whatever its net was on the
//! way. A ladder of net alone cannot tell a burn that empties a tick from one
//! that leaves it initialized at a zero net. It keeps the tick and says which
//! one, because a ladder missing an initialized tick over-states the output by a
//! unit (R11), while one holding a stale zero-net tick under-states it.

use apex_math::cl_swap::{PositionApplied, PositionUnknown, TickLadder};

fn ladder() -> TickLadder {
    TickLadder::new(vec![(-200, 5), (-100, 7), (100, -7), (200, -5)], -1_000, 1_000)
}

/// The same ticks, read with gross: -100 and 100 carry a second position of 4
/// whose nets cancel, so their gross is 11, not 7.
fn read() -> TickLadder {
    TickLadder::with_gross(vec![(-200, 5, 5), (-100, 11, 7), (100, 11, -7), (200, 5, -5)], -1_000, 1_000)
}

fn exact(p: Result<PositionApplied, PositionUnknown>) -> TickLadder {
    let p = p.expect("applied");
    assert_eq!(p.unconfirmed, None, "a change that should have applied exactly");
    p.ladder
}

#[test]
fn a_mint_adds_at_its_lower_tick_and_subtracts_at_its_upper() {
    let l = exact(ladder().apply_position(-100, 200, 3));
    assert_eq!(l.ticks(), &[(-200, 5), (-100, 10), (100, -7), (200, -8)]);
}

#[test]
fn a_mint_initializes_ticks_the_ladder_did_not_hold() {
    let l = exact(ladder().apply_position(-150, 150, 4));
    assert_eq!(l.ticks(), &[(-200, 5), (-150, 4), (-100, 7), (100, -7), (150, -4), (200, -5)]);
}

/// A mint that brings a tick's net to zero still leaves it initialized: its
/// gross rose. The tick stays, at zero.
#[test]
fn a_mint_to_a_zero_net_keeps_the_tick() {
    let l = exact(ladder().apply_position(100, 300, 7));
    assert_eq!(l.ticks(), &[(-200, 5), (-100, 7), (100, 0), (200, -5), (300, -7)]);
}

#[test]
fn a_burn_subtracts_at_its_lower_tick_and_adds_at_its_upper() {
    let l = exact(ladder().apply_position(-200, 100, -2));
    assert_eq!(l.ticks(), &[(-200, 3), (-100, 7), (100, -5), (200, -5)]);
}

/// Without gross, a burn that leaves a zero net is applied with the tick kept,
/// and the tick is named: the ladder is exact only once read again.
#[test]
fn without_gross_a_burn_to_a_zero_net_keeps_the_tick_and_names_it() {
    let p = ladder().apply_position(-200, 200, -5).unwrap();
    assert_eq!(p.ladder.ticks(), &[(-200, 0), (-100, 7), (100, -7), (200, 0)]);
    assert_eq!(p.unconfirmed, Some(-200), "the first tick left in doubt");
}

/// With gross, the burn that empties a tick removes it...
#[test]
fn with_gross_a_burn_that_empties_a_tick_removes_it() {
    let l = exact(read().apply_position(-200, 200, -5));
    assert_eq!(l.ticks(), &[(-100, 7), (100, -7)]);
    assert_eq!(l.gross(), Some(&[11u128, 11][..]));
}

/// ...and one that leaves gross behind keeps it, at a zero net.
#[test]
fn with_gross_a_burn_to_a_zero_net_that_leaves_gross_keeps_the_tick() {
    let l = exact(read().apply_position(-100, 100, -7));
    assert_eq!(l.ticks(), &[(-200, 5), (-100, 0), (100, 0), (200, -5)]);
    assert_eq!(l.gross(), Some(&[5u128, 4, 4, 5][..]));
}

/// A mint on a tick the read did not hold starts its gross at the mint.
#[test]
fn with_gross_a_mint_on_a_new_tick_starts_its_gross() {
    let l = exact(read().apply_position(-150, 100, 6));
    assert_eq!(l.ticks(), &[(-200, 5), (-150, 6), (-100, 7), (100, -13), (200, -5)]);
    assert_eq!(l.gross(), Some(&[5u128, 6, 11, 17, 5][..]));
}

/// The just-in-time pattern: a mint, then the same burn. The ladder is back
/// where it was, gross and all.
#[test]
fn a_mint_and_its_burn_leave_the_ladder_as_read() {
    let minted = exact(read().apply_position(-150, 150, 9));
    assert_eq!(exact(minted.apply_position(-150, 150, -9)), read());
}

/// A burn beyond a tick's gross, or one that empties gross with a net left
/// over, means the ladder is not the chain's: refused.
#[test]
fn with_gross_a_burn_the_ladder_cannot_hold_is_refused() {
    assert_eq!(read().apply_position(-200, 200, -6), Err(PositionUnknown::Inconsistent(-200)));
    let lopsided = TickLadder::with_gross(vec![(-200, 5, 3), (200, 5, -5)], -1_000, 1_000);
    assert_eq!(lopsided.apply_position(-200, 200, -5), Err(PositionUnknown::Inconsistent(-200)));
}

/// Both ticks or neither: an upper tick that cannot be applied refuses the
/// whole position.
#[test]
fn a_position_applies_whole_or_not_at_all() {
    assert_eq!(ladder().apply_position(-200, 150, -1), Err(PositionUnknown::NotHeld(150)));
}

/// A burn on a tick the ladder does not hold, inside its proven range, means the
/// ladder is missing something: refused.
#[test]
fn a_burn_on_a_tick_the_ladder_does_not_hold_is_refused() {
    assert_eq!(ladder().apply_position(-150, 100, -1), Err(PositionUnknown::NotHeld(-150)));
    assert_eq!(read().apply_position(-150, 100, -1), Err(PositionUnknown::NotHeld(-150)));
}

/// A position whose ticks are not in order is no position v3-core would log.
#[test]
fn a_position_out_of_order_is_refused() {
    assert_eq!(ladder().apply_position(100, -100, 1), Err(PositionUnknown::Inconsistent(100)));
    assert_eq!(ladder().apply_position(100, 100, 1), Err(PositionUnknown::Inconsistent(100)));
}

/// Outside the range the ladder proved, a tick is not the ladder's to change:
/// skipped, the other tick still applied.
#[test]
fn a_tick_outside_the_proven_range_is_skipped() {
    let l = exact(ladder().apply_position(-5_000, 100, 2));
    assert_eq!(l.ticks(), &[(-200, 5), (-100, 7), (100, -9), (200, -5)]);
    let l = exact(read().apply_position(-100, 5_000, -1));
    assert_eq!(l.ticks(), &[(-200, 5), (-100, 6), (100, -7), (200, -5)]);
    assert_eq!(l.gross(), Some(&[5u128, 10, 11, 5][..]));
    assert_eq!((l.lower_bound(), l.upper_bound()), (-1_000, 1_000), "the proven range is unchanged");
}

/// Not even a tick of zero where the ladder held none.
#[test]
fn a_zero_delta_changes_nothing() {
    assert_eq!(exact(read().apply_position(-150, 150, 0)), read());
}

/// A net that would overflow is not guessed at.
#[test]
fn an_overflowing_net_is_refused() {
    let l = TickLadder::new(vec![(-100, i128::MAX), (100, -i128::MAX)], -1_000, 1_000);
    assert_eq!(l.apply_position(-100, 100, 1), Err(PositionUnknown::Overflow(-100)));
}

/// `with_gross` sorts and deduplicates as `new` does, gross staying with its
/// tick.
#[test]
fn a_ladder_read_with_gross_keeps_each_gross_with_its_tick() {
    let l = TickLadder::with_gross(vec![(100, 11, -7), (-100, 11, 7), (100, 99, 99)], -1_000, 1_000);
    assert_eq!(l.ticks(), &[(-100, 7), (100, -7)]);
    assert_eq!(l.gross(), Some(&[11u128, 11][..]));
    assert_eq!(ladder().gross(), None);
}
