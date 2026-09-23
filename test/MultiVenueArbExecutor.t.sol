// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {MultiVenueArbImplementation, ISwapRouter02, IBalancerVault, NotExecutor} from "../contracts/executor/MultiVenueArbImplementation.sol";
import {ArbitrageCloneFactory} from "../contracts/executor/ArbitrageCloneFactory.sol";
import {BatchRouter} from "../contracts/executor/BatchRouter.sol";
import {MockERC20} from "../contracts/mocks/MockERC20.sol";
import {MockERC3156Lender, ERC3156CallbackProxy} from "../contracts/mocks/MockERC3156Lender.sol";
import {MockUSDT} from "../contracts/mocks/MockUSDT.sol";
import {MockUniswapV2Pair, MockUniswapV3FlashPool} from "../contracts/mocks/MockUniFlashPools.sol";
import {DebtNotRepaid} from "../contracts/core/ProfitInvariant.sol";
import {Test} from "forge-std/Test.sol";

contract MockPermit2 {
    struct AllowanceData {
        uint160 amount;
        uint48 expiration;
    }

    mapping(address => mapping(address => mapping(address => AllowanceData))) private _allowances;

    function approve(address token, address spender, uint160 amount, uint48 expiration) external {
        _allowances[msg.sender][token][spender] = AllowanceData({amount: amount, expiration: expiration});
    }

    function allowance(address owner, address token, address spender)
        external
        view
        returns (uint160 amount, uint48 expiration, uint48)
    {
        AllowanceData storage data = _allowances[owner][token][spender];
        return (data.amount, data.expiration, 0);
    }

    function transferFrom(address from, address to, uint160 amount, address token) external payable {
        AllowanceData storage data = _allowances[from][token][msg.sender];
        if (data.expiration != 0) {
            require(data.expiration >= block.timestamp, "expired");
        }
        require(data.amount >= amount, "allowance");
        if (data.amount != type(uint160).max) {
            data.amount = data.amount - amount;
        }
        MockERC20(token).transferFrom(from, to, uint256(amount));
    }
}


contract MockSwapRouter is ISwapRouter02 {
    address public immutable tokenIn;
    address public immutable tokenOut;
    uint256 public swapCount;

    constructor(address tokenIn_, address tokenOut_) {
        tokenIn = tokenIn_;
        tokenOut = tokenOut_;
    }

    function exactInput(ExactInputParams calldata params) external payable override returns (uint256) {
        require(_addressAt(params.path, 0) == tokenIn, "tokenIn");
        require(_addressAt(params.path, params.path.length - 20) == tokenOut, "tokenOut");
        require(MockERC20(tokenIn).transferFrom(msg.sender, address(this), params.amountIn), "router in");
        require(MockERC20(tokenOut).transfer(params.recipient, params.amountOutMinimum), "router out");
        ++swapCount;
        return params.amountOutMinimum;
    }

    function _addressAt(bytes memory path, uint256 start) internal pure returns (address addr) {
        assembly {
            addr := shr(96, mload(add(add(path, 0x20), start)))
        }
    }
}




contract MockBalancerVault is IBalancerVault {
    MultiVenueArbImplementation public immutable executor;
    uint256 public swapCount;
    int256 public lastLimitIn;
    int256 public lastLimitOut;

    constructor(MultiVenueArbImplementation executor_) {
        executor = executor_;
    }

    function trigger(address[] memory tokens, uint256[] memory amounts, uint256[] memory fees, bytes memory userData)
        external
    {
        executor.receiveFlashLoan(tokens, amounts, fees, userData);
    }

    function flashLoan(address, address[] memory tokens, uint256[] memory amounts, bytes memory userData)
        external
        override
    {
        uint256[] memory fees = new uint256[](amounts.length);
        executor.receiveFlashLoan(tokens, amounts, fees, userData);
    }

    function batchSwap(
        SwapKind kind,
        BatchSwapStep[] calldata swaps,
        address[] calldata assets,
        FundManagement calldata,
        int256[] calldata limits,
        uint256
    ) external override returns (int256[] memory assetDeltas) {
        require(kind == SwapKind.GIVEN_IN, "kind");
        require(MockERC20(assets[0]).transferFrom(msg.sender, address(this), swaps[0].amount), "vault in");
        lastLimitIn = limits[0];
        lastLimitOut = limits[1];
        uint256 amountOut = uint256(-limits[1]);
        require(MockERC20(assets[1]).transfer(msg.sender, amountOut), "vault out");
        ++swapCount;
        assetDeltas = new int256[](2);
        assetDeltas[0] = int256(swaps[0].amount);
        assetDeltas[1] = -int256(amountOut);
    }
}

contract BridgeVaultMock {
    MultiVenueArbImplementation public immutable executor;

    constructor(MultiVenueArbImplementation executor_) {
        executor = executor_;
    }

    function trigger(address[] memory tokens, uint256[] memory amounts, uint256[] memory fees, bytes memory userData)
        external
    {
        executor.receiveFlashLoan(tokens, amounts, fees, userData);
    }

    function flashLoan(address, address[] memory tokens, uint256[] memory amounts, bytes memory userData) external {
        uint256[] memory fees = new uint256[](amounts.length);
        executor.receiveFlashLoan(tokens, amounts, fees, userData);
    }
}

contract StrictMockERC20 {
    string public name;
    string public symbol;
    uint8 public immutable decimals;
    uint256 public totalSupply;

    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    event Transfer(address indexed from, address indexed to, uint256 value);
    event Approval(address indexed owner, address indexed spender, uint256 value);

    constructor(string memory _name, string memory _symbol, uint8 _decimals) {
        name = _name;
        symbol = _symbol;
        decimals = _decimals;
    }

    function mint(address to, uint256 amount) external {
        balanceOf[to] += amount;
        totalSupply += amount;
        emit Transfer(address(0), to, amount);
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        if (allowance[msg.sender][spender] != 0 && amount != 0) {
            revert("reset required");
        }
        allowance[msg.sender][spender] = amount;
        emit Approval(msg.sender, spender, amount);
        return true;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        _transfer(msg.sender, to, amount);
        return true;
    }

    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        uint256 allowed = allowance[from][msg.sender];
        if (allowed != type(uint256).max) {
            require(allowed >= amount, "allowance");
            allowance[from][msg.sender] = allowed - amount;
        }
        _transfer(from, to, amount);
        return true;
    }

    function _transfer(address from, address to, uint256 amount) internal {
        require(balanceOf[from] >= amount, "balance");
        balanceOf[from] -= amount;
        balanceOf[to] += amount;
        emit Transfer(from, to, amount);
    }
}

contract ProfitDonor {
    function donate(address token, address to, uint256 amount) external {
        MockERC20(token).transfer(to, amount);
    }
}


contract MockUniV3RouterNoop {
    uint256 public lastAmountIn;

    function exactInput(ISwapRouter02.ExactInputParams calldata params)
        external
        returns (uint256)
    {
        lastAmountIn = params.amountIn;
        return params.amountOutMinimum;
    }
}


contract NonOwnerStarter {
    function callStart(address executor, MultiVenueArbImplementation.PlanLegacy memory plan)
        external
        returns (bool ok)
    {
        (ok,) = executor.call(abi.encodeCall(MultiVenueArbImplementation.start, (plan)));
    }
}

contract NonOwnerBatchRouterCaller {
    function callStart(address router, MultiVenueArbImplementation.PlanLegacy memory plan) external returns (bool ok) {
        (ok,) = router.call(abi.encodeCall(BatchRouter.start, (plan)));
    }

    function callStartV2(address router, MultiVenueArbImplementation.PlanV2 memory plan) external returns (bool ok) {
        (ok,) = router.call(abi.encodeCall(BatchRouter.startV2, (plan)));
    }

    function callMulticall(address router, address[] memory targets, bytes[] memory data)
        external
        returns (bool ok)
    {
        (ok,) = router.call(abi.encodeCall(BatchRouter.multicall, (targets, data)));
    }
}

contract NonOwnerConfigurator {
    function callUpdateConfig(address executor, uint16 feeBps, uint16 maxSlippageBps, uint32 deadlineBuffer)
        external
        returns (bool ok)
    {
        (ok,) = executor.call(
            abi.encodeCall(MultiVenueArbImplementation.updateConfig, (feeBps, maxSlippageBps, deadlineBuffer))
        );
    }
}

contract BatchRouterTargetMock {
    uint256 public calls;

    function ping() external {
        calls += 1;
    }
}

contract CurveStyleGenericTarget {
    StrictMockERC20 public immutable tokenIn;
    MockERC20 public immutable tokenOut;

    address public lastCaller;
    uint256 public lastAmountIn;
    uint256 public lastMinOut;
    address public lastRecipient;

    constructor(address tokenIn_, address tokenOut_) {
        tokenIn = StrictMockERC20(tokenIn_);
        tokenOut = MockERC20(tokenOut_);
    }

    function noop() external {
        lastCaller = msg.sender;
    }

    function exchange(uint256 amountIn, uint256 minAmountOut, address recipient) external returns (uint256 amountOut) {
        lastCaller = msg.sender;
        lastAmountIn = amountIn;
        lastMinOut = minAmountOut;
        lastRecipient = recipient;

        tokenIn.transferFrom(msg.sender, address(this), amountIn);

        amountOut = amountIn * 2;
        require(amountOut >= minAmountOut, "slippage");
        require(tokenOut.balanceOf(address(this)) >= amountOut, "liquidity");
        tokenOut.transfer(recipient, amountOut);
    }
}

contract FalseReturnERC20 {
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    function mint(address to, uint256 amount) external {
        balanceOf[to] += amount;
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        return false;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        if (balanceOf[msg.sender] >= amount) {
            balanceOf[msg.sender] -= amount;
            balanceOf[to] += amount;
        }
        return false;
    }

    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        uint256 allowed = allowance[from][msg.sender];
        if (allowed >= amount && balanceOf[from] >= amount) {
            allowance[from][msg.sender] = allowed - amount;
            balanceOf[from] -= amount;
            balanceOf[to] += amount;
        }
        return false;
    }
}

contract MultiVenueArbExecutorTest is Test {
    function _legacyCtx(MultiVenueArbImplementation.PlanLegacy memory plan) internal pure returns (bytes memory) {
        return abi.encode(uint8(1), abi.encode(plan));
    }

    function _v2Ctx(MultiVenueArbImplementation.PlanV2 memory plan) internal pure returns (bytes memory) {
        return abi.encode(uint8(2), abi.encode(plan));
    }

    function testImplementationRuntimeSizeStaysWithinLimit() external {
        MultiVenueArbImplementation implementation = new MultiVenueArbImplementation();
        assertLt(address(implementation).code.length, 24_576, "implementation runtime exceeds EIP-170");
    }

    function testCloneFactoryDeploysMinimalProxy() external {
        MultiVenueArbImplementation implementation = new MultiVenueArbImplementation();
        ArbitrageCloneFactory factory = new ArbitrageCloneFactory(address(implementation));

        address clone = factory.deployClone(bytes32("salt"));

        assertGt(clone.code.length, 0, "clone should contain runtime code");
    }

    function testCloneFactoryDeployAndInitSetsOwnerAtomically() external {
        MultiVenueArbImplementation implementation = new MultiVenueArbImplementation();
        ArbitrageCloneFactory factory = new ArbitrageCloneFactory(address(implementation));

        bytes memory initData = abi.encodeCall(
            MultiVenueArbImplementation.initialise,
            (address(this), address(1), address(2), address(3), address(4), uint16(300), uint16(150), uint32(900))
        );

        address clone = factory.deployAndInit(bytes32("salt-init"), initData);
        MultiVenueArbImplementation executor = MultiVenueArbImplementation(clone);

        assertEq(executor.owner(), address(this), "owner should be set during deployment tx");
        vm.expectRevert(MultiVenueArbImplementation.AlreadyInitialised.selector);
        executor.initialise(address(this), address(1), address(1), address(0), address(0), 0, 0, 1);
    }

    function testSweepRevertsWhenTokenTransferReturnsFalse() external {
        MultiVenueArbImplementation implementation = new MultiVenueArbImplementation();
        ArbitrageCloneFactory factory = new ArbitrageCloneFactory(address(implementation));
        MultiVenueArbImplementation executor = MultiVenueArbImplementation(factory.deployClone(bytes32("false-transfer")));

        executor.initialise(address(this), address(1), address(1), address(0), address(0), 0, 0, 1);

        FalseReturnERC20 token = new FalseReturnERC20();
        token.mint(address(executor), 1 ether);

        vm.expectRevert();
        executor.sweep(address(token), address(0xBEEF), 1 ether);
    }

    function testGenericActionApproveRevertsWhenTokenApproveReturnsFalse() external {
        MultiVenueArbImplementation implementation = new MultiVenueArbImplementation();
        ArbitrageCloneFactory factory = new ArbitrageCloneFactory(address(implementation));
        MultiVenueArbImplementation executor = MultiVenueArbImplementation(factory.deployClone(bytes32("false-approve-direct")));

        MockPermit2 permit2 = new MockPermit2();
        MockBalancerVault vault = new MockBalancerVault(executor);
        executor.initialise(address(this), address(vault), address(1), address(0), address(permit2), 0, 0, 1);

        FalseReturnERC20 badToken = new FalseReturnERC20();
        MockERC20 loanToken = new MockERC20("Mock Loan", "MLN", 18);
        CurveStyleGenericTarget target = new CurveStyleGenericTarget(address(new StrictMockERC20("S", "S", 18)), address(loanToken));

        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](1);
        steps[0] = MultiVenueArbImplementation.Step({
            op: MultiVenueArbImplementation.Op.GENERIC,
            data: abi.encode(
                address(target),
                abi.encodeWithSelector(CurveStyleGenericTarget.noop.selector),
                uint256(1),
                address(badToken),
                uint256(1)
            )
        });

        MultiVenueArbImplementation.PlanLegacy memory plan = MultiVenueArbImplementation.PlanLegacy({
            loanToken: address(loanToken),
            amountIn: 0,
            loanProvider: MultiVenueArbImplementation.LoanProvider.BALANCER,
            cycleSlippageBps: 0,
            steps: steps,
            minProfit: 0
        });

        vm.expectRevert();
        executor.start(plan);
    }

    function testSetTokenApprovalsRevertsWhenPermit2BootstrapApproveReturnsFalse() external {
        MultiVenueArbImplementation implementation = new MultiVenueArbImplementation();
        ArbitrageCloneFactory factory = new ArbitrageCloneFactory(address(implementation));
        MultiVenueArbImplementation executor = MultiVenueArbImplementation(factory.deployClone(bytes32("false-approve-permit2")));

        MockPermit2 permit2 = new MockPermit2();
        executor.initialise(address(this), address(1), address(1), address(0), address(permit2), 0, 0, 1);

        FalseReturnERC20 badToken = new FalseReturnERC20();
        MultiVenueArbImplementation.TokenApproval[] memory approvals = new MultiVenueArbImplementation.TokenApproval[](1);
        approvals[0] = MultiVenueArbImplementation.TokenApproval({
            token: address(badToken), spender: address(0xC0FFEE), amount: 1
        });

        vm.expectRevert();
        executor.setTokenApprovals(approvals);
    }

    function testUniswapStepRevertsWhenTokenApproveReturnsFalse() external {
        MultiVenueArbImplementation implementation = new MultiVenueArbImplementation();
        ArbitrageCloneFactory factory = new ArbitrageCloneFactory(address(implementation));
        MultiVenueArbImplementation executor = MultiVenueArbImplementation(factory.deployClone(bytes32("false-approve-allowance")));

        MockPermit2 permit2 = new MockPermit2();
        MockBalancerVault vault = new MockBalancerVault(executor);
        FalseReturnERC20 badToken = new FalseReturnERC20();
        MockERC20 tokenOut = new MockERC20("Out", "OUT", 18);
        MockERC20 loanToken = new MockERC20("Mock Loan", "MLN", 18);
        MockSwapRouter router = new MockSwapRouter(address(badToken), address(tokenOut));

        executor.initialise(address(this), address(vault), address(router), address(0), address(permit2), 0, 0, 1);

        bytes memory path = abi.encodePacked(address(badToken), uint24(3000), address(tokenOut));
        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](1);
        steps[0] = MultiVenueArbImplementation.Step({
            op: MultiVenueArbImplementation.Op.UNIV3,
            data: abi.encode(path, uint256(1), uint256(0))
        });

        MultiVenueArbImplementation.PlanLegacy memory plan = MultiVenueArbImplementation.PlanLegacy({
            loanToken: address(loanToken),
            amountIn: 0,
            loanProvider: MultiVenueArbImplementation.LoanProvider.BALANCER,
            cycleSlippageBps: 0,
            steps: steps,
            minProfit: 0
        });

        vm.expectRevert();
        executor.start(plan);
    }


    function testSequentialUniswapSwapsWithZeroFirstToken() external {
        MultiVenueArbImplementation implementation = new MultiVenueArbImplementation();
        ArbitrageCloneFactory factory = new ArbitrageCloneFactory(address(implementation));
        MultiVenueArbImplementation executor = MultiVenueArbImplementation(factory.deployClone(bytes32("uni-usdt")));

        MockPermit2 permit2 = new MockPermit2();
        MockERC20 loanToken = new MockERC20("Mock Loan", "MLN", 18);
        MockUSDT usdt = new MockUSDT();
        MockERC20 weth = new MockERC20("Mock WETH", "MWETH", 18);

        MockBalancerVault vault = new MockBalancerVault(executor);
        MockSwapRouter router = new MockSwapRouter(address(usdt), address(weth));

        executor.initialise(address(this), address(vault), address(router), address(0), address(permit2), 0, 0, 1);

        bytes memory path = abi.encodePacked(address(usdt), uint24(3000), address(weth));
        uint256 amountIn = 1_000_000; // 1 USDT
        uint256 minOut = 900_000;

        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](1);
        steps[0] = MultiVenueArbImplementation.Step({
            op: MultiVenueArbImplementation.Op.UNIV3, data: abi.encode(path, amountIn, minOut)
        });

        MultiVenueArbImplementation.PlanLegacy memory plan = MultiVenueArbImplementation.PlanLegacy({
            loanToken: address(loanToken),
            amountIn: 0,
            loanProvider: MultiVenueArbImplementation.LoanProvider.BALANCER,
            cycleSlippageBps: 0,
            steps: steps,
            minProfit: 0
        });

        for (uint256 i; i < 2; ++i) {
            usdt.mint(address(executor), amountIn);
            weth.mint(address(router), minOut);
            executor.start(plan);
        }

        assertEq(router.swapCount(), 2, "router should execute twice");
        assertEq(usdt.allowance(address(executor), address(router)), type(uint256).max, "allowance should be max");
    }


    function testUniswapStepMalformedPathReverts() external {
        MultiVenueArbImplementation implementation = new MultiVenueArbImplementation();
        ArbitrageCloneFactory factory = new ArbitrageCloneFactory(address(implementation));
        MultiVenueArbImplementation executor = MultiVenueArbImplementation(factory.deployClone(bytes32("uni-bad-path")));

        MockPermit2 permit2 = new MockPermit2();
        MockERC20 loanToken = new MockERC20("Mock Loan", "MLN", 18);
        MockUSDT usdt = new MockUSDT();
        MockERC20 weth = new MockERC20("Mock WETH", "MWETH", 18);

        MockBalancerVault vault = new MockBalancerVault(executor);
        MockSwapRouter router = new MockSwapRouter(address(usdt), address(weth));

        executor.initialise(address(this), address(vault), address(router), address(0), address(permit2), 0, 0, 1);

        bytes memory malformedPath = hex"0102030405060708090a0b0c0d0e0f10111213";

        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](1);
        steps[0] = MultiVenueArbImplementation.Step({
            op: MultiVenueArbImplementation.Op.UNIV3, data: abi.encode(malformedPath, uint256(1_000_000), uint256(900_000))
        });

        MultiVenueArbImplementation.PlanLegacy memory plan = MultiVenueArbImplementation.PlanLegacy({
            loanToken: address(loanToken),
            amountIn: 0,
            loanProvider: MultiVenueArbImplementation.LoanProvider.BALANCER,
            cycleSlippageBps: 0,
            steps: steps,
            minProfit: 0
        });

        vm.expectRevert(MultiVenueArbImplementation.InvalidGenericAction.selector);
        executor.start(plan);
    }

    function testSequentialBalancerSwapsWithZeroFirstToken() external {
        MultiVenueArbImplementation implementation = new MultiVenueArbImplementation();
        ArbitrageCloneFactory factory = new ArbitrageCloneFactory(address(implementation));
        MultiVenueArbImplementation executor = MultiVenueArbImplementation(factory.deployClone(bytes32("bal-usdt")));

        MockPermit2 permit2 = new MockPermit2();
        MockERC20 loanToken = new MockERC20("Mock Loan", "MLN", 18);
        MockUSDT usdt = new MockUSDT();
        MockERC20 dai = new MockERC20("Mock DAI", "mDAI", 18);

        MockBalancerVault vault = new MockBalancerVault(executor);
        MockSwapRouter router = new MockSwapRouter(address(usdt), address(dai));

        executor.initialise(address(this), address(vault), address(router), address(0), address(permit2), 0, 0, 1);

        bytes32 poolId = bytes32(uint256(1));
        uint256 amountIn = 2_000_000;
        uint256 minOut = 1_800_000;

        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](1);
        steps[0] = MultiVenueArbImplementation.Step({
            op: MultiVenueArbImplementation.Op.BALANCER,
            data: abi.encode(poolId, address(usdt), address(dai), amountIn, minOut)
        });

        MultiVenueArbImplementation.PlanLegacy memory plan = MultiVenueArbImplementation.PlanLegacy({
            loanToken: address(loanToken),
            amountIn: 0,
            loanProvider: MultiVenueArbImplementation.LoanProvider.BALANCER,
            cycleSlippageBps: 0,
            steps: steps,
            minProfit: 0
        });

        for (uint256 i; i < 2; ++i) {
            usdt.mint(address(executor), amountIn);
            dai.mint(address(vault), minOut);
            executor.start(plan);
        }

        assertEq(vault.swapCount(), 2, "vault should execute twice");
        assertEq(usdt.allowance(address(executor), address(vault)), type(uint256).max, "allowance should be max");
    }

    function testBalancerStepZeroMinOutReverts() external {
        MultiVenueArbImplementation implementation = new MultiVenueArbImplementation();
        ArbitrageCloneFactory factory = new ArbitrageCloneFactory(address(implementation));
        MultiVenueArbImplementation executor = MultiVenueArbImplementation(factory.deployClone(bytes32("bal-zero")));

        MockPermit2 permit2 = new MockPermit2();
        MockERC20 loanToken = new MockERC20("Mock Loan", "MLN", 18);
        MockUSDT usdt = new MockUSDT();
        MockERC20 dai = new MockERC20("Mock DAI", "mDAI", 18);

        MockBalancerVault vault = new MockBalancerVault(executor);
        MockSwapRouter router = new MockSwapRouter(address(usdt), address(dai));

        executor.initialise(address(this), address(vault), address(router), address(0), address(permit2), 0, 0, 1);

        bytes32 poolId = bytes32(uint256(1));
        uint256 amountIn = 2_000_000;

        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](1);
        steps[0] = MultiVenueArbImplementation.Step({
            op: MultiVenueArbImplementation.Op.BALANCER,
            data: abi.encode(poolId, address(usdt), address(dai), amountIn, uint256(0))
        });

        MultiVenueArbImplementation.PlanLegacy memory plan = MultiVenueArbImplementation.PlanLegacy({
            loanToken: address(loanToken),
            amountIn: 0,
            loanProvider: MultiVenueArbImplementation.LoanProvider.BALANCER,
            cycleSlippageBps: 0,
            steps: steps,
            minProfit: 0
        });

        usdt.mint(address(executor), amountIn);
        vm.expectRevert();
        executor.start(plan);
    }

    function testBalancerStepNegativeMinOutPayloadReverts() external {
        MultiVenueArbImplementation implementation = new MultiVenueArbImplementation();
        ArbitrageCloneFactory factory = new ArbitrageCloneFactory(address(implementation));
        MultiVenueArbImplementation executor = MultiVenueArbImplementation(factory.deployClone(bytes32("bal-neg")));

        MockPermit2 permit2 = new MockPermit2();
        MockERC20 loanToken = new MockERC20("Mock Loan", "MLN", 18);
        MockUSDT usdt = new MockUSDT();
        MockERC20 dai = new MockERC20("Mock DAI", "mDAI", 18);

        MockBalancerVault vault = new MockBalancerVault(executor);
        MockSwapRouter router = new MockSwapRouter(address(usdt), address(dai));

        executor.initialise(address(this), address(vault), address(router), address(0), address(permit2), 0, 0, 1);

        bytes32 poolId = bytes32(uint256(1));
        uint256 amountIn = 2_000_000;

        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](1);
        steps[0] = MultiVenueArbImplementation.Step({
            op: MultiVenueArbImplementation.Op.BALANCER,
            data: abi.encode(poolId, address(usdt), address(dai), amountIn, int256(-1))
        });

        MultiVenueArbImplementation.PlanLegacy memory plan = MultiVenueArbImplementation.PlanLegacy({
            loanToken: address(loanToken),
            amountIn: 0,
            loanProvider: MultiVenueArbImplementation.LoanProvider.BALANCER,
            cycleSlippageBps: 0,
            steps: steps,
            minProfit: 0
        });

        usdt.mint(address(executor), amountIn);
        vm.expectRevert();
        executor.start(plan);
    }

    function testBalancerStepPositiveMinOutSetsLimits() external {
        MultiVenueArbImplementation implementation = new MultiVenueArbImplementation();
        ArbitrageCloneFactory factory = new ArbitrageCloneFactory(address(implementation));
        MultiVenueArbImplementation executor = MultiVenueArbImplementation(factory.deployClone(bytes32("bal-pos")));

        MockPermit2 permit2 = new MockPermit2();
        MockERC20 loanToken = new MockERC20("Mock Loan", "MLN", 18);
        MockUSDT usdt = new MockUSDT();
        MockERC20 dai = new MockERC20("Mock DAI", "mDAI", 18);

        MockBalancerVault vault = new MockBalancerVault(executor);
        MockSwapRouter router = new MockSwapRouter(address(usdt), address(dai));

        executor.initialise(address(this), address(vault), address(router), address(0), address(permit2), 0, 0, 1);

        bytes32 poolId = bytes32(uint256(1));
        uint256 amountIn = 2_000_000;
        uint256 minOut = 1_800_000;

        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](1);
        steps[0] = MultiVenueArbImplementation.Step({
            op: MultiVenueArbImplementation.Op.BALANCER,
            data: abi.encode(poolId, address(usdt), address(dai), amountIn, minOut)
        });

        MultiVenueArbImplementation.PlanLegacy memory plan = MultiVenueArbImplementation.PlanLegacy({
            loanToken: address(loanToken),
            amountIn: 0,
            loanProvider: MultiVenueArbImplementation.LoanProvider.BALANCER,
            cycleSlippageBps: 0,
            steps: steps,
            minProfit: 0
        });

        usdt.mint(address(executor), amountIn);
        dai.mint(address(vault), minOut);
        executor.start(plan);

        assertEq(vault.lastLimitIn(), int256(amountIn), "limit in");
        assertEq(vault.lastLimitOut(), -int256(minOut), "limit out");
    }
}

contract CloneFactoryFallbackTest is Test {
    function testDeployCloneRevertsOnCreate2Collision() external {
        MultiVenueArbImplementation implementation = new MultiVenueArbImplementation();
        ArbitrageCloneFactory factory = new ArbitrageCloneFactory(address(implementation));

        bytes32 salt = bytes32("fallback");
        bytes memory initCode = _minimalProxyBytecode(address(implementation));
        address expectedCreate2 = vm.computeCreate2Address(salt, keccak256(initCode), address(factory));

        // Occupy the CREATE2 address to force a collision.
        vm.etch(expectedCreate2, hex"01");

        vm.expectRevert(abi.encodeWithSelector(ArbitrageCloneFactory.Create2DeploymentFailed.selector, salt));
        factory.deployClone(salt);
    }

    function _minimalProxyBytecode(address impl) internal pure returns (bytes memory) {
        return abi.encodePacked(
            hex"3d602d80600a3d3981f3363d3d373d3d3d363d73", bytes20(impl), hex"5af43d82803e903d91602b57fd5bf3"
        );
    }
}

contract MultiVenueArbExecutorAbiV2Test is Test {
    function testPlanV2RoundTripEncoding() external {
        MultiVenueArbImplementation.Loan[] memory loans = new MultiVenueArbImplementation.Loan[](1);
        loans[0] = MultiVenueArbImplementation.Loan({
            token: address(0xBEEF),
            amount: 42 ether,
            provider: MultiVenueArbImplementation.LoanProvider.ERC3156,
            providerAddr: address(0xCAFE)
        });

        bytes memory genericData = abi.encode(address(11), bytes("call"), uint256(0), address(0x1), uint256(0));
        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](1);
        steps[0] = MultiVenueArbImplementation.Step({op: MultiVenueArbImplementation.Op.GENERIC, data: genericData});

        MultiVenueArbImplementation.PlanV2 memory plan =
            MultiVenueArbImplementation.PlanV2({loans: loans, cycleSlippageBps: 77, steps: steps, minProfit: 1 ether, declaredResidue: 0, commitment: bytes32(0), chainId: 0, deadline: 0});

        bytes memory payload = abi.encode(plan);
        MultiVenueArbImplementation.PlanV2 memory decoded = abi.decode(payload, (MultiVenueArbImplementation.PlanV2));

        assertEq(decoded.loans.length, 1, "loan length");
        assertEq(decoded.loans[0].token, loans[0].token, "token");
        assertEq(decoded.loans[0].amount, loans[0].amount, "amount");
        assertEq(uint8(decoded.loans[0].provider), uint8(loans[0].provider), "provider");
        assertEq(decoded.loans[0].providerAddr, loans[0].providerAddr, "providerAddr");
        assertEq(decoded.cycleSlippageBps, plan.cycleSlippageBps, "slippage");
        assertEq(decoded.minProfit, plan.minProfit, "minProfit");
        assertEq(decoded.steps.length, 1, "steps length");
        assertEq(uint8(decoded.steps[0].op), uint8(plan.steps[0].op), "op");
        assertEq(decoded.steps[0].data, plan.steps[0].data, "data");
    }

    function testContextCarriesPlanV2Version() external {
        MultiVenueArbImplementation.Loan[] memory loans = new MultiVenueArbImplementation.Loan[](1);
        loans[0] = MultiVenueArbImplementation.Loan({
            token: address(0x1),
            amount: 123,
            provider: MultiVenueArbImplementation.LoanProvider.BALANCER,
            providerAddr: address(0)
        });

        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](0);

        MultiVenueArbImplementation.PlanV2 memory plan =
            MultiVenueArbImplementation.PlanV2({loans: loans, cycleSlippageBps: 10, steps: steps, minProfit: 5, declaredResidue: 0, commitment: bytes32(0), chainId: 0, deadline: 0});

        bytes memory ctx = abi.encode(uint8(2), abi.encode(plan));
        (uint8 version, bytes memory payload) = abi.decode(ctx, (uint8, bytes));
        MultiVenueArbImplementation.PlanV2 memory decoded = abi.decode(payload, (MultiVenueArbImplementation.PlanV2));

        assertEq(version, 2, "version");
        assertEq(decoded.loans[0].amount, plan.loans[0].amount, "loan amount");
    }

    function testOpEnumOrderingIsStable() external {
        assertEq(uint8(MultiVenueArbImplementation.Op.UNIV3), 0, "UNIV3");
        assertEq(uint8(MultiVenueArbImplementation.Op.BALANCER), 1, "BALANCER");
        assertEq(uint8(MultiVenueArbImplementation.Op.GENERIC), 2, "GENERIC");
    }

    function testLoanProviderEnumOrderingIsStable() external {
        assertEq(uint8(MultiVenueArbImplementation.LoanProvider.BALANCER), 0, "BALANCER");
        assertEq(uint8(MultiVenueArbImplementation.LoanProvider.AAVE), 1, "AAVE");
        assertEq(uint8(MultiVenueArbImplementation.LoanProvider.ERC3156), 2, "ERC3156");
        assertEq(uint8(MultiVenueArbImplementation.LoanProvider.UNIV2), 3, "UNIV2");
        assertEq(uint8(MultiVenueArbImplementation.LoanProvider.UNIV3), 4, "UNIV3");
    }
}



contract MultiVenueArbExecutorErc3156Test is Test {
    function _deploy(uint256 feeBps)
        internal
        returns (
            MultiVenueArbImplementation executor,
            MockERC3156Lender lender,
            MockERC20 loanToken,
            MockPermit2 permit2,
            BridgeVaultMock vault
        )
    {
        MultiVenueArbImplementation implementation = new MultiVenueArbImplementation();
        ArbitrageCloneFactory factory = new ArbitrageCloneFactory(address(implementation));
        executor = MultiVenueArbImplementation(factory.deployClone(bytes32("erc3156")));

        permit2 = new MockPermit2();
        loanToken = new MockERC20("Loan", "LN", 18);
        lender = new MockERC3156Lender(address(loanToken), feeBps);
        vault = new BridgeVaultMock(executor);

        executor.initialise(address(this), address(vault), address(1), address(0), address(permit2), 0, 0, 1);
    }

    function _deployWithUniV3(uint256 feeBps, address uniV3_)
        internal
        returns (
            MultiVenueArbImplementation executor,
            MockERC3156Lender lender,
            MockERC20 loanToken,
            MockPermit2 permit2,
            BridgeVaultMock vault
        )
    {
        MultiVenueArbImplementation implementation = new MultiVenueArbImplementation();
        ArbitrageCloneFactory factory = new ArbitrageCloneFactory(address(implementation));
        executor = MultiVenueArbImplementation(factory.deployClone(bytes32("erc3156-uni")));

        permit2 = new MockPermit2();
        loanToken = new MockERC20("Loan", "LN", 18);
        lender = new MockERC3156Lender(address(loanToken), feeBps);
        vault = new BridgeVaultMock(executor);

        executor.initialise(address(this), address(vault), uniV3_, address(0), address(permit2), 0, 0, 1);
    }

    function _singleLoanPlan(
        address token,
        uint256 amount,
        address lenderAddr,
        MultiVenueArbImplementation.Step[] memory steps,
        uint256 minProfit
    ) internal pure returns (MultiVenueArbImplementation.PlanV2 memory plan) {
        MultiVenueArbImplementation.Loan[] memory loans = new MultiVenueArbImplementation.Loan[](1);
        loans[0] = MultiVenueArbImplementation.Loan({
            token: token,
            amount: amount,
            provider: MultiVenueArbImplementation.LoanProvider.ERC3156,
            providerAddr: lenderAddr
        });

        plan =
            MultiVenueArbImplementation.PlanV2({loans: loans, cycleSlippageBps: 0, steps: steps, minProfit: minProfit, declaredResidue: 0, commitment: bytes32(0), chainId: 0, deadline: 0});
    }

    /// The adapter id the fixtures register the profit donor under.
    ///
    /// A step names an adapter by ID now, never by address (B-1), so the donor
    /// has to be on the allowlist before a plan can reach it. That is the
    /// point: `_profitStep` carries the id and the caller registers the
    /// address, which is exactly the separation the fix introduces.
    uint16 internal constant DONOR_ADAPTER_ID = 1;

    function _profitStep(address donor, address token, address recipient, uint256 amount)
        internal
        pure
        returns (MultiVenueArbImplementation.Step memory)
    {
        donor; // the address reaches the contract through the registry, not the plan
        bytes memory callData = abi.encodeCall(ProfitDonor.donate, (token, recipient, amount));
        return MultiVenueArbImplementation.Step({
            op: MultiVenueArbImplementation.Op.GENERIC,
            data: abi.encode(DONOR_ADAPTER_ID, address(0), uint256(0), callData)
        });
    }

    function testErc3156LoanExecutesAndRepays() external {
        uint256 feeBps = 50;
        uint256 loanAmount = 100 ether;
        uint256 profit = 2 ether;

        (MultiVenueArbImplementation executor, MockERC3156Lender lender, MockERC20 loanToken,,) = _deploy(feeBps);
        ProfitDonor donor = new ProfitDonor();

        loanToken.mint(address(lender), loanAmount);
        loanToken.mint(address(donor), profit);

        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](1);
        executor.registerAdapter(DONOR_ADAPTER_ID, address(donor));
        executor.allowSelector(DONOR_ADAPTER_ID, ProfitDonor.donate.selector);
        steps[0] = _profitStep(address(donor), address(loanToken), address(executor), profit);

        MultiVenueArbImplementation.PlanV2 memory plan =
            _singleLoanPlan(address(loanToken), loanAmount, address(lender), steps, 1 ether);

        executor.startV2(plan);

        uint256 fee = (loanAmount * feeBps) / 10_000;
        assertEq(loanToken.balanceOf(address(lender)), loanAmount + fee, "lender repaid with fee");
        assertEq(loanToken.balanceOf(address(this)), profit - fee, "owner receives profit net of fee");
    }

    function testErc3156StartV2ReturnsGrossProfit() external {
        uint256 feeBps = 75;
        uint256 loanAmount = 200 ether;
        uint256 profit = 5 ether;

        (MultiVenueArbImplementation executor, MockERC3156Lender lender, MockERC20 loanToken,,) = _deploy(feeBps);
        ProfitDonor donor = new ProfitDonor();

        loanToken.mint(address(lender), loanAmount);
        loanToken.mint(address(donor), profit);

        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](1);
        executor.registerAdapter(DONOR_ADAPTER_ID, address(donor));
        executor.allowSelector(DONOR_ADAPTER_ID, ProfitDonor.donate.selector);
        steps[0] = _profitStep(address(donor), address(loanToken), address(executor), profit);

        MultiVenueArbImplementation.PlanV2 memory plan =
            _singleLoanPlan(address(loanToken), loanAmount, address(lender), steps, 1 ether);

        uint256 returnedGrossProfit = executor.startV2(plan);

        uint256 fee = (loanAmount * feeBps) / 10_000;
        uint256 expectedGrossProfit = profit - fee;
        assertEq(returnedGrossProfit, expectedGrossProfit, "gross profit returned");
    }

    function testErc3156ProfitDeltaExcludesStartingBalance() external {
        uint256 feeBps = 120;
        uint256 loanAmount = 80 ether;
        uint256 profit = 6 ether;
        uint256 dust = 3 ether;

        (MultiVenueArbImplementation executor, MockERC3156Lender lender, MockERC20 loanToken,,) = _deploy(feeBps);
        ProfitDonor donor = new ProfitDonor();

        loanToken.mint(address(executor), dust);
        loanToken.mint(address(lender), loanAmount);
        loanToken.mint(address(donor), profit);

        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](1);
        executor.registerAdapter(DONOR_ADAPTER_ID, address(donor));
        executor.allowSelector(DONOR_ADAPTER_ID, ProfitDonor.donate.selector);
        steps[0] = _profitStep(address(donor), address(loanToken), address(executor), profit);

        MultiVenueArbImplementation.PlanV2 memory plan =
            _singleLoanPlan(address(loanToken), loanAmount, address(lender), steps, 1 ether);

        uint256 returnedGrossProfit = executor.startV2(plan);

        uint256 fee = (loanAmount * feeBps) / 10_000;
        uint256 expectedGrossProfit = profit - fee;

        assertEq(returnedGrossProfit, expectedGrossProfit, "profit delta returned");
        assertEq(loanToken.balanceOf(address(this)), expectedGrossProfit, "owner receives delta only");
        assertEq(loanToken.balanceOf(address(executor)), dust, "dust retained");
    }

    function testFeeAndProfitRecipientsReceiveSplit() external {
        uint256 feeBps = 250; // 2.5%
        uint256 loanAmount = 100 ether;
        uint256 profit = 4 ether;
        address feeRecipient = address(0xFEE);
        address profitRecipient = address(0xBEEF);

        (MultiVenueArbImplementation executor, MockERC3156Lender lender, MockERC20 loanToken,,) = _deploy(feeBps);
        ProfitDonor donor = new ProfitDonor();

        executor.setFeeRecipient(feeRecipient);
        executor.setProfitRecipient(profitRecipient);
        executor.updateConfig(uint16(feeBps), 0, 1);

        loanToken.mint(address(lender), loanAmount);
        loanToken.mint(address(donor), profit);

        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](1);
        executor.registerAdapter(DONOR_ADAPTER_ID, address(donor));
        executor.allowSelector(DONOR_ADAPTER_ID, ProfitDonor.donate.selector);
        steps[0] = _profitStep(address(donor), address(loanToken), address(executor), profit);

        MultiVenueArbImplementation.PlanV2 memory plan =
            _singleLoanPlan(address(loanToken), loanAmount, address(lender), steps, 1 ether);

        executor.startV2(plan);

        uint256 fee = (loanAmount * feeBps) / 10_000;
        uint256 grossProfit = profit - fee;
        uint256 expectedFee = (grossProfit * feeBps) / 10_000;
        uint256 expectedRemainder = grossProfit - expectedFee;

        assertEq(loanToken.balanceOf(feeRecipient), expectedFee, "fee recipient paid");
        assertEq(loanToken.balanceOf(profitRecipient), expectedRemainder, "profit recipient paid");
    }

    function testErc3156RevertsWhenCallbackSenderMismatch() external {
        (MultiVenueArbImplementation executor, MockERC3156Lender lender, MockERC20 loanToken,,) = _deploy(0);
        ERC3156CallbackProxy proxy = new ERC3156CallbackProxy();
        lender.setForwarder(address(proxy), true);

        loanToken.mint(address(lender), 10 ether);

        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](0);
        MultiVenueArbImplementation.PlanV2 memory plan =
            _singleLoanPlan(address(loanToken), 10 ether, address(lender), steps, 0);

        vm.expectRevert(MultiVenueArbImplementation.InvalidProviderAddress.selector);
        executor.startV2(plan);
    }

    function testErc3156RevertsOnTokenMismatch() external {
        (MultiVenueArbImplementation executor, MockERC3156Lender lender, MockERC20 loanToken,,) = _deploy(0);
        lender.setOverrideToken(address(0xBEEF), true);

        loanToken.mint(address(lender), 5 ether);

        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](0);
        MultiVenueArbImplementation.PlanV2 memory plan =
            _singleLoanPlan(address(loanToken), 5 ether, address(lender), steps, 0);

        vm.expectRevert(MultiVenueArbImplementation.InvalidGenericAction.selector);
        executor.startV2(plan);
    }

    function testErc3156RevertsWhenRepayInsufficient() external {
        uint256 feeBps = 500; // 5%
        (MultiVenueArbImplementation executor, MockERC3156Lender lender, MockERC20 loanToken,,) = _deploy(feeBps);

        loanToken.mint(address(lender), 20 ether);

        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](0);
        MultiVenueArbImplementation.PlanV2 memory plan =
            _singleLoanPlan(address(loanToken), 20 ether, address(lender), steps, 0);

        // Assert the SPECIFIC invariant, not merely "something reverted". A bare
        // expectRevert() passes whether the executor rejected this for being
        // insolvent or for malformed calldata — which is exactly how an
        // unprofitable cycle stayed indistinguishable from an encoding bug.
        // With no steps the plan swaps nothing, so it must repay 20 ether plus
        // the 5% fee out of a balance that never grew.
        uint256 repay = 20 ether + (20 ether * feeBps) / 10_000;
        vm.expectRevert(
            abi.encodeWithSelector(
                // `DebtNotRepaid` replaced `InsufficientFinalBalance` when the
                // settlement moved onto `ProfitInvariant`. Same two balances,
                // plus the token -- which is the field that matters the moment
                // a plan can owe more than one asset, because "short by 1e18"
                // does not say short of what.
                DebtNotRepaid.selector,
                address(loanToken),
                20 ether,
                repay
            )
        );
        executor.startV2(plan);
    }




    function testErc3156UniswapStepRevertsOnMalformedPath() external {
        MockUniV3RouterNoop router = new MockUniV3RouterNoop();
        (MultiVenueArbImplementation executor, MockERC3156Lender lender, MockERC20 loanToken,,) =
            _deployWithUniV3(0, address(router));

        loanToken.mint(address(lender), 10 ether);

        bytes memory malformedPath = abi.encodePacked(address(loanToken));
        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](1);
        steps[0] = MultiVenueArbImplementation.Step({
            op: MultiVenueArbImplementation.Op.UNIV3,
            data: abi.encode(malformedPath, 1 ether, uint256(1))
        });
        MultiVenueArbImplementation.PlanV2 memory plan =
            _singleLoanPlan(address(loanToken), 10 ether, address(lender), steps, 0);

        vm.expectRevert(MultiVenueArbImplementation.InvalidGenericAction.selector);
        executor.startV2(plan);
    }

}


contract MultiVenueArbExecutorUniFlashTest is Test {
    function _deploy() internal returns (MultiVenueArbImplementation executor, MockERC20 loanToken, MockPermit2 permit2) {
        MultiVenueArbImplementation implementation = new MultiVenueArbImplementation();
        ArbitrageCloneFactory factory = new ArbitrageCloneFactory(address(implementation));
        executor = MultiVenueArbImplementation(factory.deployClone(bytes32("uni-flash")));

        permit2 = new MockPermit2();
        loanToken = new MockERC20("Loan", "LN", 18);
        executor.initialise(address(this), address(0), address(1), address(0), address(permit2), 0, 0, 1);
    }

    /// The adapter id the fixtures register the profit donor under.
    ///
    /// A step names an adapter by ID now, never by address (B-1), so the donor
    /// has to be on the allowlist before a plan can reach it. That is the
    /// point: `_profitStep` carries the id and the caller registers the
    /// address, which is exactly the separation the fix introduces.
    uint16 internal constant DONOR_ADAPTER_ID = 1;

    function _profitStep(address donor, address token, address recipient, uint256 amount)
        internal
        pure
        returns (MultiVenueArbImplementation.Step memory)
    {
        donor; // the address reaches the contract through the registry, not the plan
        bytes memory callData = abi.encodeCall(ProfitDonor.donate, (token, recipient, amount));
        return MultiVenueArbImplementation.Step({
            op: MultiVenueArbImplementation.Op.GENERIC,
            data: abi.encode(DONOR_ADAPTER_ID, address(0), uint256(0), callData)
        });
    }

    function testUniV2FlashLoanExecutesAndRepays() external {
        uint256 loanAmount = 100 ether;
        uint256 profit = 2 ether;

        (MultiVenueArbImplementation executor, MockERC20 loanToken,) = _deploy();
        ProfitDonor donor = new ProfitDonor();
        MockUniswapV2Pair pair = new MockUniswapV2Pair(address(loanToken), address(new MockERC20("Other", "OT", 18)), 0);

        loanToken.mint(address(pair), loanAmount);
        loanToken.mint(address(donor), profit);

        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](1);
        executor.registerAdapter(DONOR_ADAPTER_ID, address(donor));
        executor.allowSelector(DONOR_ADAPTER_ID, ProfitDonor.donate.selector);
        steps[0] = _profitStep(address(donor), address(loanToken), address(executor), profit);

        MultiVenueArbImplementation.Loan[] memory loans = new MultiVenueArbImplementation.Loan[](1);
        loans[0] = MultiVenueArbImplementation.Loan({
            token: address(loanToken),
            amount: loanAmount,
            provider: MultiVenueArbImplementation.LoanProvider.UNIV2,
            providerAddr: address(pair)
        });

        MultiVenueArbImplementation.PlanV2 memory plan =
            MultiVenueArbImplementation.PlanV2({loans: loans, cycleSlippageBps: 0, steps: steps, minProfit: 1 ether, declaredResidue: 0, commitment: bytes32(0), chainId: 0, deadline: 0});

        executor.startV2(plan);

        uint256 fee = (loanAmount * 30 + (10_000 - 30) - 1) / (10_000 - 30);
        assertEq(loanToken.balanceOf(address(pair)), loanAmount + fee, "pair repaid");
        assertEq(loanToken.balanceOf(address(this)), profit - fee, "owner net profit");
    }


    function testUniV2FlashLoanSupportsCustomPairFeeBps() external {
        uint256 loanAmount = 100 ether;
        uint256 profit = 2 ether;
        uint16 pairFeeBps = 25;

        (MultiVenueArbImplementation executor, MockERC20 loanToken,) = _deploy();
        ProfitDonor donor = new ProfitDonor();
        MockUniswapV2Pair pair = new MockUniswapV2Pair(address(loanToken), address(new MockERC20("Other", "OT", 18)), pairFeeBps);

        loanToken.mint(address(pair), loanAmount);
        loanToken.mint(address(donor), profit);

        executor.setUniswapV2FlashFeeBps(address(pair), pairFeeBps);

        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](1);
        executor.registerAdapter(DONOR_ADAPTER_ID, address(donor));
        executor.allowSelector(DONOR_ADAPTER_ID, ProfitDonor.donate.selector);
        steps[0] = _profitStep(address(donor), address(loanToken), address(executor), profit);

        MultiVenueArbImplementation.Loan[] memory loans = new MultiVenueArbImplementation.Loan[](1);
        loans[0] = MultiVenueArbImplementation.Loan({
            token: address(loanToken),
            amount: loanAmount,
            provider: MultiVenueArbImplementation.LoanProvider.UNIV2,
            providerAddr: address(pair)
        });

        MultiVenueArbImplementation.PlanV2 memory plan =
            MultiVenueArbImplementation.PlanV2({loans: loans, cycleSlippageBps: 0, steps: steps, minProfit: 1 ether, declaredResidue: 0, commitment: bytes32(0), chainId: 0, deadline: 0});

        executor.startV2(plan);

        uint256 fee = (loanAmount * pairFeeBps + (10_000 - pairFeeBps) - 1) / (10_000 - pairFeeBps);
        assertEq(loanToken.balanceOf(address(pair)), loanAmount + fee, "pair repaid");
        assertEq(loanToken.balanceOf(address(this)), profit - fee, "owner net profit");
    }

    function testUniV3FlashLoanExecutesAndRepays() external {
        uint256 feeBps = 5;
        uint256 loanAmount = 100 ether;
        uint256 profit = 2 ether;

        (MultiVenueArbImplementation executor, MockERC20 loanToken,) = _deploy();
        ProfitDonor donor = new ProfitDonor();
        MockUniswapV3FlashPool pool =
            new MockUniswapV3FlashPool(address(loanToken), address(new MockERC20("Other", "OT", 18)), uint16(feeBps));

        loanToken.mint(address(pool), loanAmount);
        loanToken.mint(address(donor), profit);

        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](1);
        executor.registerAdapter(DONOR_ADAPTER_ID, address(donor));
        executor.allowSelector(DONOR_ADAPTER_ID, ProfitDonor.donate.selector);
        steps[0] = _profitStep(address(donor), address(loanToken), address(executor), profit);

        MultiVenueArbImplementation.Loan[] memory loans = new MultiVenueArbImplementation.Loan[](1);
        loans[0] = MultiVenueArbImplementation.Loan({
            token: address(loanToken),
            amount: loanAmount,
            provider: MultiVenueArbImplementation.LoanProvider.UNIV3,
            providerAddr: address(pool)
        });

        MultiVenueArbImplementation.PlanV2 memory plan =
            MultiVenueArbImplementation.PlanV2({loans: loans, cycleSlippageBps: 0, steps: steps, minProfit: 1 ether, declaredResidue: 0, commitment: bytes32(0), chainId: 0, deadline: 0});

        executor.startV2(plan);

        uint256 fee = (loanAmount * feeBps) / 10_000;
        assertEq(loanToken.balanceOf(address(pool)), loanAmount + fee, "pool repaid");
        assertEq(loanToken.balanceOf(address(this)), profit - fee, "owner net profit");
    }
}

contract MultiVenueArbExecutorGuardrailTest is Test {
    bytes4 private constant INVALID_FEE_BPS_SELECTOR = bytes4(keccak256("InvalidFeeBps()"));
    bytes4 private constant INVALID_MAX_SLIPPAGE_SELECTOR = bytes4(keccak256("InvalidMaxSlippage()"));
    bytes4 private constant INVALID_DEADLINE_SELECTOR = bytes4(keccak256("InvalidDeadline()"));

    function _deployExecutor(uint16 maxSlippageBps)
        internal
        returns (MultiVenueArbImplementation executor, MockERC20 token)
    {
        MultiVenueArbImplementation implementation = new MultiVenueArbImplementation();
        ArbitrageCloneFactory factory = new ArbitrageCloneFactory(address(implementation));
        executor = MultiVenueArbImplementation(factory.deployClone(bytes32("guard")));

        MockPermit2 permit2 = new MockPermit2();
        token = new MockERC20("Mock Token", "MCK", 18);
        BridgeVaultMock vault = new BridgeVaultMock(executor);

        executor.initialise(
            address(this), address(vault), address(1), address(0), address(permit2), 0, maxSlippageBps, 1
        );
    }

    function testRevertsWhenCycleSlippageExceedsConfig() external {
        (MultiVenueArbImplementation executor, MockERC20 token) = _deployExecutor(150);

        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](0);
        MultiVenueArbImplementation.PlanLegacy memory plan = MultiVenueArbImplementation.PlanLegacy({
            loanToken: address(token),
            amountIn: 1 ether,
            loanProvider: MultiVenueArbImplementation.LoanProvider.BALANCER,
            cycleSlippageBps: 151,
            steps: steps,
            minProfit: 0
        });

        (bool ok,) = address(executor).call(abi.encodeCall(MultiVenueArbImplementation.start, (plan)));
        assertTrue(!ok, "start should revert when cycle slippage exceeds config cap");
    }

    function testAllowsZeroCycleSlippageAndNoNotionalProfitFloor() external {
        (MultiVenueArbImplementation executor, MockERC20 token) = _deployExecutor(150);

        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](0);
        MultiVenueArbImplementation.PlanLegacy memory plan = MultiVenueArbImplementation.PlanLegacy({
            loanToken: address(token),
            amountIn: 1 ether,
            loanProvider: MultiVenueArbImplementation.LoanProvider.BALANCER,
            cycleSlippageBps: 0,
            steps: steps,
            minProfit: 0
        });

        vm.expectRevert();
        executor.start(plan);
    }

    function testInitialiseRevertsOnInvalidConfigBounds() external {
        MultiVenueArbImplementation implementation = new MultiVenueArbImplementation();
        ArbitrageCloneFactory factory = new ArbitrageCloneFactory(address(implementation));
        MultiVenueArbImplementation executor = MultiVenueArbImplementation(factory.deployClone(bytes32("guard-config")));
        MockPermit2 permit2 = new MockPermit2();

        vm.expectRevert(INVALID_FEE_BPS_SELECTOR);
        executor.initialise(address(this), address(0), address(0), address(0), address(permit2), 10_001, 0, 1);

        vm.expectRevert(INVALID_MAX_SLIPPAGE_SELECTOR);
        executor.initialise(address(this), address(0), address(0), address(0), address(permit2), 0, 10_001, 1);

        vm.expectRevert(INVALID_DEADLINE_SELECTOR);
        executor.initialise(address(this), address(0), address(0), address(0), address(permit2), 0, 0, 3_601);
    }

    function testUpdateConfigRevertsOnInvalidConfigBounds() external {
        (MultiVenueArbImplementation executor,) = _deployExecutor(100);

        vm.expectRevert(INVALID_FEE_BPS_SELECTOR);
        executor.updateConfig(10_001, 100, 60);

        vm.expectRevert(INVALID_MAX_SLIPPAGE_SELECTOR);
        executor.updateConfig(0, 10_001, 60);

        vm.expectRevert(INVALID_DEADLINE_SELECTOR);
        executor.updateConfig(0, 100, 3_601);
    }

}

contract MultiVenueArbExecutorAccessControlTest is Test {
    function _deploy(address owner_, address vault_, address aavePool_)
        internal
        returns (MultiVenueArbImplementation executor, MockPermit2 permit2)
    {
        MultiVenueArbImplementation implementation = new MultiVenueArbImplementation();
        ArbitrageCloneFactory factory = new ArbitrageCloneFactory(address(implementation));
        executor = MultiVenueArbImplementation(factory.deployClone(bytes32("access")));
        permit2 = new MockPermit2();

        executor.initialise(owner_, vault_, address(1), aavePool_, address(permit2), 0, 100, 1);
    }

    function _plan(address token) internal pure returns (MultiVenueArbImplementation.PlanLegacy memory plan) {
        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](0);
        plan = MultiVenueArbImplementation.PlanLegacy({
            loanToken: token,
            amountIn: 1 ether,
            loanProvider: MultiVenueArbImplementation.LoanProvider.BALANCER,
            cycleSlippageBps: 100,
            steps: steps,
            minProfit: (1 ether * 100) / 10_000
        });
    }

    function testStartRestrictedToOwner() external {
        (MultiVenueArbImplementation executor,) = _deploy(address(this), address(11), address(0));
        NonOwnerStarter caller = new NonOwnerStarter();

        MultiVenueArbImplementation.PlanLegacy memory plan = _plan(address(22));
        bool ok = caller.callStart(address(executor), plan);

        assertTrue(!ok, "non-owner should not be allowed to start");
    }

    function testStartRequiresExecutorRole() external {
        (MultiVenueArbImplementation executor,) = _deploy(address(this), address(11), address(0));
        executor.setExecutor(address(this), false);

        MultiVenueArbImplementation.PlanLegacy memory plan = _plan(address(22));
        vm.expectRevert(NotExecutor.selector);
        executor.start(plan);
    }

    function testUpdateConfigRequiresConfigAdminRole() external {
        (MultiVenueArbImplementation executor,) = _deploy(address(this), address(11), address(0));
        NonOwnerConfigurator caller = new NonOwnerConfigurator();

        bool ok = caller.callUpdateConfig(address(executor), 0, 0, 1);
        assertTrue(!ok, "non-config-admin should not be allowed to update config");
    }

    function testBatchRouterOwnerGating() external {
        (MultiVenueArbImplementation executor,) = _deploy(address(this), address(11), address(0));
        BatchRouter router = new BatchRouter(address(executor));
        NonOwnerBatchRouterCaller caller = new NonOwnerBatchRouterCaller();

        MultiVenueArbImplementation.PlanLegacy memory planLegacy = _plan(address(22));

        MultiVenueArbImplementation.Loan[] memory loans = new MultiVenueArbImplementation.Loan[](0);
        MultiVenueArbImplementation.Step[] memory steps = new MultiVenueArbImplementation.Step[](0);
        MultiVenueArbImplementation.PlanV2 memory planV2 =
            MultiVenueArbImplementation.PlanV2({loans: loans, cycleSlippageBps: 0, steps: steps, minProfit: 0, declaredResidue: 0, commitment: bytes32(0), chainId: 0, deadline: 0});

        bool okLegacy = caller.callStart(address(router), planLegacy);
        bool okV2 = caller.callStartV2(address(router), planV2);

        address[] memory targets = new address[](1);
        bytes[] memory data = new bytes[](1);
        targets[0] = address(executor);
        data[0] = abi.encodeWithSignature("owner()");

        bool okMulti = caller.callMulticall(address(router), targets, data);

        assertTrue(!okLegacy, "non-owner should not be allowed to start via router");
        assertTrue(!okV2, "non-owner should not be allowed to startV2 via router");
        assertTrue(!okMulti, "non-owner should not be allowed to multicall");
    }

    function testBatchRouterTargetAllowlist() external {
        (MultiVenueArbImplementation executor,) = _deploy(address(this), address(11), address(0));
        BatchRouter router = new BatchRouter(address(executor));
        BatchRouterTargetMock target = new BatchRouterTargetMock();

        address[] memory targets = new address[](1);
        bytes[] memory data = new bytes[](1);
        targets[0] = address(target);
        data[0] = abi.encodeCall(BatchRouterTargetMock.ping, ());

        (bool ok,) = address(router).call(abi.encodeCall(BatchRouter.multicall, (targets, data)));
        assertTrue(!ok, "router should reject non-allowlisted target");

        router.setTargetAllowed(address(target), true);

        (ok,) = address(router).call(abi.encodeCall(BatchRouter.multicall, (targets, data)));
        assertTrue(ok, "router should allow allowlisted target");
        assertEq(target.calls(), 1, "target should be called once");
    }

    function testReceiveFlashLoanOnlyVaultMayCall() external {
        (MultiVenueArbImplementation executor,) = _deploy(address(this), address(55), address(0));

        address[] memory tokens = new address[](1);
        tokens[0] = address(99);
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = 1 ether;
        uint256[] memory fees = new uint256[](1);
        fees[0] = 0;

        (bool ok,) = address(executor)
            .call(abi.encodeCall(MultiVenueArbImplementation.receiveFlashLoan, (tokens, amounts, fees, bytes(""))));

        assertTrue(!ok, "callback should revert when caller is not vault");
    }

    function testExecuteOperationRequiresAavePoolCaller() external {
        (MultiVenueArbImplementation executor,) = _deploy(address(this), address(44), address(0xBEEF));

        (bool ok,) = address(executor)
            .call(
                abi.encodeCall(
                    MultiVenueArbImplementation.executeOperation, (address(1), 1 ether, 0, address(executor), bytes(""))
                )
            );

        assertTrue(!ok, "executeOperation should revert when caller is not pool");
    }

    function testExecuteOperationRequiresExecutorInitiator() external {
        (MultiVenueArbImplementation executor,) = _deploy(address(this), address(77), address(this));

        (bool ok,) = address(executor)
            .call(
                abi.encodeCall(
                    MultiVenueArbImplementation.executeOperation, (address(1), 1 ether, 0, address(0xCAFE), bytes(""))
                )
            );

        assertTrue(!ok, "executeOperation should revert when initiator mismatches");
    }
}

contract MultiVenueArbExecutorCircuitBreakerTest is Test {
    function _deployExecutor() internal returns (MultiVenueArbImplementation executor) {
        MultiVenueArbImplementation implementation = new MultiVenueArbImplementation();
        ArbitrageCloneFactory factory = new ArbitrageCloneFactory(address(implementation));
        executor = MultiVenueArbImplementation(factory.deployClone(bytes32("circuit")));

        MockPermit2 permit2 = new MockPermit2();
        executor.initialise(address(this), address(0), address(0), address(0), address(permit2), 0, 0, 1);
    }

    function testCircuitBreakerOnlyUsesManualTripCooldownWindow() external {
        MultiVenueArbImplementation executor = _deployExecutor();
        executor.setCircuitCooldown(1 hours);

        assertTrue(!executor.isCircuitOpen(), "circuit should be closed before a manual trip");
        executor.tripCircuit();
        assertTrue(executor.isCircuitOpen(), "manual trip should open the circuit during cooldown");

        vm.warp(block.timestamp + 2 hours);
        assertTrue(!executor.isCircuitOpen(), "circuit should auto-close after cooldown");
    }

    function testResetCircuitClearsManualTrip() external {
        MultiVenueArbImplementation executor = _deployExecutor();

        executor.tripCircuit();
        assertTrue(executor.isCircuitOpen(), "trip should open circuit");

        executor.resetCircuit();
        assertTrue(!executor.isCircuitOpen(), "reset should close circuit");
    }

    function testTransferOwnershipUpdatesOwner() external {
        MultiVenueArbImplementation executor = _deployExecutor();
        address newOwner = address(0xBEEF);

        executor.transferOwnership(newOwner);

        assertEq(executor.owner(), newOwner, "owner should update");
    }

    function testTransferOwnershipRevertsForZeroAddress() external {
        MultiVenueArbImplementation executor = _deployExecutor();

        vm.expectRevert(abi.encodeWithSignature("InvalidOwner()"));
        executor.transferOwnership(address(0));
    }
}
