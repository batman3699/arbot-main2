"""R37: the Uniswap v4 inventory, kept from PoolManager's Initialize logs.

Every v4 pool is born in one `Initialize(id, currency0, currency1, fee,
tickSpacing, hooks, sqrtPriceX96, tick)` log on PoolManager. This folds such
logs into the records the runtime's loader reads (`load_v4` in
crates/apex-runtime/src/live/inventory.rs), one JSON line a pool:

    {"pool_id", "currency0", "currency1", "fee", "tick_spacing", "hooks", "init_block"}

A fold is idempotent by pool ID, keeps whatever else a record already carries
(its depth), and refuses a log not from PoolManager, not an Initialize, removed,
or whose ID is not the hash of the key it carries. The loader applies the
runtime's own admission rules (hooks, native currency, dynamic fee) later.

Run from scripts/data:

    # Logs from any source (an explorer's getLogs, an RPC answer, JSON lines):
    python3 v4_inventory.py --out ../../data/base/uniswap_v4/pools.jsonl --logs FILE
    # PoolManager's Initialize topic over a range, in 2,000-block chunks:
    python3 v4_inventory.py --out ... --scan FROM TO --rpc URL
    # Daily: from the cursor's block to the head (less a margin), then the cursor:
    python3 v4_inventory.py --out ... --follow CURSOR --rpc URL
    # `--rpc public`: the census's public endpoints in turn, 50 blocks a query.
    # Pool IDs (one a line) resolved through PositionManager.poolKeys, each
    # kept only when its key hashes to its ID (no init_block then):
    python3 v4_inventory.py --out ... --resolve IDS --rpc URL
"""
from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.request

from keccak_lite import keccak256

POOL_MANAGER = "0x498581ff718922c3f8e6a244956af099b2652b2b"
# Uniswap v4's PositionManager on Base: `poolKeys(bytes25)` holds the key of
# every pool it has minted a position in, by the ID's first 25 bytes.
POSITION_MANAGER = "0x7c5f5a4bbd8fd63184577525326123b519429bdc"
POOL_KEYS = "0x86b6be7d"
INITIALIZE = "0xdd466e674ea557f56295e2d0218a125ea4b4f0f6f3307b95f85e6110838d6438"
CHUNK = 2_000
# What public endpoints answer (2026-10-29: blastapi 10 blocks, 1rpc 50,
# Tenderly 1,000), as the census asks them (arb_census.race.STEP).
PUBLIC_CHUNK = 50
# Blocks behind the head a follow stops at: past any reorg Base has had.
FOLLOW_LAG = 30


def _word(hex_str: str, i: int) -> int:
    return int(hex_str[i * 64:(i + 1) * 64], 16)


def _signed(v: int) -> int:
    return v - (1 << 256) if v >= 1 << 255 else v


def _address(v: int) -> str:
    return "0x%040x" % v


def pool_id(currency0: str, currency1: str, fee: int, tick_spacing: int, hooks: str) -> str:
    """`keccak256(abi.encode(currency0, currency1, fee, tickSpacing, hooks))`."""
    words = [int(currency0, 16), int(currency1, 16), fee, tick_spacing % (1 << 256), int(hooks, 16)]
    return "0x" + keccak256(b"".join(w.to_bytes(32, "big") for w in words)).hex()


def _quantity(v) -> int:
    return int(v, 16) if isinstance(v, str) else int(v)


def record_of(log: dict) -> dict | None:
    """The pool record `log` creates, or None when it is refused."""
    topics = [t.lower() for t in log.get("topics", [])]
    data = str(log.get("data", "")).lower().removeprefix("0x")
    if (str(log.get("address", "")).lower() != POOL_MANAGER or log.get("removed")
            or len(topics) != 4 or topics[0] != INITIALIZE or len(data) != 5 * 64):
        return None
    c0, c1 = _address(int(topics[2], 16)), _address(int(topics[3], 16))
    fee, spacing, hooks = _word(data, 0), _signed(_word(data, 1)), _address(_word(data, 2))
    if pool_id(c0, c1, fee, spacing, hooks) != topics[1]:
        return None
    return {"pool_id": topics[1], "currency0": c0, "currency1": c1, "fee": fee, "tick_spacing": spacing,
            "hooks": hooks, "init_block": _quantity(log["blockNumber"])}


def record_of_key(pool: str, answer: str) -> dict | None:
    """The record `poolKeys` answered for `pool`, or None when it does not
    hash to `pool`: an unknown pool's zeros, or another pool sharing its
    first 25 bytes."""
    data = str(answer).lower().removeprefix("0x")
    if len(data) != 5 * 64:
        return None
    c0, c1 = _address(_word(data, 0)), _address(_word(data, 1))
    fee, spacing, hooks = _word(data, 2), _signed(_word(data, 3)), _address(_word(data, 4))
    if pool_id(c0, c1, fee, spacing, hooks) != pool.lower():
        return None
    return {"pool_id": pool.lower(), "currency0": c0, "currency1": c1, "fee": fee, "tick_spacing": spacing, "hooks": hooks}


def resolve(call, ids) -> tuple[list[dict], list[str]]:
    """Each of `ids` through PositionManager.poolKeys: (records, the IDs it
    did not prove)."""
    records, unresolved = [], []
    for i in ids:
        i = i.lower()
        answer = call("eth_call", [{"to": POSITION_MANAGER, "data": POOL_KEYS + i[2:52] + "0" * 14}, "latest"])
        r = record_of_key(i, answer)
        if r is None:
            unresolved.append(i)
        else:
            records.append(r)
    return records, unresolved


def fold(logs, out: str) -> tuple[int, int]:
    """Fold `logs` into the inventory at `out`: (records added, logs refused).
    The file is rewritten only when a record is added."""
    return fold_records([record_of(l) for l in logs], out)


def fold_records(records, out: str) -> tuple[int, int]:
    """Fold `records` (None for one refused) into the inventory at `out`."""
    held: list[dict] = []
    if os.path.exists(out):
        with open(out) as f:
            held = [json.loads(l) for l in f if l.strip()]
    ids = {r["pool_id"].lower() for r in held}
    added, refused = [], 0
    for r in records:
        if r is None:
            refused += 1
        elif r["pool_id"] not in ids:
            ids.add(r["pool_id"])
            added.append(r)
    if added:
        added.sort(key=lambda r: (r.get("init_block", 0), r["pool_id"]))
        os.makedirs(os.path.dirname(out) or ".", exist_ok=True)
        tmp = out + ".tmp"
        with open(tmp, "w") as f:
            for r in held + added:
                f.write(json.dumps(r) + "\n")
            f.flush()
            os.fsync(f.fileno())
        os.replace(tmp, out)
    return len(added), refused


def load_logs(path: str) -> list[dict]:
    """Logs from a file: a JSON array, an RPC answer (`result`), an explorer's
    (`data.result`), or one log a line."""
    with open(path) as f:
        text = f.read()
    try:
        v = json.loads(text)
    except json.JSONDecodeError:
        return [json.loads(l) for l in text.splitlines() if l.strip()]
    if isinstance(v, dict):
        v = v.get("result", v.get("data", {}).get("result") if isinstance(v.get("data"), dict) else None)
    if not isinstance(v, list):
        raise ValueError(f"{path}: no list of logs")
    return v


def scan(call, start: int, end: int, chunk: int = CHUNK) -> list[dict]:
    """PoolManager's Initialize logs from `start` to `end`, `chunk` blocks a
    query. `call(method, params)` answers a JSON-RPC request."""
    logs = []
    for lo in range(start, end + 1, chunk):
        hi = min(lo + chunk - 1, end)
        logs.extend(call("eth_getLogs", [{"address": POOL_MANAGER, "topics": [INITIALIZE],
                                          "fromBlock": hex(lo), "toBlock": hex(hi)}]))
    return logs


def client(url: str):
    """(call, chunk) for `url`: a keyed endpoint at CHUNK blocks a query, or
    `public`, the census's public endpoints in turn, transient failures
    retried on the next, at PUBLIC_CHUNK."""
    if url == "public":
        from arb_census import race
        return race.Public(4.0).call, PUBLIC_CHUNK
    return rpc(url), CHUNK


def rpc(url: str):
    def call(method, params):
        body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
        req = urllib.request.Request(url, body, {"Content-Type": "application/json", "User-Agent": "apex-v4-inventory/1"})
        with urllib.request.urlopen(req, timeout=60) as r:
            answer = json.load(r)
        if "error" in answer:
            raise RuntimeError(f"{method}: {answer['error']}")
        return answer["result"]
    return call


def main(argv=None) -> int:
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("--out", required=True)
    src = p.add_mutually_exclusive_group(required=True)
    src.add_argument("--logs")
    src.add_argument("--scan", nargs=2, type=int, metavar=("FROM", "TO"))
    src.add_argument("--follow", metavar="CURSOR", help="a file holding the last block scanned")
    src.add_argument("--resolve", metavar="IDS", help="a file of pool IDs, one a line")
    p.add_argument("--rpc", help="an endpoint URL, or `public`")
    a = p.parse_args(argv)
    if (a.scan or a.follow or a.resolve) and not a.rpc:
        p.error("--scan, --follow and --resolve need --rpc")
    if a.resolve:
        with open(a.resolve) as f:
            ids = [l.split()[0] for l in f if l.strip()]
        records, unresolved = resolve(client(a.rpc)[0], ids)
        added, refused = fold_records(records, a.out)
        print(json.dumps({"ids": len(ids), "added": added, "unresolved": unresolved, "out": a.out}))
        return 0
    if a.logs:
        logs = load_logs(a.logs)
    elif a.scan:
        call, chunk = client(a.rpc)
        logs = scan(call, a.scan[0], a.scan[1], chunk)
    else:
        call, chunk = client(a.rpc)
        with open(a.follow) as f:
            last = int(f.read().strip())
        head = int(call("eth_blockNumber", []), 16) - FOLLOW_LAG
        logs = scan(call, last + 1, head, chunk) if head > last else []
    added, refused = fold(logs, a.out)
    if a.follow and head > last:
        tmp = a.follow + ".tmp"
        with open(tmp, "w") as f:
            f.write(f"{head}\n")
        os.replace(tmp, a.follow)
    print(json.dumps({"logs": len(logs), "added": added, "refused": refused, "out": a.out}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
