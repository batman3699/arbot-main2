use ethers::types::{Address, Bytes, U256};

use crate::abi::{ExecutorLoan, ExecutorPlan, ExecutorStep};

pub const START_V2_SIGNATURE: &str =
    "startV2((address,uint256,uint8,address)[],uint16,(uint8,bytes)[],uint256)";

pub fn fixture_plan_v2() -> ExecutorPlan {
    let loan = ExecutorLoan {
        token: Address::from_low_u64_be(0x1001),
        amount: U256::from(1_000_000u64),
        provider: 2u8,
        provider_addr: Address::from_low_u64_be(0x2002),
    };

    let steps = vec![
        ExecutorStep {
            op: 0u8,
            data: Bytes::from(vec![0x11, 0x22, 0x33]),
        },
        ExecutorStep {
            op: 2u8,
            data: Bytes::from(vec![0xaa, 0xbb, 0xcc, 0xdd]),
        },
    ];

    ExecutorPlan {
        loans: vec![loan],
        cycle_slippage_bps: 25,
        steps,
        min_profit: U256::from(123_456u64),
    }
}
