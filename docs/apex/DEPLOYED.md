# What is on-chain

Recorded 2026-09-25, when `main` was fast-forwarded to the Phase 0–8 work.
Amended 2026-09-29, when G-SEC-1 and R-03 closed. **Rewritten 2026-10-03, when
the Phase 5 executor was deployed** — every address and role below was read back
from Base the same day (block 52,105,860), not copied from the deploy's output.

## The Phase 5 executor on Base

Deployed by `script/DeployAndConfigure.s.sol` in one run: six transactions in
blocks 52,105,407–52,105,408, 6,118,771 gas, sent by `0x69D5…8A3`.

| Contract | Address | What it is |
|---|---|---|
| `MultiVenueArbImplementation` | `0x5C48d8845aaA633c5EE233E7D73b193F7F405Cee` | The implementation, 20,732 bytes. |
| `ArbitrageCloneFactory` | `0xE6E1a9ef38620E5682735e00d26AEDf04ABA02bf` | Deploys clones of it. |
| **Executor clone** | **`0x8940B565D050974b2b589B70De43bCb753b1F93B`** | The executor: an EIP-1167 proxy to the implementation. Runtime code hash `0x68ea95b65ff6d634c1e8e766cf66ecd3996eb0342cdc2f1179485ae03b7fa488`. Every plan is sent here. |
| `BatchRouter` | `0x9ecde68C269EbbaFD44bbB4DfC9D6716B47952C2` | Owns the clone, and is its profit recipient. |

**Roles, read back:**

- The clone's `owner()` is the router; the router's `owner()` is
  `0x69D54e5fC0b9325D7250f0D0A11690327A3dd8A3`, which deployed everything. That
  address is the **admin**.
- `executors(...)` is true for the router and for the trading signer
  `0xCB436Ba3acb945b3fc8EE6345857262584356595`, and **false for the admin**.
- Adapter 1 is Aerodrome Slipstream's `SwapRouter`
  (`0xBE6D8f0d05cC4be24d5167a3eF062215bE6D18a5`), with `exactInputSingle`
  (`0xa026383e`) allowed — what `apex-exec`'s call builder encodes for a
  Slipstream hop.

**The admin is the address that was the trading signer until this deploy.**
`0x69D5…8A3` deployed and owns; a new address, `0xCB43…6595`, trades. §18.1
holds — the trading signer neither deployed nor owns the executor — but it
changes what `.env`'s `PRIVATE_KEY` is: **it is now the owner's key.** Nothing
on the trading path may load it. `scripts/shadow.sh` reads the trader's key from
`TRADER_PRIVATE_KEY` and has no path to `PRIVATE_KEY`, and `apex shadow` refuses
to start unless that key signs as the configured trader.

**What it holds: nothing** — no ETH or WETH in the clone or the router. The
shadow run sends nothing; R-03's canary funding comes later, at canary size.

## The previous executor: retired, never to be funded

`0xDbFB219b4F1CE08fA61C5cD3c08C1307760cAec6` is a `MultiVenueArbImplementation`
clone deployed **before Phase 5 rebuilt the contract set**. It resolves no
adapters through `AdapterRegistry`, checks no profit invariant, verifies no plan
commitment, and still contains `_execGeneric` and its `target.call(data)` — the
exact surface R-03 existed for. It holds nothing (0 ETH, no token balances;
`docs/apex/reports/security-B1.md` records the same) and **must never be
funded**: the security review covered the contracts in this repository, which
are not its bytecode.

Its router was `0x8e04a6aeb3aB33349aDf849039Fa2aFC17c79954`. `ops/inputs.yaml`'s
`executor_address` and `batch_router_address`, and `ops/shadow.base.yaml`, now
name the Phase 5 contracts. Two entries still name `0xDbFB…`: the legacy
liquidation markets' `adapter` fields in `ops/inputs.yaml`, which call
`underlying()` — a function the Phase 5 executor does not implement — on the
legacy path no `apex-*` crate reads. They go with that path, not with this
deploy.

## Why nothing had been redeployed before, and what changed on 2026-09-29

Two gates in PLAN.md blocked it. **Both closed on 2026-09-29**, and the
distinction between them is why this section stays.

- **G-SEC-1** — Phase 5 Task 5.6 Step 3 requires an external security review of
  `contracts/core/` and `contracts/chains/BaseArbExecutor.sol`, with no
  unresolved high or critical findings. **Opened by the operator 2026-09-29** on
  a peer review by developers from a Web3 security community. The earlier
  SolidityScan re-run (98.5/100, 2026-09-25) was evidence and was never that
  review; the tool's own disclaimer says a static scan cannot replace a manual
  audit. PLAN.md's Task 5.6 Step 3 status block is the record.
- **R-03** — do not fund the executor beyond canary size until G-SEC-1 clears.
  It clears with G-SEC-1: Task 5.1 removed `_execGeneric`, which was R-03's
  other precondition, and §39 paired the two.

**What that unblocked was a deployment, not funding for the old address.** The
order it set — deploy, take the new address, update `ops/inputs.yaml` and this
file in the same change, then fund at canary size — is the order followed: the
first three on 2026-10-03, and funding not yet.

## The `master` branch, checked 2026-09-30

**Nothing on `master` belongs on `main`.** Checked rather than assumed, because
the two branches share no common ancestor and that looks alarming until you see
why.

`git merge-base origin/main origin/master` exits non-zero: **unrelated
histories**. Different roots — `master` from `588f78d` (2026-06-02) and `main`
from `7dab66b` (2026-08-16 10:25). `master`'s tip is `d9499a2`, nineteen minutes
*before* main's root.

The re-init is what happened, and it lost nothing:

```text
git diff --name-only origin/master <main's root>   →   0 paths
```

**Main's root commit contains master's tip tree byte-identically.** So
`master`'s 41 "unique" commits are unique only in the sense that the history was
re-rooted; every line of their content is present in main's first commit. The 326
commits since are the divergence, and all of it is deliberate.

`master` carries 147 tracked paths `main` does not, and each class has a reason:

| On master, not on main | Why |
|---|---|
| 48 × `src/*.rs`, 2 × `tests/*.rs` | Relocated. 40 to `crates/arb-exec-legacy/`, 10 to `apex-math` / `apex-venues` — `math.rs`, `quote_common.rs`, `quote_solidly.rs` to the first and the rest of the quoters plus `discovery.rs` to the second, exactly as §34.6 specifies. Every one verified present. |
| 147 × `contracts/artifacts/**` | **Deliberately untracked** on `main`: `.gitignore:102` carries `/contracts/artifacts/` with the reason beside it — *"Forge artifacts committed by hand. `out/` is the single source of truth."* |
| `contracts/executor/steps/{Bridge,Generic,Jit,Swap}Executor.sol` | Deleted by Task 5.5 once `_executeSteps` called the handlers directly. The trampolines were a contract-size workaround that had become pure indirection. |
| `contracts/libraries/BridgeLib.sol`, `contracts/mocks/MockBridge.sol` | §1.4 excludes cross-chain bridging. |
| `plan.md`, `docs/ARCHITECTURE_PIVOT_HANDOFF.md`, `docs/PRODUCTION_AUDIT_FIX_PLAN.md` | Superseded by `PLAN.md`. |
| `prometheus.yml`, `docs/grafana/*` | Relocated to `ops/observability/` — all four files present there. |

**What to do with `master`: nothing, and that is a decision rather than neglect.**
It is a complete, self-consistent snapshot of the pre-v4 system with its own
history, and deleting it would discard the only record of how that system got
there — `main`'s root squashed all of it into one commit. It is 326 commits
behind and must never be merged: an unrelated history would reintroduce the
workspace split's `src/` tree, the deleted step trampolines and the bridge code
in one move.
