"""Task 8.5 R37: record a Uniswap v4 pool at one block, for the quote-exactness
test (crates/apex-runtime/tests/live_v4_quote.rs).

Through StateView (slot0, liquidity, the tick bitmap and ticks around the
price) and V4Quoter (quoteExactInputSingle, both directions), with foundry's
`cast` on an archive endpoint. Nothing here is computed: every figure is the
chain's answer at `--block`, so the Rust side is held to the quoter, never to
this script.

    python3 -I scripts/data/record_v4_fixture.py --rpc https://base.drpc.org --block 53155787 \\
        --pool 0xc19d91d9...27fa --currency0 0x5be7... --currency1 0x8335... --fee 100 --tick-spacing 1 \\
        --hooks 0xb0b2... --out crates/apex-runtime/tests/fixtures/v4_axis_quotes.json
"""

import argparse
import json
import subprocess
import time

STATE_VIEW = "0xA3c0c9b65baD0b08107Aa264b0f3dB444b867A71"
V4_QUOTER = "0x0d5e0F971ED27FBfF6c2837bf31316121532048D"
POOL_MANAGER = "0x498581fF718922c3f8e6A244956aF099B2652b2b"


def cast(rpc, block, *args, retries=8):
    """One `cast call` at `block`, retried past an endpoint's rate limit."""
    cmd = ["cast", "call", "--rpc-url", rpc, "--block", str(block), *args]
    for attempt in range(retries):
        r = subprocess.run(cmd, capture_output=True, text=True)
        if r.returncode == 0:
            time.sleep(0.25)
            # One value a line, a large number followed by its `[1.2e3]`.
            return [line.split()[0] for line in r.stdout.splitlines() if line.strip()]
        if "429" not in r.stderr and "rate limit" not in r.stderr.lower():
            raise SystemExit(f"cast failed: {' '.join(args[:2])}: {r.stderr.strip()[:300]}")
        time.sleep(2 + 2 * attempt)
    raise SystemExit("rate-limited past every retry")


def number(token):
    """`cast`'s answer for a number, without its `[1.2e3]` annotation."""
    return int(token)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--rpc", required=True)
    p.add_argument("--block", type=int, required=True)
    p.add_argument("--pool", required=True)
    p.add_argument("--currency0", required=True)
    p.add_argument("--currency1", required=True)
    p.add_argument("--fee", type=int, required=True)
    p.add_argument("--tick-spacing", type=int, required=True)
    p.add_argument("--hooks", required=True)
    p.add_argument("--words", type=int, default=3, help="bitmap words read each side of the price's")
    p.add_argument("--sizes0", default="", help="zero-for-one exact inputs, comma-separated base units")
    p.add_argument("--sizes1", default="", help="one-for-zero exact inputs, comma-separated base units")
    p.add_argument("--out", required=True)
    a = p.parse_args()

    manager = cast(a.rpc, a.block, V4_QUOTER, "poolManager()(address)")[0]
    if manager.lower() != POOL_MANAGER.lower():
        raise SystemExit(f"{V4_QUOTER} is not a V4Quoter of PoolManager (poolManager() = {manager})")

    s0 = cast(a.rpc, a.block, STATE_VIEW, "getSlot0(bytes32)(uint160,int24,uint24,uint24)", a.pool)
    sqrt_price, tick, protocol_fee, lp_fee = (number(x) for x in s0)
    liquidity = number(cast(a.rpc, a.block, STATE_VIEW, "getLiquidity(bytes32)(uint128)", a.pool)[0])

    compressed = tick // a.tick_spacing
    here = compressed >> 8
    words = {}
    ticks = {}
    for w in range(here - a.words, here + a.words + 1):
        bits = number(cast(a.rpc, a.block, STATE_VIEW, "getTickBitmap(bytes32,int16)(uint256)", "--", a.pool, str(w))[0])
        words[w] = bits
        for i in range(256):
            if bits >> i & 1:
                t = (w * 256 + i) * a.tick_spacing
                info = cast(a.rpc, a.block, STATE_VIEW, "getTickLiquidity(bytes32,int24)(uint128,int128)", "--", a.pool, str(t))
                ticks[t] = [str(number(info[0])), str(number(info[1]))]

    key = f"({a.currency0},{a.currency1},{a.fee},{a.tick_spacing},{a.hooks})"
    quotes = []
    for zero_for_one, sizes in ((True, a.sizes0), (False, a.sizes1)):
        for size in [s for s in sizes.split(",") if s]:
            out = cast(
                a.rpc,
                a.block,
                V4_QUOTER,
                "quoteExactInputSingle(((address,address,uint24,int24,address),bool,uint128,bytes))(uint256,uint256)",
                f"({key},{str(zero_for_one).lower()},{size},0x)",
            )
            quotes.append({"zero_for_one": zero_for_one, "amount_in": size, "amount_out": str(number(out[0]))})

    fixture = {
        "_provenance": f"Task 8.5 R37. Recorded at Base block {a.block} by scripts/data/record_v4_fixture.py "
        f"through StateView {STATE_VIEW} and V4Quoter {V4_QUOTER} (poolManager() checked). Every figure is the "
        "chain's answer; nothing is computed here.",
        "block": a.block,
        "pool_id": a.pool,
        "key": {"currency0": a.currency0, "currency1": a.currency1, "fee": a.fee, "tick_spacing": a.tick_spacing, "hooks": a.hooks},
        "slot0": {"sqrt_price_x96": str(sqrt_price), "tick": tick, "protocol_fee": protocol_fee, "lp_fee": lp_fee},
        "liquidity": str(liquidity),
        "bitmap": {str(w): str(b) for w, b in sorted(words.items())},
        "ticks": {str(t): v for t, v in sorted(ticks.items())},
        "quotes": quotes,
    }
    with open(a.out, "w") as f:
        json.dump(fixture, f, indent=1)
        f.write("\n")
    print(f"{len(words)} words, {len(ticks)} ticks, {len(quotes)} quotes -> {a.out}")


if __name__ == "__main__":
    main()
