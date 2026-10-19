//! R27, R29: which lender a size borrows from.
//!
//! The lean executor borrows from Morpho Blue directly or from Balancer's
//! vault. Measured on a fork of Base (R29, block 52,671,989), Morpho's loan is
//! 12,647 gas cheaper, so a size borrows from the cheapest lender that holds
//! enough of it.

use apex_exec::lean::LeanLender;
use apex_runtime::live::frontier::BALANCER_FLASH;
use apex_runtime::live::lenders::{Lenders, BALANCER, BALANCER_EXTRA_GAS, MORPHO, MORPHO_FLASH};
use ethers_core::types::U256;

fn weth(n: u64) -> U256 {
    U256::from(n) * U256::exp10(18)
}

#[test]
fn each_choice_names_its_lender_and_its_gas() {
    assert_eq!((MORPHO.id, MORPHO.lender, MORPHO.extra_gas), (MORPHO_FLASH, LeanLender::Morpho, 0));
    assert_eq!((BALANCER.id, BALANCER.lender, BALANCER.extra_gas), (BALANCER_FLASH, LeanLender::Balancer, BALANCER_EXTRA_GAS));
    assert_eq!(BALANCER_EXTRA_GAS, 12_700, "the fork's 12,647, rounded up");
}

#[test]
fn the_cheapest_lender_that_covers_the_size_is_chosen() {
    let both = Lenders { balancer: weth(19), morpho: Some(weth(82_679)) };
    for size in [U256::one(), weth(19), weth(19) + 1, weth(82_679)] {
        assert_eq!(both.for_size(size), Some(MORPHO), "{size}: Morpho is the cheaper and holds enough");
    }
    assert_eq!(both.for_size(weth(82_679) + 1), None, "neither holds it");

    let thin = Lenders { balancer: weth(19), morpho: Some(weth(5)) };
    assert_eq!(thin.for_size(weth(5)), Some(MORPHO));
    assert_eq!(thin.for_size(weth(5) + 1), Some(BALANCER), "Morpho cannot cover it; Balancer can");
    assert_eq!(thin.for_size(weth(19) + 1), None);

    let balancer_only = Lenders { balancer: weth(19), morpho: None };
    assert_eq!(balancer_only.for_size(weth(19)), Some(BALANCER));
    assert_eq!(balancer_only.for_size(weth(19) + 1), None);
}

#[test]
fn the_cap_is_the_largest_holding() {
    assert_eq!(Lenders { balancer: weth(19), morpho: Some(weth(82_679)) }.cap(), weth(82_679));
    assert_eq!(Lenders { balancer: weth(19), morpho: Some(weth(5)) }.cap(), weth(19));
    assert_eq!(Lenders { balancer: weth(19), morpho: None }.cap(), weth(19));
}

#[test]
fn the_extra_gas_is_the_chosen_lenders() {
    let thin = Lenders { balancer: weth(19), morpho: Some(weth(5)) };
    assert_eq!(thin.extra_gas(weth(1)), 0);
    assert_eq!(thin.extra_gas(weth(6)), BALANCER_EXTRA_GAS);
    assert_eq!(thin.extra_gas(weth(20)), 0, "no lender: the size is refused elsewhere, not charged here");
}
