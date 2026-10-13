//! R27: which lender a size borrows from.

use alloy_primitives::{address, Address};
use apex_exec::commitment::LoanProvider;
use apex_runtime::live::frontier::{BALANCER_FLASH, BALANCER_VAULT, WETH};
use apex_runtime::live::lenders::{loan, morpho, Lenders, BALANCER, MORPHO_EXTRA_GAS, MORPHO_FLASH};
use ethers_core::types::U256;

const LENDER: Address = address!("00000000000000000000000000000000000000ee");

fn weth(n: u64) -> U256 {
    U256::from(n) * U256::exp10(18)
}

#[test]
fn a_size_borrows_from_balancer_while_it_covers_it_then_from_morpho() {
    let both = Lenders { balancer: weth(19), morpho: Some((LENDER, weth(82_679))) };
    assert_eq!(both.cap(), weth(82_679));
    assert_eq!(both.for_size(weth(1)), Some(BALANCER));
    assert_eq!(both.for_size(weth(19)), Some(BALANCER), "the whole holding is lendable");
    assert_eq!(both.for_size(weth(19) + 1), Some(morpho(LENDER)));
    assert_eq!(both.for_size(weth(82_679) + 1), None, "past both");
    assert_eq!((both.extra_gas(weth(19)), both.extra_gas(weth(20))), (0, MORPHO_EXTRA_GAS));
    assert_eq!((morpho(LENDER).id, morpho(LENDER).provider), (MORPHO_FLASH, LoanProvider::Erc3156));
    assert_eq!((BALANCER.id, BALANCER.provider, BALANCER.address), (BALANCER_FLASH, LoanProvider::Balancer, BALANCER_VAULT));

    let alone = Lenders::balancer_only(weth(19));
    assert_eq!((alone.cap(), alone.for_size(weth(20))), (weth(19), None));
    // Morpho holding less than Balancer still caps at the larger.
    let thin = Lenders { balancer: weth(19), morpho: Some((LENDER, weth(5))) };
    assert_eq!((thin.cap(), thin.for_size(weth(10))), (weth(19), Some(BALANCER)));
}

/// The deployed executor counts Aave's later pull as profit (simulated
/// 2026-10-13): no Aave loan is ever built.
#[test]
fn a_loan_names_its_lender_and_never_aave() {
    let amount = alloy_primitives::U256::from(5u64);
    let b = loan(&BALANCER, WETH, amount).unwrap();
    assert_eq!((b.token, b.amount, b.provider, b.provider_addr), (WETH, amount, LoanProvider::Balancer, BALANCER_VAULT));
    let m = loan(&morpho(LENDER), WETH, amount).unwrap();
    assert_eq!((m.provider, m.provider_addr), (LoanProvider::Erc3156, LENDER));
    let aave = apex_runtime::live::lenders::LenderChoice { provider: LoanProvider::Aave, ..BALANCER };
    assert!(loan(&aave, WETH, amount).unwrap_err().contains("cannot settle"));
}
