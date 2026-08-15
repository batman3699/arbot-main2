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

### Why the scanner used to warn (fixed)

Startup previously compared `EXECUTOR_OWNER` (your wallet) against `executor.owner()` on-chain.
That was wrong: `executor.owner()` is the **BatchRouter** contract, not your wallet.

The bot now validates the full chain:

1. `executor.owner() == BatchRouter`
2. `BatchRouter.owner() == BASE_EXECUTOR_OWNER` (your operator wallet)

A mismatch on step 2 is a **hard startup failure**, not a cosmetic warning.

### Base (chain id 8453) inclusion

On Base, arbot submits via **public `eth_sendRawTransaction`**, not Flashbots-style bundle relays.
Configure inclusion competitiveness with:

- `ARBOT_TIP_BPS` — fraction of expected net profit (basis points) bid as priority fee per gas
- `SEARCHER_PRIORITY_FEE_WEI` / `FILLER_PRIORITY_FEE_WEI` — static priority fee floors

Shadow mode logs the computed tip and would-send transaction without broadcasting.
If `PRIVATE_RELAY_URL` is set on Base, startup emits a warning and the public path is used anyway.

### REVM fork simulation (`ARBOT_SIM_REVM=1`)

Optional in-process fork simulation via `src/sim_revm.rs` before falling back to `eth_call`.

| Variable | Default | Description |
|----------|---------|-------------|
| `ARBOT_SIM_REVM` | `0` | Enable revm fork sim |
| `ARBOT_SIM_REVM_TIMEOUT_MS` | `500` | Total budget for prefetch + execute |
| `ARBOT_SIM_L1_FEE` | `1` on Base (8453) | Model OP-Stack L1 data fee via GasPriceOracle `getL1Fee` |
| `ARBOT_SIM_PREFETCH` | `1` | Batch-prefetch caller, executor, plan tokens before revm |
| `ARBOT_SIM_PREFETCH_MAX_ACCOUNTS` | `32` | Cap on unique accounts prefetched |

Prometheus: `sim_revm_prefetch_accounts_total`, `sim_revm_prefetch_ms`.
