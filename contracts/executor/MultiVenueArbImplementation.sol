// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {SwapExecutor} from "./steps/SwapExecutor.sol";
import {GenericExecutor} from "./steps/GenericExecutor.sol";
import {FullMath} from "../libraries/FullMath.sol";
import {TickMath} from "../libraries/TickMath.sol";
import {LiquidityAmounts} from "../libraries/LiquidityAmounts.sol";
import {ReentrancyGuard} from "../utils/ReentrancyGuard.sol";

error InvalidImplementation();
error InvalidBatch();
error InvalidExecutor();
error InvalidOwner();
error InvalidTarget();
error InvalidFeeBps();
error InvalidMaxSlippage();
error InvalidDeadline();
error NotOwner();
error NotExecutor();
error NotConfigAdmin();
error InitFailed();
error InvalidRoleAccount();
error InvalidCanonicalProfitToken();

interface IBalancerVault {
    enum SwapKind {
        GIVEN_IN,
        GIVEN_OUT
    }

    struct BatchSwapStep {
        bytes32 poolId;
        uint256 assetInIndex;
        uint256 assetOutIndex;
        uint256 amount;
        bytes userData;
    }

    struct FundManagement {
        address sender;
        bool fromInternalBalance;
        address recipient;
        bool toInternalBalance;
    }

    function flashLoan(address recipient, address[] memory tokens, uint256[] memory amounts, bytes memory userData)
        external;

    function batchSwap(
        SwapKind kind,
        BatchSwapStep[] calldata swaps,
        address[] calldata assets,
        FundManagement calldata funds,
        int256[] calldata limits,
        uint256 deadline
    ) external returns (int256[] memory assetDeltas);
}

interface IERC20 {
    function approve(address, uint256) external returns (bool);
    function balanceOf(address) external view returns (uint256);
    function transfer(address, uint256) external returns (bool);
    function allowance(address, address) external view returns (uint256);
}

interface IPermit2 {
    function approve(address token, address spender, uint160 amount, uint48 expiration) external;

    function allowance(address owner, address token, address spender)
        external
        view
        returns (uint160 amount, uint48 expiration, uint48 nonce);

    function transferFrom(address from, address to, uint160 amount, address token) external payable;
}

interface ISwapRouter02 {
    struct ExactInputParams {
        bytes path;
        address recipient;
        uint256 amountIn;
        uint256 amountOutMinimum;
    }

    function exactInput(ExactInputParams calldata) external payable returns (uint256);
}

interface IUniswapV3Pool {
    function token0() external view returns (address);
    function token1() external view returns (address);
    function fee() external view returns (uint24);
    function tickSpacing() external view returns (int24);
    function slot0() external view returns (uint160 sqrtPriceX96, int24 tick, uint16, uint16, uint16, uint8, bool);
    function flash(address recipient, uint256 amount0, uint256 amount1, bytes calldata data) external;
    function mint(address recipient, int24 tickLower, int24 tickUpper, uint128 amount, bytes calldata data)
        external
        returns (uint256 amount0, uint256 amount1);
    function burn(int24 tickLower, int24 tickUpper, uint128 amount) external returns (uint256 amount0, uint256 amount1);
    function collect(
        address recipient,
        int24 tickLower,
        int24 tickUpper,
        uint128 amount0Requested,
        uint128 amount1Requested
    ) external returns (uint256 amount0, uint256 amount1);
}

interface IUniswapV2Pair {
    function token0() external view returns (address);
    function token1() external view returns (address);
    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external;
}

interface IAaveV3Pool {
    function flashLoanSimple(
        address receiverAddress,
        address asset,
        uint256 amount,
        bytes calldata params,
        uint16 referralCode
    ) external;
}

interface IAaveFlashLoanSimpleReceiver {
    function executeOperation(address asset, uint256 amount, uint256 premium, address initiator, bytes calldata params)
        external
        returns (bool);
}

interface IERC3156FlashBorrower {
    function onFlashLoan(address initiator, address token, uint256 amount, uint256 fee, bytes calldata data)
        external
        returns (bytes32);
}

interface IERC3156FlashLender {
    function flashLoan(IERC3156FlashBorrower receiver, address token, uint256 amount, bytes calldata data)
        external
        returns (bool);
}

library SafeCall {
    function safeCall(address target, bytes memory data) internal returns (bytes memory) {
        (bool ok, bytes memory ret) = target.call(data);
        if (!ok) {
            if (ret.length == 0) {
                revert();
            }
            assembly {
                revert(add(ret, 0x20), mload(ret))
            }
        }
        return ret;
    }

    function safeCallOptionalReturnBool(address target, bytes memory data) internal {
        bytes memory ret = safeCall(target, data);
        if (ret.length == 0) {
            return;
        }
        if (ret.length < 32 || !abi.decode(ret, (bool))) {
            revert();
        }
    }
}

struct PackedConfig {
    // layout: [feeBps | maxSlippageBps | deadlineBuffer]
    uint64 raw;
}

library ConfigCodec {
    function pack(uint16 feeBps, uint16 maxSlippageBps, uint32 deadlineBuffer) internal pure returns (uint64) {
        return (uint64(feeBps) << 48) | (uint64(maxSlippageBps) << 32) | uint64(deadlineBuffer);
    }

    function unpack(uint64 raw) internal pure returns (uint16 feeBps, uint16 maxSlippageBps, uint32 deadlineBuffer) {
        feeBps = uint16(raw >> 48);
        maxSlippageBps = uint16((raw >> 32) & 0xFFFF);
        deadlineBuffer = uint32(raw);
    }
}

contract MultiVenueArbImplementation is IAaveFlashLoanSimpleReceiver, IERC3156FlashBorrower, ReentrancyGuard {
    using SafeCall for address;

    uint8 private constant PLAN_VERSION_V2 = 2;

    /// §1.4 excludes JIT liquidity and cross-chain bridging, so `BRIDGE`,
    /// `JIT_LP_ADD` and `JIT_LP_REMOVE` are gone. Removing enum variants
    /// renumbers nothing here -- they were the last three -- so an old plan
    /// naming one of them now decodes as an out-of-range `Op` and reverts,
    /// which is the outcome wanted.
    enum Op {
        UNIV3,
        BALANCER,
        GENERIC
    }

    enum LoanProvider {
        BALANCER,
        AAVE,
        ERC3156,
        UNIV2,
        UNIV3
    }

    struct Step {
        Op op;
        bytes data;
    }

    struct Loan {
        address token;
        uint256 amount;
        LoanProvider provider;
        address providerAddr;
    }

    struct TokenApproval {
        address token;
        address spender;
        uint256 amount;
    }

    struct PlanLegacy {
        address loanToken;
        uint256 amountIn;
        LoanProvider loanProvider;
        uint16 cycleSlippageBps;
        Step[] steps;
        uint256 minProfit;
    }

    struct PlanV2 {
        Loan[] loans;
        uint16 cycleSlippageBps;
        Step[] steps;
        uint256 minProfit;
    }

    struct ActiveLoanContext {
        bytes32 ctxHash;
        address lender;
        address token;
        uint256 amount;
        LoanProvider provider;
    }


    address public owner;
    IBalancerVault public vault;
    ISwapRouter02 public uniV3;
    IPermit2 public permit2;
    IAaveV3Pool public aavePool;

    PackedConfig internal config;
    bool private initialised;

    ActiveLoanContext private activeLoan;
    // Profit returned by start/startV2 is computed as delta vs the pre-loan balance
    // to prevent counting/consuming pre-funded balances (dust) as trade profit.
    uint256 private lastGrossProfit;
    address private activeStartToken;
    uint256 private activeStartBalance;
    address public feeRecipient;
    address public profitRecipient;
    address public canonicalProfitToken;

    mapping(address => bool) public executors;
    mapping(address => bool) public configAdmins;
    mapping(address => uint16) public uniswapV2FlashFeeBps;

    address private immutable swapExecutorModule;
    address private immutable genericExecutorModule;

    uint256 private constant BPS = 10_000;
    uint32 private constant MAX_DEADLINE_BUFFER = 3600;
    uint48 private constant PERMIT2_MAX_EXPIRATION = type(uint48).max;
    bytes32 private constant ERC3156_CALLBACK_SUCCESS = keccak256("ERC3156FlashBorrower.onFlashLoan");

    constructor() {
        swapExecutorModule = address(new SwapExecutor());
        genericExecutorModule = address(new GenericExecutor());
        initialised = true;
    }

    event ConfigUpdated(uint16 feeBps, uint16 maxSlippageBps, uint32 deadlineBuffer);
    event ProfitRealised(address indexed token, uint256 grossProfit, uint256 ownerFee);
    event TokenApprovalSet(address indexed token, address indexed spender, uint256 amount);
    event ExecutorUpdated(address indexed account, bool allowed);
    event ConfigAdminUpdated(address indexed account, bool allowed);
    event CanonicalProfitTokenUpdated(address indexed token);
    event UniswapV2FlashFeeBpsUpdated(address indexed pair, uint16 feeBps);
    event CircuitTripped(uint256 timestamp);
    event CircuitReset(uint256 timestamp);

    error AlreadyInitialised();
    error InvalidVault();
    error InvalidRouter();
    error InvalidAavePool();
    error InvalidGenericAction();
    /// The cycle ended holding LESS of the start token than it began with.
    ///
    /// Previously this reverted as `InvalidGenericAction()`, which the contract
    /// raises at ~40 unrelated sites — so a plan that simply lost money was
    /// indistinguishable from malformed calldata, a bad adapter, or a failed
    /// approval. Carries both balances so the shortfall is readable directly
    /// from the revert instead of being inferred.
    error InsufficientFinalBalance(uint256 finalBalance, uint256 requiredBalance);
    error InvalidPermit2();
    error Permit2AmountOverflow();
    error CircuitOpen();
    error InvalidLoanCount();
    error InvalidProviderAddress();
    error EmptyRevertData();
    error UnsupportedPlanVersion();

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    modifier onlyExecutor() {
        if (!executors[msg.sender]) revert NotExecutor();
        _;
    }

    modifier onlyConfigAdmin() {
        if (!configAdmins[msg.sender]) revert NotConfigAdmin();
        _;
    }

    // Circuit-breaker is manual-only and cooldown-based.
    uint256 public lastCircuitTrip;
    uint256 public circuitCooldown = 30 minutes;

    function setCircuitCooldown(uint256 _cooldown) external onlyConfigAdmin {
        circuitCooldown = _cooldown;
    }

    function isCircuitOpen() public view returns (bool) {
        if (lastCircuitTrip != 0 && block.timestamp < lastCircuitTrip + circuitCooldown) return true;
        return false;
    }

    function tripCircuit() external onlyConfigAdmin {
        lastCircuitTrip = block.timestamp;
        emit CircuitTripped(block.timestamp);
    }

    function resetCircuit() external onlyConfigAdmin {
        lastCircuitTrip = 0;
        emit CircuitReset(block.timestamp);
    }

    function initialise(
        address _owner,
        address _vault,
        address _uni,
        address _aavePool,
        address _permit2,
        uint16 feeBps,
        uint16 maxSlippageBps,
        uint32 deadlineBuffer
    ) external {
        if (initialised) revert AlreadyInitialised();
        if (_owner == address(0)) revert();
        _validateConfig(feeBps, maxSlippageBps, deadlineBuffer);
        owner = _owner;
        executors[_owner] = true;
        configAdmins[_owner] = true;
        if (_vault != address(0)) {
            vault = IBalancerVault(_vault);
        }
        if (_uni != address(0)) {
            uniV3 = ISwapRouter02(_uni);
        }
        if (_permit2 != address(0)) {
            permit2 = IPermit2(_permit2);
        }
        if (_aavePool != address(0)) {
            aavePool = IAaveV3Pool(_aavePool);
        }
        feeRecipient = _owner;
        profitRecipient = _owner;
        canonicalProfitToken = address(0);
        config.raw = ConfigCodec.pack(feeBps, maxSlippageBps, deadlineBuffer);
        circuitCooldown = 30 minutes;
        lastCircuitTrip = 0;
        initialised = true;
        emit ConfigUpdated(feeBps, maxSlippageBps, deadlineBuffer);
    }

    function updateConfig(uint16 feeBps, uint16 maxSlippageBps, uint32 deadlineBuffer) external onlyConfigAdmin {
        _validateConfig(feeBps, maxSlippageBps, deadlineBuffer);
        config.raw = ConfigCodec.pack(feeBps, maxSlippageBps, deadlineBuffer);
        emit ConfigUpdated(feeBps, maxSlippageBps, deadlineBuffer);
    }

    function setFeeRecipient(address recipient) external onlyConfigAdmin {
        if (recipient == address(0)) revert();
        feeRecipient = recipient;
    }

    function setProfitRecipient(address recipient) external onlyConfigAdmin {
        if (recipient == address(0)) revert();
        profitRecipient = recipient;
    }

    function setCanonicalProfitToken(address token) external onlyConfigAdmin {
        if (token == address(0)) revert InvalidCanonicalProfitToken();
        canonicalProfitToken = token;
        emit CanonicalProfitTokenUpdated(token);
    }

    function clearCanonicalProfitToken() external onlyConfigAdmin {
        canonicalProfitToken = address(0);
        emit CanonicalProfitTokenUpdated(address(0));
    }

    function setUniswapV2FlashFeeBps(address pair, uint16 feeBps) external onlyConfigAdmin {
        if (pair == address(0) || feeBps >= BPS) revert InvalidFeeBps();
        uniswapV2FlashFeeBps[pair] = feeBps;
        emit UniswapV2FlashFeeBpsUpdated(pair, feeBps);
    }

    function setExecutor(address account, bool allowed) external onlyOwner {
        if (account == address(0)) revert InvalidRoleAccount();
        executors[account] = allowed;
        emit ExecutorUpdated(account, allowed);
    }

    function setConfigAdmin(address account, bool allowed) external onlyOwner {
        if (account == address(0)) revert InvalidRoleAccount();
        configAdmins[account] = allowed;
        emit ConfigAdminUpdated(account, allowed);
    }

    function transferOwnership(address newOwner) external onlyOwner {
        if (newOwner == address(0)) revert InvalidOwner();
        owner = newOwner;
    }

    function getConfig() external view returns (uint16 feeBps, uint16 maxSlippageBps, uint32 deadlineBuffer) {
        return ConfigCodec.unpack(config.raw);
    }

    function setTokenApprovals(TokenApproval[] calldata approvals) external onlyConfigAdmin {
        uint256 len = approvals.length;
        for (uint256 i; i < len;) {
            TokenApproval calldata approval = approvals[i];
            if (approval.token == address(0) || approval.spender == address(0)) revert();
            _setPermit2Allowance(approval.token, approval.spender, approval.amount);
            emit TokenApprovalSet(approval.token, approval.spender, approval.amount);
            unchecked {
                ++i;
            }
        }
    }

    function start(PlanLegacy calldata p) external onlyExecutor nonReentrant returns (uint256 grossProfit) {
        _validatePlanGuards(p.cycleSlippageBps);
        _validateLegacyProvider(p.loanProvider);
        lastGrossProfit = 0;
        _initiateLoanV2(_legacyToV2(p));
        return lastGrossProfit;
    }

    function startLegacy(PlanLegacy calldata p) external onlyExecutor nonReentrant {
        _validatePlanGuards(p.cycleSlippageBps);
        _validateLegacyProvider(p.loanProvider);
        _initiateLoanV2(_legacyToV2(p));
    }

    function startV2(PlanV2 calldata p) external onlyExecutor nonReentrant returns (uint256 grossProfit) {
        _validatePlanGuards(p.cycleSlippageBps);
        lastGrossProfit = 0;
        _initiateLoanV2(p);
        return lastGrossProfit;
    }

    function _providerAddress(LoanProvider provider) internal view returns (address) {
        if (provider == LoanProvider.BALANCER) {
            return address(vault);
        }
        if (provider == LoanProvider.AAVE) {
            return address(aavePool);
        }
        return address(0);
    }

    function _encodeContext(uint8 version, bytes memory payload) private pure returns (bytes memory ctx) {
        ctx = abi.encode(version, payload);
    }

    function _setActiveStartBalance(address token) private {
        activeStartToken = token;
        activeStartBalance = IERC20(token).balanceOf(address(this));
    }

    function _getActiveStartBalance(address token) private view returns (uint256) {
        if (activeStartToken != token) revert InvalidGenericAction();
        return activeStartBalance;
    }

    function _clearActiveStartBalance() private {
        activeStartToken = address(0);
        activeStartBalance = 0;
    }

    function _initiateLoanV2(PlanV2 memory p) internal {
        if (p.loans.length != 1) revert InvalidLoanCount();
        Loan memory loan = p.loans[0];
        _setActiveStartBalance(loan.token);
        bytes memory ctx = _encodeContext(PLAN_VERSION_V2, abi.encode(p));
        if (loan.provider == LoanProvider.BALANCER) {
            address vaultAddr = _providerAddress(loan.provider);
            if (vaultAddr == address(0)) revert InvalidVault();
            address[] memory tokens = new address[](1);
            tokens[0] = loan.token;
            uint256[] memory amts = new uint256[](1);
            amts[0] = loan.amount;
            IBalancerVault(vaultAddr).flashLoan(address(this), tokens, amts, ctx);
        } else if (loan.provider == LoanProvider.AAVE) {
            address poolAddr = _providerAddress(loan.provider);
            if (poolAddr == address(0)) revert InvalidAavePool();
            IAaveV3Pool(poolAddr).flashLoanSimple(address(this), loan.token, loan.amount, ctx, 0);
        } else if (loan.provider == LoanProvider.ERC3156) {
            if (loan.providerAddr == address(0)) revert InvalidProviderAddress();
            if (activeLoan.ctxHash != bytes32(0)) revert InvalidGenericAction();
            activeLoan = ActiveLoanContext({
                ctxHash: keccak256(ctx),
                lender: loan.providerAddr,
                token: loan.token,
                amount: loan.amount,
                provider: loan.provider
            });
            bool ok = IERC3156FlashLender(loan.providerAddr).flashLoan(this, loan.token, loan.amount, ctx);
            if (!ok) revert InvalidGenericAction();
        } else if (loan.provider == LoanProvider.UNIV2) {
            if (loan.providerAddr == address(0)) revert InvalidProviderAddress();
            if (activeLoan.ctxHash != bytes32(0)) revert InvalidGenericAction();

            IUniswapV2Pair pair = IUniswapV2Pair(loan.providerAddr);
            uint256 amount0Out;
            uint256 amount1Out;
            if (pair.token0() == loan.token) {
                amount0Out = loan.amount;
            } else if (pair.token1() == loan.token) {
                amount1Out = loan.amount;
            } else {
                revert InvalidGenericAction();
            }

            activeLoan = ActiveLoanContext({
                ctxHash: keccak256(ctx),
                lender: loan.providerAddr,
                token: loan.token,
                amount: loan.amount,
                provider: loan.provider
            });
            pair.swap(amount0Out, amount1Out, address(this), ctx);
        } else if (loan.provider == LoanProvider.UNIV3) {
            if (loan.providerAddr == address(0)) revert InvalidProviderAddress();
            if (activeLoan.ctxHash != bytes32(0)) revert InvalidGenericAction();

            IUniswapV3Pool pool = IUniswapV3Pool(loan.providerAddr);
            uint256 amount0;
            uint256 amount1;
            if (pool.token0() == loan.token) {
                amount0 = loan.amount;
            } else if (pool.token1() == loan.token) {
                amount1 = loan.amount;
            } else {
                revert InvalidGenericAction();
            }

            activeLoan = ActiveLoanContext({
                ctxHash: keccak256(ctx),
                lender: loan.providerAddr,
                token: loan.token,
                amount: loan.amount,
                provider: loan.provider
            });
            pool.flash(address(this), amount0, amount1, ctx);
        } else {
            revert InvalidGenericAction();
        }
    }

    function receiveFlashLoan(
        address[] memory tokens,
        uint256[] memory amounts,
        uint256[] memory fees,
        bytes memory userData
    ) external {
        (uint8 version, bytes memory payload) = abi.decode(userData, (uint8, bytes));
        address expectedVault = _providerAddress(LoanProvider.BALANCER);
        if (expectedVault != msg.sender) revert InvalidVault();
        if (tokens.length != 1 || amounts.length != 1 || fees.length != 1) revert InvalidGenericAction();
        _executeVersionedFlashCallback(version, payload, tokens[0], amounts[0], fees[0], msg.sender);
    }

    function executeOperation(address asset, uint256 amount, uint256 premium, address initiator, bytes calldata params)
        external
        override
        returns (bool)
    {
        (uint8 version, bytes memory payload) = abi.decode(params, (uint8, bytes));
        address expectedPool = _providerAddress(LoanProvider.AAVE);
        if (expectedPool != msg.sender) revert InvalidAavePool();
        if (initiator != address(this)) revert InvalidGenericAction();
        _executeVersionedFlashCallback(version, payload, asset, amount, premium, msg.sender);
        return true;
    }

    function onFlashLoan(address initiator, address token, uint256 amount, uint256 fee, bytes calldata data)
        external
        returns (bytes32)
    {
        (uint8 version, bytes memory payload) = abi.decode(data, (uint8, bytes));
        address lender = _validateActiveLoanAndSender(LoanProvider.ERC3156, data);
        ActiveLoanContext memory ctx = activeLoan;
        if (ctx.token != token || ctx.amount != amount) revert InvalidGenericAction();
        if (initiator != address(this)) revert InvalidGenericAction();
        _executeVersionedFlashCallback(version, payload, token, amount, fee, lender);
        delete activeLoan;
        return ERC3156_CALLBACK_SUCCESS;
    }

    function uniswapV2Call(address sender, uint256 amount0, uint256 amount1, bytes calldata data) external {
        (uint8 version, bytes memory payload) = abi.decode(data, (uint8, bytes));
        _validateActiveLoanAndSender(LoanProvider.UNIV2, data);
        ActiveLoanContext memory ctx = activeLoan;
        if (sender != address(this)) revert InvalidGenericAction();

        uint256 amount = amount0 > 0 ? amount0 : amount1;
        bool hasAmount0 = amount0 > 0;
        bool hasAmount1 = amount1 > 0;
        if (amount == 0 || (hasAmount0 && hasAmount1) || (!hasAmount0 && !hasAmount1)) revert InvalidGenericAction();
        if (ctx.amount != amount) revert InvalidGenericAction();

        uint256 fee = _computeUniswapV2FlashFee(msg.sender, amount);
        _executeVersionedFlashCallback(version, payload, ctx.token, amount, fee, msg.sender);
        delete activeLoan;
    }

    function uniswapV3FlashCallback(uint256 fee0, uint256 fee1, bytes calldata data) external {
        (uint8 version, bytes memory payload) = abi.decode(data, (uint8, bytes));
        _validateActiveLoanAndSender(LoanProvider.UNIV3, data);
        ActiveLoanContext memory ctx = activeLoan;

        IUniswapV3Pool pool = IUniswapV3Pool(msg.sender);
        uint256 fee;
        if (pool.token0() == ctx.token) {
            fee = fee0;
            if (fee1 != 0) revert InvalidGenericAction();
        } else if (pool.token1() == ctx.token) {
            fee = fee1;
            if (fee0 != 0) revert InvalidGenericAction();
        } else {
            revert InvalidGenericAction();
        }

        _executeVersionedFlashCallback(version, payload, ctx.token, ctx.amount, fee, msg.sender);
        delete activeLoan;
    }

    function _executeSteps(Step[] calldata steps, uint256 deadline, address self, address vaultAddr) external {
        if (msg.sender != address(this)) revert InvalidGenericAction();

        uint256 len = steps.length;
        for (uint256 i; i < len;) {
            Step calldata s = steps[i];
            bytes memory callData;
            address module;
            if (s.op == Op.UNIV3 || s.op == Op.BALANCER) {
                module = swapExecutorModule;
                callData = abi.encodeWithSelector(SwapExecutor.execute.selector, uint8(s.op), s.data, deadline, self, vaultAddr, address(uniV3));
            } else if (s.op == Op.GENERIC) {
                module = genericExecutorModule;
                callData = abi.encodeWithSelector(GenericExecutor.execute.selector, s.data);
            } else {
                revert InvalidGenericAction();
            }
            (bool ok, bytes memory ret) = module.delegatecall(callData);
            if (!ok) {
                if (ret.length == 0) revert InvalidGenericAction();
                assembly { revert(add(ret, 0x20), mload(ret)) }
            }
            unchecked { ++i; }
        }
    }

    function moduleExecSwap(uint8 op, bytes memory data, uint256 deadline, address recipient, address vaultAddr, address) external {
        if (msg.sender != address(this)) revert InvalidGenericAction();
        if (op == uint8(Op.UNIV3)) {
            _execUniswap(data, deadline, recipient);
        } else if (op == uint8(Op.BALANCER)) {
            _execBalancer(data, deadline, vaultAddr);
        } else {
            revert InvalidGenericAction();
        }
    }

    function moduleExecGeneric(bytes memory data) external {
        if (msg.sender != address(this)) revert InvalidGenericAction();
        _execGeneric(data);
    }



    function _execUniswap(bytes memory data, uint256 deadline, address recipient) internal {
        if (address(uniV3) == address(0)) revert InvalidRouter();
        (bytes memory path, uint256 amountIn, uint256 minOut) = abi.decode(data, (bytes, uint256, uint256));
        if (minOut == 0) revert InvalidGenericAction();
        address tokenIn = _tokenAt(path, 0);
        _ensureAllowance(tokenIn, address(uniV3), amountIn);
        ISwapRouter02.ExactInputParams memory ep = ISwapRouter02.ExactInputParams({
            path: path, recipient: recipient, amountIn: amountIn, amountOutMinimum: minOut
        });
        uniV3.exactInput(ep);
    }

    function _execBalancer(bytes memory data, uint256 deadline, address vaultAddr) internal {
        if (vaultAddr == address(0)) revert InvalidVault();
        (bytes32 poolId, address tokenIn, address tokenOut, uint256 amountIn, uint256 minOut) =
            abi.decode(data, (bytes32, address, address, uint256, uint256));
        if (minOut == 0 || minOut > uint256(type(int256).max)) revert();
        address[] memory assets = new address[](2);
        assets[0] = tokenIn;
        assets[1] = tokenOut;
        IBalancerVault.BatchSwapStep[] memory swaps = new IBalancerVault.BatchSwapStep[](1);
        swaps[0] = IBalancerVault.BatchSwapStep({
            poolId: poolId, assetInIndex: 0, assetOutIndex: 1, amount: amountIn, userData: bytes("")
        });
        int256[] memory limits = new int256[](2);
        limits[0] = int256(amountIn);
        limits[1] = -int256(minOut);
        _ensureAllowance(tokenIn, vaultAddr, amountIn);
        // FundManagement construction + the 6-arg batchSwap are isolated in a thin
        // helper so the decoded locals above do not all stay live at the external
        // call site. This keeps the legacy (non-viaIR) codegen under the 16-slot
        // EVM stack limit (fixes "Stack too deep" at the batchSwap call).
        _runBalancerBatchSwap(swaps, assets, limits, deadline);
    }

    function _runBalancerBatchSwap(
        IBalancerVault.BatchSwapStep[] memory swaps,
        address[] memory assets,
        int256[] memory limits,
        uint256 deadline
    ) private {
        IBalancerVault.FundManagement memory fm = IBalancerVault.FundManagement({
            sender: address(this), fromInternalBalance: false, recipient: address(this), toInternalBalance: false
        });
        vault.batchSwap(IBalancerVault.SwapKind.GIVEN_IN, swaps, assets, fm, limits, deadline);
    }

    function _execGeneric(bytes memory data) internal {
        (address target, bytes memory callData, uint256 action, address token, uint256 amount) =
            abi.decode(data, (address, bytes, uint256, address, uint256));
        if (action == 1) {
            if (token == address(0) || target == address(0)) revert InvalidGenericAction();
            _ensureDirectAllowance(token, target, amount);
        } else if (action == 2) {
            if (token == address(0) || target == address(0)) revert InvalidGenericAction();
            _safeTransfer(token, target, amount);
        } else if (action != 0) {
            revert InvalidGenericAction();
        }
        // wrap external generic calls to capture revert data via SafeCall
        target.safeCall(callData);
    }


    function _repay(LoanProvider provider, address token, uint256 amount, address target) internal {
        if (
            provider == LoanProvider.BALANCER || provider == LoanProvider.UNIV2 || provider == LoanProvider.UNIV3
        ) {
            _safeTransfer(token, target, amount);
        } else if (provider == LoanProvider.AAVE || provider == LoanProvider.ERC3156) {
            _ensureDirectAllowance(token, target, amount);
        } else {
            revert InvalidGenericAction();
        }
    }





    function _validateActiveLoanAndSender(LoanProvider provider, bytes calldata data) private view returns (address sender) {
        sender = msg.sender;
        ActiveLoanContext memory ctx = activeLoan;
        if (ctx.provider != provider) revert InvalidGenericAction();
        if (ctx.lender != sender) revert InvalidProviderAddress();
        if (ctx.ctxHash != keccak256(data)) revert InvalidGenericAction();
    }

    function _executeVersionedFlashCallback(
        uint8 version,
        bytes memory payload,
        address token,
        uint256 amount,
        uint256 fee,
        address providerAddr
    ) private {
        if (version != PLAN_VERSION_V2) revert UnsupportedPlanVersion();
        PlanV2 memory p = abi.decode(payload, (PlanV2));
        _handleFlashLoanV2(p, token, amount, fee, providerAddr);
    }

    function _legacyToV2(PlanLegacy calldata p) private pure returns (PlanV2 memory out) {
        Loan[] memory loans = new Loan[](1);
        loans[0] = Loan({token: p.loanToken, amount: p.amountIn, provider: p.loanProvider, providerAddr: address(0)});
        out = PlanV2({loans: loans, cycleSlippageBps: p.cycleSlippageBps, steps: p.steps, minProfit: p.minProfit});
    }

    function _validateLegacyProvider(LoanProvider provider) private pure {
        if (provider == LoanProvider.BALANCER || provider == LoanProvider.AAVE) {
            return;
        }
        if (provider == LoanProvider.UNIV2 || provider == LoanProvider.UNIV3) revert InvalidProviderAddress();
        revert InvalidGenericAction();
    }


    function _tokenAt(bytes memory path, uint256 idx) private pure returns (address t) {
        if (path.length < 43 || (path.length - 20) % 23 != 0) revert InvalidGenericAction();
        uint256 tokenCount = ((path.length - 20) / 23) + 1;
        if (idx >= tokenCount) revert InvalidGenericAction();
        uint256 off = idx * 23;
        assembly {
            t := shr(96, mload(add(add(path, 32), off)))
        }
    }

    function _ensureDirectAllowance(address token, address spender, uint256 amount) private {
        if (amount == 0) {
            token.safeCallOptionalReturnBool(abi.encodeWithSelector(IERC20.approve.selector, spender, 0));
            return;
        }

        uint256 current = IERC20(token).allowance(address(this), spender);
        if (current < amount) {
            if (current != 0) {
                token.safeCallOptionalReturnBool(abi.encodeWithSelector(IERC20.approve.selector, spender, 0));
            }
            token.safeCallOptionalReturnBool(abi.encodeWithSelector(IERC20.approve.selector, spender, amount));
        }
    }

    function _setPermit2Allowance(address token, address spender, uint256 amount) private {
        if (token == address(0) || spender == address(0)) revert InvalidGenericAction();
        if (address(permit2) == address(0)) revert InvalidPermit2();
        if (amount > type(uint160).max) revert Permit2AmountOverflow();

        if (amount > 0) {
            _ensurePermit2SpenderApproval(token, amount);
        }

        uint160 amt = uint160(amount);
        uint48 expiry = amount == 0 ? uint48(0) : PERMIT2_MAX_EXPIRATION;
        permit2.approve(token, spender, amt, expiry);
    }

    function _ensurePermit2Allowance(address token, address spender, uint256 amount) private {
        if (amount == 0) {
            _setPermit2Allowance(token, spender, 0);
            return;
        }

        (uint160 current,,) = permit2.allowance(address(this), token, spender);
        if (current < amount) {
            _setPermit2Allowance(token, spender, amount);
        }
    }

    function _ensureAllowance(address token, address spender, uint256 amount) private {
        if (spender == address(permit2)) {
            _ensurePermit2Allowance(token, spender, amount);
            return;
        }

        uint256 current = IERC20(token).allowance(address(this), spender);

        if (amount == 0) {
            if (current != 0) {
                token.safeCallOptionalReturnBool(abi.encodeWithSelector(IERC20.approve.selector, spender, 0));
            }
            return;
        }

        if (current < amount) {
            if (current != 0) {
                token.safeCallOptionalReturnBool(abi.encodeWithSelector(IERC20.approve.selector, spender, 0));
            }
            token.safeCallOptionalReturnBool(
                abi.encodeWithSelector(IERC20.approve.selector, spender, type(uint256).max)
            );
        }
    }


    function _ensurePermit2SpenderApproval(address token, uint256 minNeeded) private {
        (bool ok, bytes memory ret) =
            token.staticcall(abi.encodeWithSelector(IERC20.allowance.selector, address(this), address(permit2)));
        if (!ok || ret.length < 32) revert InvalidPermit2();
        uint256 current = abi.decode(ret, (uint256));
        if (current < minNeeded) {
            if (current != 0) {
                token.safeCallOptionalReturnBool(
                    abi.encodeWithSelector(IERC20.approve.selector, address(permit2), 0)
                );
            }
            token.safeCallOptionalReturnBool(
                abi.encodeWithSelector(IERC20.approve.selector, address(permit2), type(uint256).max)
            );
        }
    }

    function _safeTransfer(address token, address to, uint256 amount) private {
        if (token == address(0) || to == address(0)) revert InvalidGenericAction();
        token.safeCallOptionalReturnBool(abi.encodeWithSelector(IERC20.transfer.selector, to, amount));
    }

    function _computeUniswapV2FlashFee(address pair, uint256 amount) private view returns (uint256 fee) {
        uint256 swapFeeBps = uniswapV2FlashFeeBps[pair];
        if (swapFeeBps == 0) {
            swapFeeBps = 30;
        }

        uint256 denominator = BPS - swapFeeBps;
        uint256 numerator = amount * swapFeeBps;
        fee = numerator / denominator;
        if (numerator % denominator != 0) {
            fee += 1;
        }
    }

    function _handleFlashLoanV2(PlanV2 memory p, address token, uint256 amount, uint256 fee, address providerAddr)
        private
    {
        if (p.loans.length != 1) revert InvalidLoanCount();
        Loan memory loan = p.loans[0];
        if (
            (loan.provider == LoanProvider.ERC3156 || loan.provider == LoanProvider.UNIV2
                || loan.provider == LoanProvider.UNIV3) && providerAddr == address(0)
        ) revert InvalidProviderAddress();
        if (token != loan.token) revert InvalidGenericAction();
        address canonicalToken = canonicalProfitToken;
        if (canonicalToken != address(0) && canonicalToken != token) revert InvalidCanonicalProfitToken();

        address expectedAddr = _resolveProviderAddress(loan);
        if (loan.provider == LoanProvider.ERC3156) {
            if (expectedAddr != providerAddr) revert InvalidProviderAddress();
        } else if (providerAddr != address(0) && providerAddr != expectedAddr) {
            revert InvalidGenericAction();
        }

        _executePlan(p.cycleSlippageBps, p.steps);

        uint256 repay = amount + fee;
        uint256 startBalance = _getActiveStartBalance(token);
        uint256 balanceAfterPlan = IERC20(token).balanceOf(address(this));

        if (loan.provider == LoanProvider.ERC3156) {
            if (balanceAfterPlan < startBalance + repay) {
                revert InsufficientFinalBalance(balanceAfterPlan, startBalance + repay);
            }

            uint256 profitDelta = balanceAfterPlan - startBalance - repay;
            lastGrossProfit = profitDelta;

            _distributeProfit(token, profitDelta, p.minProfit);
            _ensureDirectAllowance(token, providerAddr, repay);
        } else {
            _repay(loan.provider, token, repay, providerAddr);

            uint256 finalBalance = IERC20(token).balanceOf(address(this));
            if (finalBalance < startBalance) {
                revert InsufficientFinalBalance(finalBalance, startBalance);
            }

            uint256 profitDelta = finalBalance - startBalance;
            lastGrossProfit = profitDelta;

            _distributeProfit(token, profitDelta, p.minProfit);
        }

        _clearActiveStartBalance();
    }

    function _executePlan(uint16 cycleSlippageBps, Step[] memory steps) private {
        (, uint16 maxSlippageBps, uint32 deadlineBuffer) = ConfigCodec.unpack(config.raw);
        uint256 deadline = block.timestamp + deadlineBuffer;

        if (cycleSlippageBps > maxSlippageBps) revert InvalidMaxSlippage();

        if (isCircuitOpen()) {
            revert CircuitOpen();
        }

        this._executeSteps(steps, deadline, address(this), address(vault));
    }

    function _distributeProfit(address token, uint256 profitDelta, uint256 minProfit) private {
        (uint16 feeBpsLocal,,) = ConfigCodec.unpack(config.raw);
        if (profitDelta < minProfit) revert();

        uint256 ownerFee = (profitDelta * feeBpsLocal) / BPS;
        if (ownerFee > 0) {
            _safeTransfer(token, feeRecipient, ownerFee);
        }

        uint256 remainder = profitDelta - ownerFee;
        if (remainder > 0) {
            _safeTransfer(token, profitRecipient, remainder);
        }

        emit ProfitRealised(token, profitDelta, ownerFee);
    }

    function _resolveProviderAddress(Loan memory loan) private view returns (address) {
        if (loan.provider == LoanProvider.BALANCER) {
            return address(vault);
        }
        if (loan.provider == LoanProvider.AAVE) {
            return address(aavePool);
        }
        return loan.providerAddr;
    }

    function _validatePlanGuards(uint16 cycleSlippageBps) private view {
        if (isCircuitOpen()) revert CircuitOpen();
        (, uint16 maxSlippageBps,) = ConfigCodec.unpack(config.raw);
        if (cycleSlippageBps > maxSlippageBps) revert InvalidMaxSlippage();
    }

    function _validateConfig(uint16 feeBps, uint16 maxSlippageBps, uint32 deadlineBuffer) private pure {
        if (feeBps > BPS) revert InvalidFeeBps();
        if (maxSlippageBps > BPS) revert InvalidMaxSlippage();
        if (deadlineBuffer > MAX_DEADLINE_BUFFER) revert InvalidDeadline();
    }

    function sweep(address token, address to, uint256 amount) external onlyOwner nonReentrant {
        _safeTransfer(token, to, amount);
    }
}
