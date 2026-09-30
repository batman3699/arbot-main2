# What is on-chain, and how it differs from this repository

Recorded 2026-09-25, when `main` was fast-forwarded to the Phase 0–8 work.
Amended 2026-09-29, when G-SEC-1 and R-03 closed. **Everything this file says
about the deployed bytecode is still true** — no redeploy has happened.

## The repository no longer matches the deployed bytecode

`0xDbFB219b4F1CE08fA61C5cD3c08C1307760cAec6` on Base is a
`MultiVenueArbImplementation` clone deployed **before Phase 5 rebuilt the
contract set**. It is the only executor this project has ever deployed.

Reading `contracts/` on `main` will now tell you the executor resolves adapters
through `AdapterRegistry`, checks a multi-asset profit invariant, verifies a plan
commitment, and has no arbitrary-call path. **The deployed bytecode does none of
those things.** `AdapterRegistry.sol` was first committed in `e248550`, long
after that address was created; the code at that address still contains
`_execGeneric` and its `target.call(data)`.

The gap is deliberate and it is not a deployment that went wrong. Phase 5
replaced the contracts; nothing has been redeployed, because redeploying is
gated — see below.

## What that address holds

**Nothing.** 0 ETH and no token balances, and no live run has occurred.
`docs/apex/reports/security-B1.md` records the same, and it is why B-1's
disclosure created no exposure: the analysis was published alongside source that
had been public throughout, describing a contract holding nothing.

## Why nothing had been redeployed, and what changed on 2026-09-29

Two gates in PLAN.md blocked it. **Both are now closed**, and the distinction
between them is the reason this section is not simply deleted.

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

**What that unblocked is a deployment. It did not make this address fundable.**
The review covered the contracts in this repository, and as the section above
says, they are not the bytecode at `0xDbFB…cAec6` — that address still holds
`_execGeneric` and its `target.call(data)`, the exact surface R-03 existed for.
Funding it would put capital behind the one contract the review did not read,
while a reader of the gate log would reasonably believe the opposite.

The order is: deploy from `script/Deploy.s.sol`, take the new address, update
`ops/inputs.yaml` and this file in the same change, then fund at canary size.

## If and when a redeploy happens

Three things have to be true, and the first two are the reason this file exists
rather than a changelog entry:

1. **The address changes.** A fresh clone means a new address. Every
   `executor_address` in `ops/inputs.yaml` and anything referencing
   `0xDbFB…cAec6` has to move with it, or the system will sign plans against a
   contract that cannot execute them.
2. **This file stops being true.** Update it in the same change, or the next
   reader inherits a document asserting a mismatch that no longer exists —
   which is worse than no document.
3. `script/Deploy.s.sol` is the path. Task 5.7 gave it a `DeployConfig` seam, so
   a deploy is a function of explicit inputs rather than of whatever the
   environment happened to hold.

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
