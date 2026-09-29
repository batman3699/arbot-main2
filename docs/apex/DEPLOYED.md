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
