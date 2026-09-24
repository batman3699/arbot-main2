// SPDX-License-Identifier: MIT
pragma solidity ^0.8.21;

import {AdapterRegistry, UnknownAdapter} from "../core/AdapterRegistry.sol";
import {ProfitInvariant} from "../core/ProfitInvariant.sol";
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

contract MultiVenueArbImplementation is AdapterRegistry, IAaveFlashLoanSimpleReceiver, IERC3156FlashBorrower, ReentrancyGuard {
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
        /// How much of a non-profit token this route expects to be left
        /// holding. Zero for a route that ends flat, which is every route the
        /// planner currently builds. A surplus above it reverts: an
        /// intermediate balance nobody expected means a hop was mispriced or
        /// overtaken, and keeping it quietly lets the contract's balance sheet
        /// drift from what the planner believes.
        uint256 declaredResidue;
        /// keccak256 over every parameter that decides what this plan DOES,
        /// computed by whoever built it (§25, INV-06). The executor recomputes
        /// it and reverts on mismatch.
        ///
        /// `bytes32(0)` means "not committed" and is accepted, because the
        /// commitment arrives with the encoder that produces it and a plan
        /// built before that encoder is not malformed. Once the encoder always
        /// sets it, Phase 7 makes a zero commitment a rejection.
        bytes32 commitment;
        /// The chain this plan was built for (INV-30). Zero means unstated.
        uint64 chainId;
        /// Unix seconds after which this route is stale (INV-31). Zero means
        /// no expiry.
        ///
        /// Zero-as-absent is weaker here than it is for `commitment`, and the
        /// asymmetry is deliberate rather than overlooked: an unstated
        /// commitment simply skips a check, while an unstated deadline means a
        /// plan never expires. That is the status quo -- today's contract has
        /// no plan deadline at all, deriving one from `block.timestamp` at
        /// execution, which is not an expiry but a fresh clock every time. So
        /// this is not a regression, and Phase 7's encoder is where zero
        /// becomes a rejection.
        uint64 deadline;
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


    uint256 private constant BPS = 10_000;
    uint32 private constant MAX_DEADLINE_BUFFER = 3600;
    uint48 private constant PERMIT2_MAX_EXPIRATION = type(uint48).max;
    bytes32 private constant ERC3156_CALLBACK_SUCCESS = keccak256("ERC3156FlashBorrower.onFlashLoan");

    constructor() {
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
    // `InsufficientFinalBalance` was declared here and is gone. Nothing raises
    // it any more -- `ProfitInvariant.DebtNotRepaid` carries the same two
    // balances plus the token -- and an error in the ABI that cannot occur
    // tells an integrator to handle a failure mode that does not exist.
    error InvalidPermit2();
    error Permit2AmountOverflow();
    error CircuitOpen();
    error InvalidLoanCount();
    /// The plan does not hash to the commitment it carries (§25, INV-06).
    /// Both values are reported so the mismatch can be diffed against the
    /// ticket rather than merely observed.
    error CommitmentMismatch(bytes32 declared, bytes32 recomputed);
    /// The plan was built for a different chain (INV-30).
    error WrongChain(uint64 planChainId, uint256 actualChainId);
    /// The route's validity window has passed (INV-31).
    error RouteExpired(uint64 deadline, uint256 blockTimestamp);
    error InvalidProviderAddress();
    error EmptyRevertData();
    error UnsupportedPlanVersion();

    /// Put an adapter on the allowlist. `onlyOwner`: see `AdapterRegistry`
    /// for why the executor role is deliberately not enough.
    function registerAdapter(uint16 adapterId, address adapter) external onlyOwner {
        _registerAdapter(adapterId, adapter);
    }

    /// Take one off. Replacing an adapter is deregister-then-register, two
    /// transactions, so a substitution cannot happen silently in one.
    function deregisterAdapter(uint16 adapterId) external onlyOwner {
        _deregisterAdapter(adapterId);
    }

    /// Allow one function on a registered adapter (INV-26).
    function allowSelector(uint16 adapterId, bytes4 selector) external onlyOwner {
        _allowSelector(adapterId, selector);
    }

    function revokeSelector(uint16 adapterId, bytes4 selector) external onlyOwner {
        _revokeSelector(adapterId, selector);
    }

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

    /// Recompute a plan's commitment (§25, INV-06).
    ///
    /// # What the on-chain check actually catches
    ///
    /// Worth stating plainly, because it is easy to claim more. The plan and
    /// its commitment arrive in the same calldata from the same caller, so an
    /// executor that wanted to run a different trade could simply commit to
    /// the different trade. This check does **not** constrain a malicious
    /// executor.
    ///
    /// What it does constrain is everything between the planner and the chain:
    /// an encoder that builds a plan the planner did not describe, a field
    /// dropped or reordered by an ABI change, a transport that corrupts one
    /// word. Those are the failures that are otherwise invisible — the trade
    /// executes, it is simply not the trade that was simulated, risk-checked
    /// and sized. The off-chain half of INV-06 (a signer refusing a payload
    /// whose recomputed commitment differs from the ticket's) is what
    /// constrains the executor, and the two halves catch different things.
    ///
    /// # What is in it
    ///
    /// Everything that decides what the plan does, and nothing that a correct
    /// execution may legitimately vary. `block.chainid` and `address(this)`
    /// are in: a plan committed for one deployment must not execute on
    /// another, which is INV-05's wrong-chain submission expressed where it
    /// can actually be enforced. Gas price, deadline buffer and block number
    /// are out: they vary between commitment and inclusion by design.
    function planCommitment(PlanV2 memory p) public view returns (bytes32) {
        bytes32 loansHash;
        bytes32 stepsHash;
        uint256 len = p.loans.length;
        for (uint256 i; i < len;) {
            Loan memory l = p.loans[i];
            loansHash = keccak256(
                abi.encode(loansHash, l.token, l.amount, uint8(l.provider), l.providerAddr)
            );
            unchecked { ++i; }
        }
        len = p.steps.length;
        for (uint256 i; i < len;) {
            // The step's data is hashed rather than concatenated, so a long
            // payload cannot be split across a boundary to collide with a
            // different step list.
            stepsHash = keccak256(abi.encode(stepsHash, uint8(p.steps[i].op), keccak256(p.steps[i].data)));
            unchecked { ++i; }
        }
        return keccak256(
            abi.encode(
                block.chainid,
                address(this),
                PLAN_VERSION_V2,
                loansHash,
                p.cycleSlippageBps,
                stepsHash,
                p.minProfit,
                p.declaredResidue,
                // Both of these are checked BEFORE the commitment, so leaving
                // them out would let a plan be re-aimed at another chain or
                // given a new expiry without the commitment noticing -- which
                // is exactly what `testEveryCommittedFieldMovesTheCommitment`
                // exists to catch.
                p.chainId,
                p.deadline
            )
        );
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
        if (p.chainId != 0 && p.chainId != block.chainid) {
            revert WrongChain(p.chainId, block.chainid);
        }
        if (p.deadline != 0 && block.timestamp > p.deadline) {
            revert RouteExpired(p.deadline, block.timestamp);
        }
        if (p.commitment != bytes32(0)) {
            bytes32 recomputed = planCommitment(p);
            if (recomputed != p.commitment) revert CommitmentMismatch(p.commitment, recomputed);
        }
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
            if (s.op == Op.UNIV3) {
                _execUniswap(s.data, deadline, self);
            } else if (s.op == Op.BALANCER) {
                _execBalancer(s.data, deadline, vaultAddr);
            } else if (s.op == Op.GENERIC) {
                _execAdapter(s.data);
            } else {
                revert InvalidGenericAction();
            }
            unchecked { ++i; }
        }
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

    /// Execute one step against a **registered** adapter (B-1's fix).
    ///
    /// The payload names an adapter by id and the contract resolves it. Three
    /// things that used to come from the caller no longer do:
    ///
    /// * **the target**, now `adapters[adapterId]`, so a step can only reach
    ///   an address the owner put on the allowlist;
    /// * **the approval spender**, now that same resolved adapter rather than
    ///   a separate address from the payload — which is what made a single
    ///   successful settlement able to leave behind an unlimited standing
    ///   claim on everything the contract holds;
    /// * **an outright transfer destination**, which is gone entirely. A
    ///   settlement contract has no business sending tokens to an address a
    ///   plan chose. That path was already caught by the final-balance check,
    ///   so removing it costs nothing and closes a way to reach a target
    ///   without calling it.
    function _execAdapter(bytes memory data) internal {
        (uint16 adapterId, address token, uint256 approveAmount, bytes memory callData) =
            abi.decode(data, (uint16, address, uint256, bytes));

        address target = _resolveAdapter(adapterId);
        // INV-26: an adapter is a contract with more functions than the one a
        // route needs.
        _requireAllowedCall(adapterId, callData);

        if (approveAmount != 0) {
            if (token == address(0)) revert InvalidGenericAction();
            // The spender is the resolved adapter. There is no expression here
            // that a payload can steer.
            _ensureDirectAllowance(token, target, approveAmount);
        }

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
        out = PlanV2({loans: loans, cycleSlippageBps: p.cycleSlippageBps, steps: p.steps, minProfit: p.minProfit, declaredResidue: 0, commitment: bytes32(0), chainId: 0, deadline: 0});
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

        // Both branches are the same invariant in different shapes, and both
        // now go through `ProfitInvariant` rather than checking inline. The
        // library is the code path while only one loan is permitted, so
        // lifting that restriction later is a data change rather than a
        // rewrite of the settlement -- and a library nothing calls is one
        // nobody finds out is wrong.
        //
        // ERC3156 pulls its repayment AFTER this returns, so the contract must
        // still be holding it. Every other provider is repaid first, so by the
        // time the check runs the debt is zero.
        if (loan.provider == LoanProvider.ERC3156) {
            uint256 profitDelta =
                _assertSettled(token, startBalance, repay, p.declaredResidue);
            lastGrossProfit = profitDelta;

            _distributeProfit(token, profitDelta, p.minProfit);
            _ensureDirectAllowance(token, providerAddr, repay);
        } else {
            _repay(loan.provider, token, repay, providerAddr);

            uint256 profitDelta = _assertSettled(token, startBalance, 0, p.declaredResidue);
            lastGrossProfit = profitDelta;

            _distributeProfit(token, profitDelta, p.minProfit);
        }

        _clearActiveStartBalance();
    }

    /// One borrowed asset, expressed as the multi-asset invariant.
    ///
    /// The array is length one today because `p.loans.length != 1` still
    /// holds: borrowing from several providers at once needs nested callbacks,
    /// which is a capability rather than a restriction to delete, and it is
    /// not what INV-27 is about. What INV-27 is about is that the check is
    /// per-asset and the profit is denominated in exactly one declared token,
    /// and that is true here whether the array holds one entry or four.
    function _assertSettled(
        address token,
        uint256 startBalance,
        uint256 repayment,
        uint256 declaredResidue
    ) private view returns (uint256) {
        ProfitInvariant.Debt[] memory debts = new ProfitInvariant.Debt[](1);
        debts[0] = ProfitInvariant.Debt({
            token: token,
            startBalance: startBalance,
            repayment: repayment,
            declaredResidue: declaredResidue
        });
        return ProfitInvariant.assertMultiAsset(debts, token);
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
