Executor owner = owner/admin    or    wallet address
Executor address = clone address

private key - wallet

signer address = wallet address



### Short answer

- **`executor_address`** = the deployed **executor clone** (`MultiVenueArbImplementation`) address.

- **`executor_owner`** (in ops/env) is intended as the **human/multisig governance owner** that ultimately controls execution via the router owner path. In deploy flow, that configured owner is transferred to the **BatchRouter owner**, while the executor contract owner remains the router contract itself.

- So yes: your mental model is right:
  
  - `executor-owner` = wallet/multisig address (router owner)
  
  - `executor address` = clone contract address

### Why the scanner warns

Startup currently compares `EXECUTOR_OWNER` against `executor.owner()` on-chain. But `executor.owner()` is expected to be the **BatchRouter contract address**, not your wallet. That is why you get:

> “executor owner mismatch; using on-chain owner”

### What this means operationally

- If deploy followed the documented flow, this warning is mostly **cosmetic** under the current code path.

- The key invariant is:
  
  1. `executor.owner() == BatchRouter address` and
  
  2. `BatchRouter.owner() == your configured wallet/multisig`
