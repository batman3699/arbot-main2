# Easy Monitoring & Profit Guide

You turned on the bot—great job! Now keep an eye on it using these simple steps. Think of this like watching a pet: check its food, mood, and health every day.

## Step 1: Turn on Helpful Logs

1. When you start the bot, use:
   ```bash
   RUST_LOG=info,rpc=info cargo run --release
   ```
2. This prints two kinds of messages:
   - `EXEC` lines tell you about trades.
   - `rpc` lines tell you about your connections.

## Step 2: Understand `EXEC` Success Lines

1. A happy trade looks like this (your numbers will differ):
   ```
   EXEC hops=3 ... netWei=180000000000000 ... latencyMs=900 ... tx=0xabc...
   ```
2. Important words:
   - `hops`: how many swaps happened in the trade.
   - `netWei`: profit after gas and safety buffers. Bigger is better.
   - `gasWei`: the gas cost the bot planned for.
   - `latencyMs`: how long it took to land in a block. Smaller is better.
   - `relayReject`: `true` means the private relay said “no thanks.”
   - `tx`: the transaction hash to look up on a block explorer.
3. To keep score, copy the `netWei` numbers into a spreadsheet, divide each by `1e18` to get ETH, then multiply by the current ETH price to see your profits in dollars.

## Step 3: Understand `EXEC FAILED` Lines

1. A failed try looks like:
   ```
   EXEC FAILED tx=0xabc... reason=SimulationReverted relayReject=false edges_scanned=92
   ```
2. What to do:
   - Read the `reason`. If it says `gas estimation failed`, your RPC might be slow or out of sync.
   - If `relayReject=true`, your private relay did not like the bundle. Try a different relay or raise your tip.
   - If failures keep happening, pause the bot and investigate before losing money.

## Step 4: Watch the Scanner Messages

1. Sometimes you will see:
   ```
   No executable cycle: <reason> (edges scanned: 120)
   ```
2. If this repeats for more than 10 minutes, something might be wrong:
   - Maybe the market is quiet.
   - Maybe your data is stale.
   - Maybe the RPC is down. Check the `rpc` logs next.

## Step 5: Check RPC & Relay Health

1. Look for these messages:
   - `connecting websocket endpoint`: trying to connect.
   - `websocket endpoint connected`: success.
   - `all websocket endpoints failed, backing off before retry`: danger! None of your connections are working.
2. If you hit the danger message:
   - Make sure your provider is online.
   - Try a different RPC URL.
   - Increase `RPC_MAX_BACKOFF_SECS` if the logs flip between connect and fail.

## Step 6: Track Your Profits

1. Add up all the `netWei` values for the day.
2. Divide the total by `1e18` to get ETH.
3. Subtract any extra costs (loans, relay tips) that are not in gas.
4. Check the wallet balance on-chain. It should match your spreadsheet. If it does not, investigate until you find the missing funds.
5. Save the list of `tx` hashes. They prove what happened and help you audit.

### Profit metric definitions (important)

- `gross_profit_*`: profit after the loan is repaid (principal + lender fee), but before executor/protocol fee split.
- `net_profit_*`: profit left after gas and all tracked fees.

Use `net_profit_*` for your daily PnL target and alerting. Use `gross_profit_*` to diagnose execution quality before fee overhead.

## Step 7: Use the Warning Table

| Warning you see | What it probably means | Quick fix |
| ---------------- | ---------------------- | --------- |
| `compPress` above 1.5 for a while | Lots of other bots competing | Raise `ROUTE_MIN_ABS_PROFIT_WEI` or lower `PROFIT_MARGIN_BPS`. |
| `latencyMs` bigger than 1500 | Your bundles are slow | Move a faster relay to the front of `PRIVATE_RELAY_URLS`. |
| Many `relayReject=true` messages | Relay keeps saying no | Double-check bundle format and consider another relay. |
| `all websocket endpoints failed` | No working RPC | Swap to fresh RPC URLs and alert your provider. |
| `SimulationReverted` failures | Bad route info | Update the token list in `.env`/`config` and restart. |

## Step 8: Daily Checklist

- **Every block:** Glance at new `EXEC` lines. They should show positive `netWei`.
- **Every hour:** Scroll through `rpc` logs to confirm the bot is connected and not backing off forever.
- **Every day:** Export your profit numbers, compare wallet balances, and rotate any secrets if your policy requires it.
- **After updates:** Run `cargo run --release`, type `start`, `stop`, and `quit` in the console to make sure the supervisor works.

Stick to this routine and you will catch issues quickly while keeping profits steady.

## Need Full Grafana Telemetry Fast?

If you want a live dashboard that shows profit, gas burn, and relay health at a glance, follow the [Grafana Profit Dashboard Quickstart](./grafana/quickstart.md). It now spells out every click and command—even if you have never touched a terminal before—so you can expose the Prometheus metrics endpoint, run Prometheus + Grafana on the host, and import the pre-built dashboard without guessing.
