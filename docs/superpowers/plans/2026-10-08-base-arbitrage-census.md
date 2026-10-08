# Base arbitrage census (Task 8.5 R23) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A repeatable, read-only measurement of what arbitrage winners bank on Base over 7 days, ranked by venue, market-maker venue and pair, calm and spike.

**Architecture:** A Python package, `scripts/data/arb_census/`. `rpc.py` talks to BlockPI with a rate limit. `venues.py` decodes each venue's trade event into the venue-side change of each token (a `Leg`). `detect.py` groups legs by transaction, tests closure, and reads what a receipt's transfers banked. `verify.py` checks a decoder against the receipt's own transfers before its venue counts. `collect.py` runs one day at a time into `data/census/`. `report.py` ranks.

**Tech Stack:** Python 3.12 standard library only (urllib, json, gzip, statistics, unittest), `cast` for nothing at runtime, BlockPI JSON-RPC.

Spec: `docs/superpowers/specs/2026-10-08-base-arbitrage-census-design.md`.

## Global Constraints

- Read-only: no transaction is sent, no live-path code changes, the shadow is untouched.
- The BlockPI key is read from `.env`'s `BLOCKPI_KEY` and never printed, logged or put in an argument list.
- Python standard library only; no new packages.
- An arbitrage spends no token beyond 0.01% of that token's flow, and gains at least one. Native ETH counts as WETH.
- Receipts are read for every arbitrage worth $1 or more by pool movement, and every arbitrage with a market-maker-venue leg.
- Spike hours: total gross above 5× the median hour of the window.
- A venue counts only after its decoder passes verification on at least three live transactions; otherwise it is reported as not covered.
- Data under `data/census/` (outside git). Requests rate-limited (default 10 a second).
- Stage named paths only; commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Reference values (computed 2026-10-08 with `cast keccak` / `cast sig`)

| Name | Value |
|---|---|
| Uniswap v3-style `Swap` (Uniswap v3, Slipstream, Algebra v1, forks) | `0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67` |
| PancakeSwap v3 `Swap` | `0x19b47279256b2a23a1665c810c8d55a1758940ee09377d4f8d26497a3577dc83` |
| Uniswap v2 `Swap` | `0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822` |
| Aerodrome v2 `Swap` | `0xb3e2773606abfd36b5bd91394b3a54d1398336c65005baf7bf7a05efeffaf75b` |
| Uniswap v4 `Swap` (PoolManager `0x498581fF718922c3f8e6A244956aF099B2652b2b`) | `0x40e9cecb9f5f1f1c5b9c97dec2917b7ee92e57ba5563708daca94dd84ad7112f` |
| Balancer v2 `Swap` (vault `0xBA12222222228d8Ba445958a75a0704d566BF2C8`) | `0x2170c741c41531aec20e7c107c24eecfdd15e69c9bb0a8dd37b1840b9e0b207b` |
| Balancer v3 `Swap` (vault `0xbA1333333333a1BA1108E8412f11850A5C319bA9`) | `0x0874b2d545cb271cdbda4e093020c452328b24af12382ed62c4d00f5c26709db` |
| `TesseraTrade` (`0x55555522005BcAE1c2424D474BfD5ed477749E3e`) | `0x97ba0cd8ff13f074b3b1aeace7fa3bf7fe54bdf2d728b6a097e901073b2bad6a` |
| `ElfomoTrade` (`0xf0f0F0F0FB0d738452EfD03A28e8be14C76d5f73`) | `0xbe65a3f1f381da16732df786f571604a72b7c122cff3ae2b355566ddf01e2528` |
| Maverick v2 `PoolSwap` (factory `0x0A7e848Aca42d879EF06507Fca0E7b33A0a63c1e`) | `0x103ed084e94a44c8f5f6ba8e3011507c41063177e29949083c439777d8d63f60` |
| Fluid `Swap(bool,uint256,uint256,address)` (resolver `0x160ffC75904515f38C9b7Ed488e1F5A43CE71eBA`) | `0xdc004dbca4ef9c966218431ee5d9133d337ad018dd5b5c5493722803f75c64f7` |
| Curve `TokenExchange` (int128 ids) | `0x8b3e96f2b889fa771c53c981b40daf005f63f637f1869f707052d15a3dd97140` |
| Curve `TokenExchange` (uint256 ids) | `0xb2e76ae99761dc136e598d4a629bb347eccb9532a5f8bbd72e18467c3c34cc98` |
| Algebra Integral `Swap` (9 fields) | `0x121cb44ee54098b1a04743c487e7460d8dd429b27f88b1f4d4767396e1a59f79` |
| Metric `Swap` (adapter's layout) | `0xcd8b75a7fb6cb82ab3acded68ac53af5c43d19b51a91b9d5640bb73622dbdf57` |
| Metric `PoolCreated` (factory `0xe22F9fc0f04486dE25ed6CF1800a4a47aFD82e0C`, Base from block 42,570,144) | `0xe1c304acd9cda85cdced26da15edf3fd1a07d96caf4f9cedb068823618b93d4d` |
| Hanji `OrderPlaced` v2 | `0x1047930a29721c0069210227eeb58936b0a4667fe473b91765b069d71b1e3d7d` |
| Hanji `OnchainCLOBCreated` (factory `0xC7264dB7c78Dd418632B73A415595c7930A9EEA4`, Base from block 37,860,153) | `0x04b3d813686f2d1061e11e5a0d93455065f32174906b0c037cf1176818af86be` |
| ERC-20 `Transfer` | `0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef` |
| Selectors | `token0` `0x0dfe1681`, `token1` `0xd21220a7`, `factory` `0xc45a0155`, `poolKeys(bytes25)` `0x86b6be7d`, `tokenA` `0x0fc63d10`, `tokenB` `0x5f64b55b`, `poolCount` `0xf525cb68`, `lookup(uint256,uint256)` `0xb4b9d1f1`, `getAllPools` `0xd88ff1f4`, `coins(uint256)` `0xc6610657`, `coins(int128)` `0x23746eb8`, `getConfig` `0xc3f909d4`, `decimals` `0x313ce567`, `symbol` `0x95d89b41` |

Fixture transactions: `spike_855` `0xe8c6c2802e2fbd55b883a3a048ef95ce2f8d2b9643c8059493de0fbec3ff0469` (two Uniswap-v3-style legs); `omi_10hop` `0xd7b4462124903cde0ddaf0601be5789dae87d602812f3d3cc4a0aab15831342f` (banked 5.92 WETH); `v4_2hop` `0x3dbfd72f53c0239f9e1074a67c8cd5742c59231bebc34ff1c612e9c3a6967d89`; `tessera` `0x85d0ae824691a6eb43f4b9cc64ef8e1f4e7b017ce436c1d1b21f61d0d70f8e8f`; `elfomo` `0x128acc59243c77034868bfde1db0ea0c131ca1036bfca0386733e90e0d80badd`; `user_route` `0x0a801f75b94e7501133eb742233ae374d21e02fc6083dca50224ec1050ca6c13` (spends WETH: not an arbitrage).

All commands below run from `scripts/data/` unless stated: `cd /home/scotty/arbot-main2/arbot-main-main/scripts/data`.

---

### Task 1: The package, its RPC client and recorded fixtures

**Files:**
- Create: `scripts/data/arb_census/__init__.py`, `scripts/data/arb_census/rpc.py`, `scripts/data/arb_census/record_fixtures.py`, `scripts/data/arb_census/test_rpc.py`
- Data (committed): `scripts/data/arb_census/fixtures/*.json`

**Interfaces:**
- Produces: `rpc.endpoint(env_path) -> str`; `rpc.Rpc(url=None, max_per_second=10.0)` with `.call(method, params)`, `.batch(calls: list[tuple[str, list]], size=50) -> list`; `rpc.RpcError`. Fixture files `fixtures/<name>.json` = `{"tx": hash, "receipt": receipt, "meta": {pool_key: [token0, token1, venue]}}`.

- [ ] **Step 1: Write the failing test** `test_rpc.py`:

```python
import tempfile, unittest
from pathlib import Path
from arb_census import rpc


class RpcTest(unittest.TestCase):
    def test_the_endpoint_comes_from_env_and_the_repr_hides_it(self):
        with tempfile.TemporaryDirectory() as d:
            env = Path(d) / ".env"
            env.write_text("OTHER=1\nBLOCKPI_KEY='abc123secret'\n")
            url = rpc.endpoint(env)
            self.assertTrue(url.endswith("/abc123secret"))
            self.assertNotIn("abc123secret", repr(rpc.Rpc(url=url)))

    def test_a_missing_key_is_refused(self):
        with tempfile.TemporaryDirectory() as d:
            env = Path(d) / ".env"
            env.write_text("OTHER=1\n")
            with self.assertRaises(SystemExit):
                rpc.endpoint(env)


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run it to make sure it fails**

Run: `python3 -m unittest arb_census.test_rpc`
Expected: `ModuleNotFoundError: No module named 'arb_census'`.

- [ ] **Step 3: Write `__init__.py` and `rpc.py`**

`__init__.py`:

```python
"""What arbitrage winners bank on Base, by venue and pair (Task 8.5 R23).

Read-only. Run from scripts/data/: `python3 -m arb_census.collect`, then
`python3 -m arb_census.report`. Spec:
docs/superpowers/specs/2026-10-08-base-arbitrage-census-design.md.
"""
```

`rpc.py`:

```python
"""JSON-RPC to Base through BlockPI, for the census.

The key is read from .env and never printed: the client's repr hides the
endpoint, and transport errors are reported by type, never with the URL.
Requests are rate-limited because the shadow shares the endpoint.
"""
import json
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]


class RpcError(RuntimeError):
    pass


def endpoint(env_path=REPO / ".env") -> str:
    for line in Path(env_path).read_text().splitlines():
        if line.startswith("BLOCKPI_KEY="):
            key = line.split("=", 1)[1].strip().strip('"').strip("'")
            if key:
                return "https://base.blockpi.network/v1/rpc/" + key
    raise SystemExit("no BLOCKPI_KEY in .env")


class Rpc:
    def __init__(self, url=None, max_per_second=10.0):
        self._url = url or endpoint()
        self._gap = 1.0 / max_per_second
        self._next = 0.0
        self._lock = threading.Lock()

    def __repr__(self):
        return "Rpc(<endpoint hidden>)"

    def _post(self, body):
        with self._lock:
            now = time.monotonic()
            if self._next > now:
                time.sleep(self._next - now)
            self._next = max(now, self._next) + self._gap
        req = urllib.request.Request(
            self._url,
            data=json.dumps(body).encode(),
            headers={"Content-Type": "application/json", "User-Agent": "apex-arb-census/1"},
        )
        for attempt in range(4):
            try:
                with urllib.request.urlopen(req, timeout=90) as r:
                    return json.loads(r.read())
            except (urllib.error.URLError, TimeoutError, ConnectionError, json.JSONDecodeError) as e:
                if attempt == 3:
                    raise RpcError(f"transport failed: {type(e).__name__}") from None
                time.sleep(2 ** attempt)

    def call(self, method, params):
        out = self._post({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})
        if "error" in out:
            raise RpcError(str(out["error"].get("message", "rpc error")))
        return out["result"]

    def batch(self, calls, size=50):
        """Each call's result, or None where it failed, in order."""
        res = []
        for i in range(0, len(calls), size):
            chunk = calls[i:i + size]
            body = [{"jsonrpc": "2.0", "id": j, "method": m, "params": p} for j, (m, p) in enumerate(chunk)]
            try:
                out = self._post(body)
                if not isinstance(out, list):
                    raise RpcError("no batch")
                byid = {o.get("id"): o.get("result") for o in out}
                res.extend(byid.get(j) for j in range(len(chunk)))
            except RpcError:
                for m, p in chunk:
                    try:
                        res.append(self.call(m, p))
                    except RpcError:
                        res.append(None)
        return res
```

- [ ] **Step 4: Run the test**

Run: `python3 -m unittest arb_census.test_rpc`
Expected: 2 tests OK.

- [ ] **Step 5: Write `record_fixtures.py`** and record the six fixtures:

```python
"""Record the census tests' fixtures: each transaction's receipt and the
tokens and venue of every pool its swaps touch, read from Base. Run once;
the files are committed so the tests need no network."""
import json
from pathlib import Path
from arb_census.rpc import Rpc

FIXTURES = {
    "spike_855": "0xe8c6c2802e2fbd55b883a3a048ef95ce2f8d2b9643c8059493de0fbec3ff0469",
    "omi_10hop": "0xd7b4462124903cde0ddaf0601be5789dae87d602812f3d3cc4a0aab15831342f",
    "v4_2hop": "0x3dbfd72f53c0239f9e1074a67c8cd5742c59231bebc34ff1c612e9c3a6967d89",
    "tessera": "0x85d0ae824691a6eb43f4b9cc64ef8e1f4e7b017ce436c1d1b21f61d0d70f8e8f",
    "elfomo": "0x128acc59243c77034868bfde1db0ea0c131ca1036bfca0386733e90e0d80badd",
    "user_route": "0x0a801f75b94e7501133eb742233ae374d21e02fc6083dca50224ec1050ca6c13",
}
OUT = Path(__file__).resolve().parent / "fixtures"


def main():
    from arb_census.collect import Resolver
    rpc = Rpc()
    OUT.mkdir(exist_ok=True)
    for name, tx in FIXTURES.items():
        receipt = rpc.call("eth_getTransactionReceipt", [tx])
        resolver = Resolver(rpc)
        meta = resolver.meta_for_logs(receipt["logs"])
        (OUT / f"{name}.json").write_text(json.dumps({"tx": tx, "receipt": receipt, "meta": meta}, indent=1))
        print(name, len(receipt["logs"]), "logs,", len(meta), "pools")


if __name__ == "__main__":
    main()
```

It needs `collect.Resolver` (Task 5); record the fixtures at the end of Task 5, Step 1, and keep this file as written.

---

### Task 2: Venue decoders

**Files:**
- Create: `scripts/data/arb_census/venues.py`, `scripts/data/arb_census/test_venues.py`

**Interfaces:**
- Consumes: fixture files (Task 5 records them; until then the tests use the inline logs below).
- Produces: `Leg(tx: str, block: int, venue: str, pool: str, deltas: dict[str, int])`; `decode(log: dict, meta: dict) -> Leg | None`; topic constants as in the Reference table; `VENUE_OF_FACTORY: dict[str, str]`; `OURS: frozenset[str]`; `MARKET_MAKERS: frozenset[str]`; `pool_key(log) -> str | None` (the key a log's pool metadata is stored under).

`meta` maps a pool key to `[token0, token1, venue]`, or for Curve `[coins..., "curve"]` as `{"coins": [...], "venue": "curve"}`, or for Hanji `{"x": .., "y": .., "sx": int, "sy": int, "venue": "hanji"}`. Keys: the emitting address (lower-case) for pool venues, the pool id for Uniswap v4, `"bal2:" + poolId` / `"bal3:" + pool` for Balancer, the fixed contract for Tessera and ElfomoFi.

- [ ] **Step 1: Write the failing tests** `test_venues.py`:

```python
import unittest
from arb_census import venues as v

T0 = "0x" + "11" * 20
T1 = "0x" + "22" * 20
POOL = "0x" + "aa" * 20


def word(x):
    return format(x % (1 << 256), "064x")


def addr_word(a):
    return a[2:].rjust(64, "0")


def log(address, topics, words, tx="0x01", block=1):
    return {"address": address, "topics": topics, "data": "0x" + "".join(words),
            "transactionHash": tx, "blockNumber": hex(block)}


class DecodeTest(unittest.TestCase):
    def test_a_v3_swap_is_the_pools_own_signed_amounts(self):
        l = log(POOL, [v.UNI_V3, "0x" + "0" * 64, "0x" + "0" * 64], [word(500), word(-300), word(0), word(0), word(0)])
        leg = v.decode(l, {POOL: [T0, T1, "uniswap_v3"]})
        self.assertEqual(leg.deltas, {T0: 500, T1: -300})
        self.assertEqual(leg.venue, "uniswap_v3")

    def test_a_v2_swap_is_in_minus_out(self):
        l = log(POOL, [v.UNI_V2, "0x" + "0" * 64, "0x" + "0" * 64], [word(0), word(70), word(40), word(0)])
        self.assertEqual(v.decode(l, {POOL: [T0, T1, "uniswap_v2"]}).deltas, {T0: -40, T1: 70})

    def test_a_v4_swap_is_negated_and_native_eth_is_weth(self):
        pid = "0x" + "bb" * 32
        l = log(v.POOL_MANAGER, [v.UNI_V4, pid, "0x" + "0" * 64], [word(-1000), word(900), word(0), word(0), word(0), word(0)])
        leg = v.decode(l, {pid: [v.ZERO, T1, "uniswap_v4"]})
        self.assertEqual(leg.deltas, {v.WETH: 1000, T1: -900})

    def test_tessera_carries_its_tokens(self):
        l = log(v.TESSERA_SWAP, [v.TESSERA], [addr_word(T0), addr_word(T1), word(10), word(9), addr_word(POOL)])
        leg = v.decode(l, {})
        self.assertEqual((leg.venue, leg.deltas), ("tessera", {T0: 10, T1: -9}))

    def test_elfomo_carries_its_tokens(self):
        l = log(v.ELFOMO_SWAP, [v.ELFOMO, "0x" + "0" * 64, "0x" + "0" * 64],
                [addr_word(POOL), addr_word(POOL), addr_word(T0), addr_word(T1), word(10), word(9)])
        self.assertEqual(v.decode(l, {}).deltas, {T0: 10, T1: -9})

    def test_maverick_follows_the_side_paid_in(self):
        l = log(POOL, [v.MAVERICK_V2], [addr_word(POOL), addr_word(POOL), word(10), word(0), word(0), word(0), word(10), word(8)])
        self.assertEqual(v.decode(l, {POOL: [T0, T1, "maverick_v2"]}).deltas, {T1: 10, T0: -8})

    def test_fluid_follows_its_direction(self):
        l = log(POOL, [v.FLUID], [word(1), word(10), word(8), addr_word(POOL)])
        self.assertEqual(v.decode(l, {POOL: [T0, T1, "fluid"]}).deltas, {T0: 10, T1: -8})

    def test_curve_reads_its_coins(self):
        l = log(POOL, [v.CURVE_I128, "0x" + "0" * 64], [word(1), word(10), word(0), word(9)])
        self.assertEqual(v.decode(l, {POOL: {"coins": [T0, T1], "venue": "curve"}}).deltas, {T1: 10, T0: -9})

    def test_a_pool_without_metadata_is_not_decoded(self):
        l = log(POOL, [v.UNI_V3, "0x" + "0" * 64, "0x" + "0" * 64], [word(1), word(-1), word(0), word(0), word(0)])
        self.assertIsNone(v.decode(l, {}))

    def test_a_tessera_event_from_another_contract_is_ignored(self):
        l = log(POOL, [v.TESSERA], [addr_word(T0), addr_word(T1), word(10), word(9), addr_word(POOL)])
        self.assertIsNone(v.decode(l, {}))


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `python3 -m unittest arb_census.test_venues`
Expected: `ImportError` (no `venues`).

- [ ] **Step 3: Write `venues.py`**

```python
"""Each venue's trade event, decoded into what the venue's side gained and
lost of each token (R23).

A `Leg` is one trade: `deltas[token]` is positive when the venue received the
token and negative when it paid it out. Native ETH is folded into WETH.
`decode` returns None for a log it cannot attribute: an unknown pool, or a
fixed-contract venue's event emitted by some other contract.
"""
from dataclasses import dataclass, field

WETH = "0x4200000000000000000000000000000000000006"
USDC = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"
ZERO = "0x" + "0" * 40

UNI_V3 = "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67"
PANCAKE_V3 = "0x19b47279256b2a23a1665c810c8d55a1758940ee09377d4f8d26497a3577dc83"
ALGEBRA_INTEGRAL = "0x121cb44ee54098b1a04743c487e7460d8dd429b27f88b1f4d4767396e1a59f79"
UNI_V2 = "0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822"
AERO_V2 = "0xb3e2773606abfd36b5bd91394b3a54d1398336c65005baf7bf7a05efeffaf75b"
UNI_V4 = "0x40e9cecb9f5f1f1c5b9c97dec2917b7ee92e57ba5563708daca94dd84ad7112f"
BAL_V2 = "0x2170c741c41531aec20e7c107c24eecfdd15e69c9bb0a8dd37b1840b9e0b207b"
BAL_V3 = "0x0874b2d545cb271cdbda4e093020c452328b24af12382ed62c4d00f5c26709db"
TESSERA = "0x97ba0cd8ff13f074b3b1aeace7fa3bf7fe54bdf2d728b6a097e901073b2bad6a"
ELFOMO = "0xbe65a3f1f381da16732df786f571604a72b7c122cff3ae2b355566ddf01e2528"
MAVERICK_V2 = "0x103ed084e94a44c8f5f6ba8e3011507c41063177e29949083c439777d8d63f60"
FLUID = "0xdc004dbca4ef9c966218431ee5d9133d337ad018dd5b5c5493722803f75c64f7"
CURVE_I128 = "0x8b3e96f2b889fa771c53c981b40daf005f63f637f1869f707052d15a3dd97140"
CURVE_U256 = "0xb2e76ae99761dc136e598d4a629bb347eccb9532a5f8bbd72e18467c3c34cc98"
METRIC = "0xcd8b75a7fb6cb82ab3acded68ac53af5c43d19b51a91b9d5640bb73622dbdf57"
HANJI_ORDER = "0x1047930a29721c0069210227eeb58936b0a4667fe473b91765b069d71b1e3d7d"
TRANSFER = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"

POOL_MANAGER = "0x498581ff718922c3f8e6a244956af099b2652b2b"
BAL_V2_VAULT = "0xba12222222228d8ba445958a75a0704d566bf2c8"
BAL_V3_VAULT = "0xba1333333333a1ba1108e8412f11850a5c319ba9"
TESSERA_SWAP = "0x55555522005bcae1c2424d474bfd5ed477749e3e"
ELFOMO_SWAP = "0xf0f0f0f0fb0d738452efd03a28e8be14c76d5f73"

# v2/v3-style pools are attributed by their factory().
VENUE_OF_FACTORY = {
    "0x33128a8fc17869897dce68ed026d694621f6fdfd": "uniswap_v3",
    "0x0bfbcf9fa4f9c56b0f40a671ad40e0805a091865": "pancakeswap_v3",
    "0x5e7bb104d84c7cb9b682aac2f3d509f5f406809a": "slipstream_5e7b",
    "0xf8f2eb4940cfe7d13603dddd87f123820fc061ef": "slipstream_f8f2",
    "0xade65c38cd4849adba595a4323a8c7ddfe89716a": "cl_ade6",
    "0x8909dc15e40173ff4699343b6eb8132c65e18ec6": "uniswap_v2",
    "0x420dd381b31aef6683db6b902084cb0ffece40da": "aerodrome_v2",
}
# The venues the shadow prices (R22).
OURS = frozenset({"uniswap_v3", "pancakeswap_v3", "slipstream_5e7b", "slipstream_f8f2"})
MARKET_MAKERS = frozenset({"tessera", "elfomofi", "metric", "hanji"})

POOL_TOPICS = (UNI_V3, PANCAKE_V3, ALGEBRA_INTEGRAL, UNI_V2, AERO_V2, MAVERICK_V2, FLUID,
               CURVE_I128, CURVE_U256, METRIC, HANJI_ORDER)
ALL_TOPICS = POOL_TOPICS + (UNI_V4, BAL_V2, BAL_V3, TESSERA, ELFOMO)


@dataclass(frozen=True)
class Leg:
    tx: str
    block: int
    venue: str
    pool: str
    deltas: dict = field(hash=False, compare=True)


def _s256(h):
    x = int(h, 16)
    return x - (1 << 256) if x >= 1 << 255 else x


def _words(data):
    d = data[2:]
    return [d[i:i + 64] for i in range(0, len(d), 64)]


def _addr(word):
    return "0x" + word[-40:].lower()


def _eth(token):
    return WETH if token == ZERO else token


def _add(deltas, token, x):
    token = _eth(token)
    deltas[token] = deltas.get(token, 0) + x


def pool_key(log):
    """Where a log's pool metadata is stored; None for a fixed-contract venue."""
    t0, a = log["topics"][0], log["address"].lower()
    if t0 == UNI_V4:
        return log["topics"][1] if a == POOL_MANAGER else None
    if t0 == BAL_V2:
        return "bal2:" + log["topics"][1] if a == BAL_V2_VAULT else None
    if t0 == BAL_V3:
        return "bal3:" + _addr(log["topics"][1]) if a == BAL_V3_VAULT else None
    if t0 in (TESSERA, ELFOMO):
        return None
    return a


def decode(log, meta):
    """One log as a Leg, or None."""
    t0, a = log["topics"][0], log["address"].lower()
    tx, block = log["transactionHash"], int(log["blockNumber"], 16)
    try:
        w = _words(log["data"])
        d = {}
        if t0 == TESSERA:
            if a != TESSERA_SWAP:
                return None
            _add(d, _addr(w[0]), int(w[2], 16))
            _add(d, _addr(w[1]), -int(w[3], 16))
            return Leg(tx, block, "tessera", a, d)
        if t0 == ELFOMO:
            if a != ELFOMO_SWAP:
                return None
            _add(d, _addr(w[2]), int(w[4], 16))
            _add(d, _addr(w[3]), -int(w[5], 16))
            return Leg(tx, block, "elfomofi", a, d)
        if t0 in (BAL_V2, BAL_V3):
            key = pool_key(log)
            if key is None:
                return None
            _add(d, _addr(log["topics"][2]), int(w[0], 16))
            _add(d, _addr(log["topics"][3]), -int(w[1], 16))
            return Leg(tx, block, "balancer_v2" if t0 == BAL_V2 else "balancer_v3", key, d)
        key = pool_key(log)
        m = meta.get(key) if key else None
        if not m:
            return None
        if t0 == CURVE_I128 or t0 == CURVE_U256:
            coins = m["coins"]
            sold, bought = int(w[0], 16), int(w[2], 16)
            if sold >= len(coins) or bought >= len(coins):
                return None
            _add(d, coins[sold], int(w[1], 16))
            _add(d, coins[bought], -int(w[3], 16))
            return Leg(tx, block, m["venue"], key, d)
        if t0 == HANJI_ORDER:
            # topics: owner, initiator, isAsk; data: order_id, quantity, price,
            # passive_shares, passive_fee, aggressive_shares, aggressive_value,
            # aggressive_fee, market_only, post_only. The taker's fill is the
            # aggressive part; verification decides whether this reading counts.
            shares, value, fee = int(w[5], 16), int(w[6], 16), int(w[7], 16)
            if shares == 0:
                return None
            is_ask = int(log["topics"][3], 16) == 1
            x, y = shares * m["sx"], value * m["sy"]
            if is_ask:
                _add(d, m["x"], x)
                _add(d, m["y"], -(y - fee * m["sy"]))
            else:
                _add(d, m["y"], y + fee * m["sy"])
                _add(d, m["x"], -x)
            return Leg(tx, block, "hanji", key, d)
        tok0, tok1, venue = m[0], m[1], m[2]
        if t0 in (UNI_V3, PANCAKE_V3, ALGEBRA_INTEGRAL):
            _add(d, tok0, _s256(w[0]))
            _add(d, tok1, _s256(w[1]))
        elif t0 == UNI_V4:
            _add(d, tok0, -_s256(w[0]))
            _add(d, tok1, -_s256(w[1]))
        elif t0 in (UNI_V2, AERO_V2):
            _add(d, tok0, int(w[0], 16) - int(w[2], 16))
            _add(d, tok1, int(w[1], 16) - int(w[3], 16))
        elif t0 == MAVERICK_V2:
            # sender, recipient, (amount, tokenAIn, exactOutput, tickLimit), amountIn, amountOut
            a_in = int(w[3], 16) == 1
            ain, aout = int(w[6], 16), int(w[7], 16)
            _add(d, tok0 if a_in else tok1, ain)
            _add(d, tok1 if a_in else tok0, -aout)
        elif t0 == FLUID:
            zero_for_one = int(w[0], 16) == 1
            _add(d, tok0 if zero_for_one else tok1, int(w[1], 16))
            _add(d, tok1 if zero_for_one else tok0, -int(w[2], 16))
        elif t0 == METRIC:
            # sender, recipient, exactInput, amount0Delta, amount1Delta: the
            # pool's side, as Uniswap v3's (verification checks the sign).
            _add(d, tok0, _s256(w[3]))
            _add(d, tok1, _s256(w[4]))
        else:
            return None
        return Leg(tx, block, venue, key, d)
    except (IndexError, ValueError, KeyError, TypeError):
        return None
```

- [ ] **Step 4: Run the tests**

Run: `python3 -m unittest arb_census.test_venues`
Expected: 10 tests OK.

---

### Task 3: Detection and banked value

**Files:**
- Create: `scripts/data/arb_census/detect.py`, `scripts/data/arb_census/test_detect.py`

**Interfaces:**
- Consumes: `venues.Leg`, `venues.decode`, `venues.TRANSFER`, `venues.WETH`.
- Produces: `trader_change(legs) -> dict[str, int]`; `is_arbitrage(legs) -> bool`; `banked(receipt) -> dict[str, int]`; `gas_wei(receipt) -> int`; `legs_of(receipt, meta) -> list[Leg]`.

- [ ] **Step 1: Write the failing tests** (the fixture tests run once Task 5 has recorded `fixtures/`; until then they are skipped):

```python
import json, unittest
from pathlib import Path
from arb_census import detect
from arb_census.venues import Leg, WETH

FIX = Path(__file__).resolve().parent / "fixtures"
T = "0x" + "33" * 20


def fixture(name):
    p = FIX / f"{name}.json"
    if not p.exists():
        raise unittest.SkipTest(f"fixture {name} not recorded yet")
    return json.loads(p.read_text())


class ClosureTest(unittest.TestCase):
    def test_a_closed_cycle_with_a_gain_is_an_arbitrage(self):
        legs = [Leg("0x1", 1, "a", "p", {WETH: 100, T: -50}), Leg("0x1", 1, "b", "q", {T: 50, WETH: -103})]
        self.assertEqual(detect.trader_change(legs), {WETH: 3, T: 0})
        self.assertTrue(detect.is_arbitrage(legs))

    def test_spending_a_token_is_not_an_arbitrage(self):
        legs = [Leg("0x1", 1, "a", "p", {WETH: 100, T: -50}), Leg("0x1", 1, "b", "q", {T: 49, WETH: -101})]
        self.assertFalse(detect.is_arbitrage(legs))

    def test_rounding_dust_is_not_spending(self):
        # WETH: 10 spent of ~2,000,000 flow (0.0005%) is dust; T: 10 gained.
        legs = [Leg("0x1", 1, "a", "p", {WETH: 1_000_000, T: -60}), Leg("0x1", 1, "b", "q", {T: 50, WETH: -999_990})]
        self.assertTrue(detect.is_arbitrage(legs))

    def test_one_leg_is_never_an_arbitrage(self):
        self.assertFalse(detect.is_arbitrage([Leg("0x1", 1, "a", "p", {WETH: -1, T: 5})]))


class FixtureTest(unittest.TestCase):
    def test_the_855_spike_trade_is_an_arbitrage(self):
        f = fixture("spike_855")
        self.assertTrue(detect.is_arbitrage(detect.legs_of(f["receipt"], f["meta"])))

    def test_a_users_route_is_not(self):
        f = fixture("user_route")
        self.assertFalse(detect.is_arbitrage(detect.legs_of(f["receipt"], f["meta"])))

    def test_the_v4_trade_is_an_arbitrage(self):
        f = fixture("v4_2hop")
        self.assertTrue(detect.is_arbitrage(detect.legs_of(f["receipt"], f["meta"])))

    def test_the_omi_trade_banked_5_92_weth(self):
        f = fixture("omi_10hop")
        self.assertEqual(detect.banked(f["receipt"]).get(WETH), 5921809260898424195)

    def test_gas_includes_the_l1_fee(self):
        f = fixture("spike_855")
        r = f["receipt"]
        expected = int(r["gasUsed"], 16) * int(r["effectiveGasPrice"], 16) + int(r.get("l1Fee", "0x0"), 16)
        self.assertEqual(detect.gas_wei(r), expected)


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `python3 -m unittest arb_census.test_detect`
Expected: `ImportError` (no `detect`).

- [ ] **Step 3: Write `detect.py`**

```python
"""Is a transaction an arbitrage, and what did it bank? (R23)

Closure is judged on decoded legs; value on the receipt's transfers, which
also catch legs on venues no decoder reads.
"""
from arb_census.venues import TRANSFER, decode

# Spending under this share of a token's flow is rounding, not spending.
DUST = 1e-4


def trader_change(legs):
    """What the trader gained (+) or spent (-) of each token: the opposite
    of the venues' side, summed."""
    out = {}
    for leg in legs:
        for t, x in leg.deltas.items():
            out[t] = out.get(t, 0) - x
    return out


def is_arbitrage(legs):
    if len(legs) < 2:
        return False
    change = trader_change(legs)
    flow = {}
    for leg in legs:
        for t, x in leg.deltas.items():
            flow[t] = flow.get(t, 0) + abs(x)
    spent = any(x < 0 and -x > flow[t] * DUST for t, x in change.items())
    return not spent and any(x > 0 for x in change.values())


def legs_of(receipt, meta):
    out = []
    for log in receipt["logs"]:
        if not log["topics"]:
            continue
        log = dict(log, transactionHash=receipt["transactionHash"], blockNumber=receipt["blockNumber"])
        leg = decode(log, meta)
        if leg:
            out.append(leg)
    return out


def banked(receipt):
    """Net ERC-20 transfers to the sender and its target: what the bot kept."""
    who = {receipt["from"].lower(), (receipt.get("to") or "").lower()}
    net = {}
    for log in receipt["logs"]:
        t = log["topics"]
        if len(t) != 3 or t[0] != TRANSFER or log["data"] in ("0x", ""):
            continue
        src, dst = "0x" + t[1][-40:], "0x" + t[2][-40:]
        token, x = log["address"].lower(), int(log["data"][:66], 16)
        if dst in who and src not in who:
            net[token] = net.get(token, 0) + x
        elif src in who and dst not in who:
            net[token] = net.get(token, 0) - x
    return {k: v for k, v in net.items() if v}


def gas_wei(receipt):
    return int(receipt["gasUsed"], 16) * int(receipt.get("effectiveGasPrice", "0x0"), 16) + int(
        receipt.get("l1Fee") or "0x0", 16
    )
```

- [ ] **Step 4: Run the tests**

Run: `python3 -m unittest arb_census.test_detect`
Expected: 4 closure tests OK, 5 fixture tests skipped (until Task 5 records them).

---

### Task 4: Decoder verification

**Files:**
- Create: `scripts/data/arb_census/verify.py`, `scripts/data/arb_census/test_verify.py`

**Interfaces:**
- Consumes: `detect.legs_of`, `venues.TRANSFER`, `venues.decode`.
- Produces: `check_leg(leg, receipt) -> bool`; `verify_venue(rpc, venue, txs, meta) -> dict` (`{"venue", "checked", "passed", "failures": [tx...]}`); CLI `python3 -m arb_census.verify --venue NAME --topic T [--address A] [--blocks N]` appending to `data/census/coverage.json`.

The check: for a leg whose venue holds its tokens at the emitting address (`uniswap_v3`-style pools, `uniswap_v2`/`aerodrome_v2`, Maverick, Curve, Metric, Fluid is excluded: its liquidity layer holds the funds), the pool's net `Transfer` change of each token in the receipt equals the decoded delta, sign included. For the others (Uniswap v4, Balancer, Tessera, ElfomoFi, Fluid, Hanji), each decoded amount's absolute value appears as the value of some `Transfer` of that token in the receipt (native ETH legs excepted).

- [ ] **Step 1: Write the failing tests**

```python
import json, unittest
from pathlib import Path
from arb_census import detect, verify

FIX = Path(__file__).resolve().parent / "fixtures"


def fixture(name):
    p = FIX / f"{name}.json"
    if not p.exists():
        raise unittest.SkipTest(f"fixture {name} not recorded yet")
    return json.loads(p.read_text())


class VerifyTest(unittest.TestCase):
    def test_the_spike_trades_legs_match_their_pools_transfers(self):
        f = fixture("spike_855")
        legs = detect.legs_of(f["receipt"], f["meta"])
        self.assertTrue(legs)
        self.assertTrue(all(verify.check_leg(l, f["receipt"]) for l in legs))

    def test_a_wrong_sign_fails(self):
        f = fixture("spike_855")
        leg = detect.legs_of(f["receipt"], f["meta"])[0]
        flipped = type(leg)(leg.tx, leg.block, leg.venue, leg.pool, {t: -x for t, x in leg.deltas.items()})
        self.assertFalse(verify.check_leg(flipped, f["receipt"]))

    def test_tessera_and_elfomo_amounts_appear_as_transfers(self):
        for name in ("tessera", "elfomo"):
            f = fixture(name)
            legs = detect.legs_of(f["receipt"], f["meta"])
            self.assertTrue(legs, name)
            self.assertTrue(all(verify.check_leg(l, f["receipt"]) for l in legs), name)


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `python3 -m unittest arb_census.test_verify`
Expected: `ImportError` (no `verify`).

- [ ] **Step 3: Write `verify.py`**

```python
"""Does a venue's decoder read its trades right? Checked against the
receipt's own transfers before the venue counts in the census (R23)."""
import argparse
import json
import random
from pathlib import Path

from arb_census.detect import legs_of
from arb_census.venues import TRANSFER, WETH

# Venues whose tokens sit at the address that emits the trade event.
HOLDS_AT_EMITTER = {"uniswap_v3", "pancakeswap_v3", "slipstream_5e7b", "slipstream_f8f2", "cl_ade6",
                    "uniswap_v2", "aerodrome_v2", "maverick_v2", "curve", "metric", "quickswap"}


def _transfers(receipt):
    for log in receipt["logs"]:
        t = log["topics"]
        if len(t) == 3 and t[0] == TRANSFER and log["data"] not in ("0x", ""):
            yield log["address"].lower(), "0x" + t[1][-40:], "0x" + t[2][-40:], int(log["data"][:66], 16)


def check_leg(leg, receipt):
    if leg.venue in HOLDS_AT_EMITTER or leg.venue.startswith("factory:"):
        pool = leg.pool.lower()
        net = {}
        for token, src, dst, x in _transfers(receipt):
            if dst == pool:
                net[token] = net.get(token, 0) + x
            if src == pool:
                net[token] = net.get(token, 0) - x
        return all(net.get(t, 0) == x for t, x in leg.deltas.items())
    seen = {}
    for token, _src, _dst, x in _transfers(receipt):
        seen.setdefault(token, set()).add(x)
    return all(abs(x) in seen.get(t, set()) or (t == WETH and x != 0 and not seen.get(t)) for t, x in leg.deltas.items() if x)


def verify_venue(rpc, venue, txs, meta):
    checked, passed, failures = 0, 0, []
    for tx in txs:
        receipt = rpc.call("eth_getTransactionReceipt", [tx])
        legs = [l for l in legs_of(receipt, meta) if l.venue == venue]
        if not legs:
            continue
        checked += 1
        if all(check_leg(l, receipt) for l in legs):
            passed += 1
        else:
            failures.append(tx)
    return {"venue": venue, "checked": checked, "passed": passed, "failures": failures[:5]}


def main():
    from arb_census.collect import Resolver
    from arb_census.rpc import Rpc
    ap = argparse.ArgumentParser()
    ap.add_argument("--venue", required=True)
    ap.add_argument("--topic", required=True)
    ap.add_argument("--address")
    ap.add_argument("--blocks", type=int, default=3000)
    ap.add_argument("--sample", type=int, default=12)
    ap.add_argument("--out", default=str(Path(__file__).resolve().parents[3] / "data" / "census" / "coverage.json"))
    a = ap.parse_args()
    rpc = Rpc()
    head = int(rpc.call("eth_blockNumber", []), 16)
    q = {"fromBlock": hex(head - a.blocks), "toBlock": hex(head), "topics": [a.topic]}
    if a.address:
        q["address"] = a.address
    logs = []
    lo = head - a.blocks
    while lo <= head:
        hi = min(lo + 499, head)
        logs += rpc.call("eth_getLogs", [dict(q, fromBlock=hex(lo), toBlock=hex(hi))])
        lo = hi + 1
    meta = Resolver(rpc).meta_for_logs(logs)
    txs = sorted({l["transactionHash"] for l in logs})
    random.Random(7).shuffle(txs)
    result = verify_venue(rpc, a.venue, txs[: a.sample], meta)
    result["counts"] = result["checked"] >= 3 and result["passed"] == result["checked"]
    out = Path(a.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    cov = json.loads(out.read_text()) if out.exists() else {}
    cov[a.venue] = result
    out.write_text(json.dumps(cov, indent=1))
    print(json.dumps(result))


if __name__ == "__main__":
    main()
```

- [ ] **Step 4: Run the tests**

Run: `python3 -m unittest arb_census.test_verify`
Expected: skipped until Task 5 records the fixtures; re-run then.

---

### Task 5: Pool metadata, collection, and the fixtures

**Files:**
- Create: `scripts/data/arb_census/collect.py`
- Data (committed): `scripts/data/arb_census/fixtures/*.json`

**Interfaces:**
- Consumes: `rpc.Rpc`, `venues.*`, `detect.*`.
- Produces: `Resolver(rpc, cache_path=None)` with `.meta_for_logs(logs) -> dict` (fills and returns metadata for every pool key the logs need; caches to `data/census/meta.json`); `collect_day(rpc, resolver, lo, hi) -> dict` (the day file's content); CLI `python3 -m arb_census.collect --days 7 [--end BLOCK] [--out DIR] [--rate R]`.

Day file `data/census/day-<lo>-<hi>.json.gz`: `{"lo", "hi", "eth_usd", "prices": {token: usd}, "decimals": {token: n}, "symbols": {token: str}, "logs": n, "arbs": [...]}`, each arb `{"tx", "block", "venues": [..], "pools": [..], "pairs": [[t0, t1], ..], "hops", "est_usd", "banked": {token: str(int)}, "banked_usd", "valued_by": "banked"|"pool", "gas_usd", "sender", "target", "mm": bool}`.

- [ ] **Step 1: Write `collect.py`**

```python
"""Collect a week of Base arbitrage, one day at a time (R23).

For each day: every decodable trade log, grouped by transaction; closure
tested on the decoded legs; receipts read for arbitrages worth $1 or more
and every arbitrage touching a market-maker venue, for what was banked and
the gas. Resumable: a day whose file exists is skipped.
"""
import argparse
import gzip
import json
import statistics
from pathlib import Path

from arb_census import detect
from arb_census import venues as V
from arb_census.rpc import Rpc, RpcError

REPO = Path(__file__).resolve().parents[3]
DATA = REPO / "data" / "census"
BLOCKS_PER_DAY = 43_200
RECEIPT_FLOOR_USD = 1.0
DEC = {V.WETH: 18, V.USDC: 6, "0xd9aaec86b65d86f6a7b5b1b0c42ffa531710b6ca": 6,
       "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf": 8}
POSITION_MANAGER = "0x7c5f5a4bbd8fd63184577525326123b519429bdc"
MAVERICK_FACTORY = "0x0a7e848aca42d879ef06507fca0e7b33a0a63c1e"
FLUID_RESOLVER = "0x160ffc75904515f38c9b7ed488e1f5a43ce71eba"
METRIC_FACTORY = "0xe22f9fc0f04486de25ed6cf1800a4a47afd82e0c"
METRIC_FROM = 42_570_144
METRIC_CREATED = "0xe1c304acd9cda85cdced26da15edf3fd1a07d96caf4f9cedb068823618b93d4d"
HANJI_FACTORY = "0xc7264db7c78dd418632b73a415595c7930a9eea4"
HANJI_FROM = 37_860_153
HANJI_CREATED = "0x04b3d813686f2d1061e11e5a0d93455065f32174906b0c037cf1176818af86be"


def _addr(h):
    return "0x" + h[-40:].lower() if h and len(h) >= 42 else None


def _words(h):
    d = h[2:] if h else ""
    return [d[i:i + 64] for i in range(0, len(d), 64)]


class Resolver:
    """Pool keys to tokens and venue; venue pool sets discovered once."""

    def __init__(self, rpc, cache_path=None):
        self.rpc = rpc
        self.path = Path(cache_path) if cache_path else DATA / "meta.json"
        self.meta = json.loads(self.path.read_text()) if self.path.exists() else {}
        self._sets = None

    def save(self):
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.path.write_text(json.dumps(self.meta))

    def _scan(self, address, topic, lo):
        head = int(self.rpc.call("eth_blockNumber", []), 16)
        out, b, step = [], lo, 10_000
        while b <= head:
            e = min(b + step - 1, head)
            try:
                out += self.rpc.call("eth_getLogs", [{"address": address, "topics": [topic], "fromBlock": hex(b), "toBlock": hex(e)}])
            except RpcError:
                if step > 500:
                    step //= 2
                    continue
                raise
            b = e + 1
        return out

    def venue_sets(self):
        """Maverick, Fluid, Metric and Hanji pools, discovered from their
        factories once and cached in the metadata."""
        if self._sets is not None:
            return self._sets
        if "_sets" in self.meta:
            self._sets = self.meta["_sets"]
            return self._sets
        sets = {"maverick_v2": {}, "fluid": {}, "metric": {}, "hanji": {}}
        n = int(self.rpc.call("eth_call", [{"to": MAVERICK_FACTORY, "data": "0xf525cb68"}, "latest"]), 16)
        for start in range(0, n, 200):
            r = self.rpc.call("eth_call", [{"to": MAVERICK_FACTORY, "data": "0xb4b9d1f1" + format(start, "064x") + format(min(start + 200, n), "064x")}, "latest"])
            w = _words(r)
            pools = [_addr(x) for x in w[2:2 + int(w[1], 16)]]
            toks = self.rpc.batch([c for p in pools for c in (("eth_call", [{"to": p, "data": "0x0fc63d10"}, "latest"]), ("eth_call", [{"to": p, "data": "0x5f64b55b"}, "latest"]))])
            for i, p in enumerate(pools):
                a, b = _addr(toks[2 * i]), _addr(toks[2 * i + 1])
                if a and b:
                    sets["maverick_v2"][p] = [a, b, "maverick_v2"]
        r = self.rpc.call("eth_call", [{"to": FLUID_RESOLVER, "data": "0xd88ff1f4"}, "latest"])
        w = _words(r)
        count = int(w[1], 16)
        for i in range(count):
            p, t0, t1 = _addr(w[2 + 4 * i]), _addr(w[3 + 4 * i]), _addr(w[4 + 4 * i])
            sets["fluid"][p] = [t0, t1, "fluid"]
        for log in self._scan(METRIC_FACTORY, METRIC_CREATED, METRIC_FROM):
            sets["metric"][_addr(_words(log["data"])[0])] = [_addr(log["topics"][1]), _addr(log["topics"][2]), "metric"]
        for log in self._scan(HANJI_FACTORY, HANJI_CREATED, HANJI_FROM):
            w = _words(log["data"])
            book = _addr(w[0])
            cfg = _words(self.rpc.call("eth_call", [{"to": book, "data": "0xc3f909d4"}, "latest"]))
            sets["hanji"][book] = {"x": _addr(w[1]), "y": _addr(w[2]), "sx": int(cfg[0], 16), "sy": int(cfg[1], 16), "venue": "hanji"}
        self.meta["_sets"] = self._sets = sets
        self.save()
        return sets

    def meta_for_logs(self, logs):
        sets = self.venue_sets()
        need_pool, need_v4, need_curve = set(), set(), set()
        for log in logs:
            if not log["topics"]:
                continue
            t0, key = log["topics"][0], V.pool_key(log)
            if key is None or key in self.meta:
                continue
            if t0 == V.MAVERICK_V2:
                if key in sets["maverick_v2"]:
                    self.meta[key] = sets["maverick_v2"][key]
            elif t0 == V.FLUID:
                if key in sets["fluid"]:
                    self.meta[key] = sets["fluid"][key]
            elif t0 == V.METRIC:
                if key in sets["metric"]:
                    self.meta[key] = sets["metric"][key]
            elif t0 == V.HANJI_ORDER:
                if key in sets["hanji"]:
                    self.meta[key] = sets["hanji"][key]
            elif t0 == V.UNI_V4:
                need_v4.add(key)
            elif t0 in (V.CURVE_I128, V.CURVE_U256):
                need_curve.add(key)
            elif t0 in (V.UNI_V3, V.PANCAKE_V3, V.ALGEBRA_INTEGRAL, V.UNI_V2, V.AERO_V2):
                need_pool.add(key)
        pools = sorted(need_pool)
        res = self.rpc.batch([c for p in pools for c in (
            ("eth_call", [{"to": p, "data": "0x0dfe1681"}, "latest"]),
            ("eth_call", [{"to": p, "data": "0xd21220a7"}, "latest"]),
            ("eth_call", [{"to": p, "data": "0xc45a0155"}, "latest"]))])
        for i, p in enumerate(pools):
            t0, t1, f = _addr(res[3 * i]), _addr(res[3 * i + 1]), _addr(res[3 * i + 2])
            if t0 and t1:
                self.meta[p] = [t0, t1, V.VENUE_OF_FACTORY.get(f, f"factory:{f}" if f else "nofactory")]
        ids = sorted(need_v4)
        res = self.rpc.batch([("eth_call", [{"to": POSITION_MANAGER, "data": "0x86b6be7d" + i[2:52] + "0" * 14}, "latest"]) for i in ids])
        for i, r in zip(ids, res):
            w = _words(r)
            if len(w) >= 5 and (int(w[0], 16) or int(w[1], 16)):
                self.meta[i] = [_addr(w[0]), _addr(w[1]), "uniswap_v4"]
        for p in sorted(need_curve):
            coins = []
            for sel in ("0xc6610657", "0x23746eb8"):
                for k in range(8):
                    try:
                        c = _addr(self.rpc.call("eth_call", [{"to": p, "data": sel + format(k, "064x")}, "latest"]))
                    except RpcError:
                        break
                    if not c or c == V.ZERO:
                        break
                    coins.append(c)
                if coins:
                    break
            if len(coins) >= 2:
                self.meta[p] = {"coins": coins, "venue": "curve"}
        self.save()
        return {k: self.meta[k] for k in {V.pool_key(l) for l in logs if l["topics"]} if k in self.meta}


def _prices(rpc, legs):
    """USD prices from the day's own trades: WETH from WETH/USDC, others
    from their trades against WETH or USDC, three observations at least."""
    obs = {}
    for leg in legs:
        if len(leg.deltas) != 2:
            continue
        (ta, xa), (tb, xb) = leg.deltas.items()
        if xa and xb:
            obs.setdefault(ta, []).append((tb, abs(xa), abs(xb)))
            obs.setdefault(tb, []).append((ta, abs(xb), abs(xa)))
    usd = {V.USDC: 1.0, "0xd9aaec86b65d86f6a7b5b1b0c42ffa531710b6ca": 1.0}
    weth = [b / 1e6 / (a / 1e18) for (o, a, b) in obs.get(V.WETH, []) if o == V.USDC]
    usd[V.WETH] = statistics.median(weth) if weth else 0.0
    others = [t for t in obs if t not in usd]
    decs = rpc.batch([("eth_call", [{"to": t, "data": "0x313ce567"}, "latest"]) for t in others])
    dec = dict(DEC)
    for t, h in zip(others, decs):
        try:
            n = int(h, 16)
            if 0 < n <= 36:
                dec[t] = n
        except (TypeError, ValueError):
            pass
    for t in others:
        if t not in dec:
            continue
        xs = [ao / 10 ** dec[o] * usd[o] / (ax / 10 ** dec[t]) for (o, ax, ao) in obs[t]
              if o in (V.WETH, V.USDC) and usd.get(o) and o in dec]
        if len(xs) >= 3:
            usd[t] = statistics.median(xs)
    return usd, dec


def _usd(amounts, usd, dec):
    total, unvalued = 0.0, False
    for t, x in amounts.items():
        if t in usd and t in dec:
            total += x / 10 ** dec[t] * usd[t]
        elif x > 0:
            unvalued = True
    return total, unvalued


def collect_day(rpc, resolver, lo, hi):
    logs_seen, by_tx = 0, {}
    b, step = lo, 50
    while b <= hi:
        e = min(b + step - 1, hi)
        try:
            logs = rpc.call("eth_getLogs", [{"fromBlock": hex(b), "toBlock": hex(e), "topics": [list(V.ALL_TOPICS)]}])
        except RpcError:
            if step > 5:
                step //= 2
                continue
            raise
        logs_seen += len(logs)
        chunk = {}
        for log in logs:
            chunk.setdefault(log["transactionHash"], []).append(log)
        keep = {t: v for t, v in chunk.items() if len(v) >= 2}
        if keep:
            meta = resolver.meta_for_logs([l for v in keep.values() for l in v])
            for t, v in keep.items():
                legs = [x for x in (V.decode(l, meta) for l in v) if x]
                if legs:
                    by_tx[t] = legs
        b = e + 1
        step = min(step * 2, 50)
    all_legs = [l for legs in by_tx.values() for l in legs]
    usd, dec = _prices(rpc, all_legs)
    arbs = []
    for t, legs in by_tx.items():
        if not detect.is_arbitrage(legs):
            continue
        change = {k: x for k, x in detect.trader_change(legs).items() if x > 0}
        est, _ = _usd(change, usd, dec)
        venues = sorted({l.venue for l in legs})
        arbs.append({"tx": t, "block": legs[0].block, "venues": venues, "pools": sorted({l.pool for l in legs}),
                     "pairs": sorted({tuple(sorted(l.deltas)) for l in legs if len(l.deltas) == 2}),
                     "hops": len(legs), "est_usd": est, "mm": bool(set(venues) & V.MARKET_MAKERS)})
    want = [a for a in arbs if a["est_usd"] >= RECEIPT_FLOOR_USD or a["mm"]]
    receipts = rpc.batch([("eth_getTransactionReceipt", [a["tx"]]) for a in want])
    for a, r in zip(want, receipts):
        if not r:
            continue
        kept = detect.banked(r)
        value, unvalued = _usd(kept, usd, dec)
        a["banked"] = {k: str(x) for k, x in kept.items()}
        a["banked_usd"] = value
        a["valued_by"] = "banked" if kept and not unvalued else "pool"
        a["gas_usd"] = detect.gas_wei(r) / 1e18 * usd[V.WETH]
        a["sender"], a["target"] = r["from"].lower(), (r.get("to") or "").lower()
    toks = sorted({t for a in arbs for p in a["pairs"] for t in p})
    syms = rpc.batch([("eth_call", [{"to": t, "data": "0x95d89b41"}, "latest"]) for t in toks])
    symbols = {t: _text(s) for t, s in zip(toks, syms)}
    return {"lo": lo, "hi": hi, "eth_usd": usd[V.WETH], "prices": usd, "decimals": dec, "symbols": symbols,
            "logs": logs_seen, "arbs": [dict(a, pairs=[list(p) for p in a["pairs"]]) for a in arbs]}


def _text(h):
    try:
        b = bytes.fromhex(h[2:])
        if len(b) >= 96:
            n = int.from_bytes(b[32:64], "big")
            return b[64:64 + n].decode("utf-8", "replace")
        return b.rstrip(b"\0").decode("utf-8", "replace")
    except (TypeError, ValueError, AttributeError):
        return "?"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--days", type=int, default=7)
    ap.add_argument("--end", type=int, help="last block (default: the head)")
    ap.add_argument("--out", default=str(DATA))
    ap.add_argument("--rate", type=float, default=10.0, help="requests a second")
    a = ap.parse_args()
    rpc = Rpc(max_per_second=a.rate)
    resolver = Resolver(rpc, Path(a.out) / "meta.json")
    end = a.end or int(rpc.call("eth_blockNumber", []), 16)
    for d in range(a.days, 0, -1):
        hi = end - (d - 1) * BLOCKS_PER_DAY
        lo = hi - BLOCKS_PER_DAY + 1
        path = Path(a.out) / f"day-{lo}-{hi}.json.gz"
        if path.exists():
            print(f"day {lo}..{hi}: done before", flush=True)
            continue
        day = collect_day(rpc, resolver, lo, hi)
        path.parent.mkdir(parents=True, exist_ok=True)
        with gzip.open(path, "wt") as f:
            json.dump(day, f)
        print(f"day {lo}..{hi}: {day['logs']} logs, {len(day['arbs'])} arbitrages", flush=True)


if __name__ == "__main__":
    main()
```

- [ ] **Step 2: Record the fixtures**

Run: `python3 -m arb_census.record_fixtures` (the first run discovers the Maverick, Fluid, Metric and Hanji pool sets and caches them in `data/census/meta.json`).
Expected: six lines; `fixtures/` holds six JSON files.

- [ ] **Step 3: Run every test**

Run: `python3 -m unittest discover -s arb_census -p 'test_*.py'`
Expected: all pass, none skipped. If a fixture test fails, the decoder or the fixture is wrong: inspect the receipt before changing any expectation.

- [ ] **Step 4: Smoke-run 300 blocks** into a scratch directory:

Run `collect_day` directly for the last 300 blocks:

```bash
python3 -c "
from arb_census.collect import Resolver, collect_day
from arb_census.rpc import Rpc
r = Rpc(); h = int(r.call('eth_blockNumber', []), 16)
d = collect_day(r, Resolver(r), h - 300, h)
print(d['logs'], len(d['arbs']), round(d['eth_usd']), sum(a.get('banked_usd') or a['est_usd'] for a in d['arbs']))
print(sorted({v for a in d['arbs'] for v in a['venues']}))
"
```

Expected: thousands of logs, hundreds of arbitrages, an ETH price near the market's, and venue names including `uniswap_v4` and at least one of `tessera`/`elfomofi` only if an arbitrage touched them.

---

### Task 6: Verify every added venue

**Files:**
- Data: `data/census/coverage.json`

- [ ] **Step 1: Run verification for each venue** (12 sampled transactions each, the last 3,000 blocks; for a quiet venue raise `--blocks` to 43,200):

```bash
python3 -m arb_census.verify --venue tessera --topic 0x97ba0cd8ff13f074b3b1aeace7fa3bf7fe54bdf2d728b6a097e901073b2bad6a --address 0x55555522005BcAE1c2424D474BfD5ed477749E3e
python3 -m arb_census.verify --venue elfomofi --topic 0xbe65a3f1f381da16732df786f571604a72b7c122cff3ae2b355566ddf01e2528 --address 0xf0f0F0F0FB0d738452EfD03A28e8be14C76d5f73
python3 -m arb_census.verify --venue maverick_v2 --topic 0x103ed084e94a44c8f5f6ba8e3011507c41063177e29949083c439777d8d63f60 --blocks 43200
python3 -m arb_census.verify --venue fluid --topic 0xdc004dbca4ef9c966218431ee5d9133d337ad018dd5b5c5493722803f75c64f7 --blocks 43200
python3 -m arb_census.verify --venue curve --topic 0x8b3e96f2b889fa771c53c981b40daf005f63f637f1869f707052d15a3dd97140 --blocks 43200
python3 -m arb_census.verify --venue curve --topic 0xb2e76ae99761dc136e598d4a629bb347eccb9532a5f8bbd72e18467c3c34cc98 --blocks 43200
python3 -m arb_census.verify --venue metric --topic 0xcd8b75a7fb6cb82ab3acded68ac53af5c43d19b51a91b9d5640bb73622dbdf57 --blocks 43200
python3 -m arb_census.verify --venue hanji --topic 0x1047930a29721c0069210227eeb58936b0a4667fe473b91765b069d71b1e3d7d
```

Expected for each: `checked >= 3`, `passed == checked`, `counts: true`. Record every result.

- [ ] **Step 2: Resolve what fails, without guessing.** For a venue that fails or has no traffic:
  - **Metric with no logs:** list the Metric pools in `data/census/meta.json` `_sets.metric`, read one pool's recent logs without a topic filter (`eth_getLogs` by address, last 43,200 blocks) and print their `topics[0]`. If its swap topic differs, add it as a constant beside `METRIC`, decode it by reading one receipt's `Transfer` logs against its data words, and re-run verification.
  - **Hanji failing:** print one failing receipt's `Transfer` logs beside the decoded leg; adjust only the scaling or fee term the transfers show, and re-run.
  - **QuickSwap:** find its Base pools on GeckoTerminal (`curl -s https://api.geckoterminal.com/api/v2/networks/base/dexes?page=1` and following pages; take the QuickSwap dex ids, then `.../dexes/<id>/pools`), read a pool's `factory()` and recent `topics[0]`. If its topic is the Uniswap v3 one, add its factory to `VENUE_OF_FACTORY` as `"quickswap"`; if it is `ALGEBRA_INTEGRAL`, the same, and verify with that topic.
  - A venue still failing after one evidence-based change is recorded as not covered, with the failing transactions, and is left out of the counts (its legs decode to nothing: remove its topic from `ALL_TOPICS`).

- [ ] **Step 3: Re-run the tests** after any change: `python3 -m unittest discover -s arb_census -p 'test_*.py'`.

---

### Task 7: The report

**Files:**
- Create: `scripts/data/arb_census/report.py`, `scripts/data/arb_census/test_report.py`

**Interfaces:**
- Consumes: day files from Task 5, `coverage.json` from Task 6, `venues.OURS`, `venues.MARKET_MAKERS`.
- Produces: `value(arb) -> float`; `spike_hours(arbs, lo, hi) -> set[int]`; `venue_table(arbs, spikes) -> list[dict]`; `unlock_table(arbs, spikes, hours) -> list[dict]`; `market_maker_table(arbs) -> list[dict]`; `pair_table(arbs, symbols, spikes) -> list[dict]`; CLI `python3 -m arb_census.report [--dir data/census]` writing `report-<lo>-<hi>.md` and `.json`.

- [ ] **Step 1: Write the failing tests**

```python
import unittest
from arb_census import report as R

H = 1800  # blocks an hour


def arb(block, usd, venues, sender="0xs", pairs=(("0xa", "0xb"),), mm=False):
    return {"tx": f"0x{block}{usd}", "block": block, "venues": list(venues), "pools": [], "pairs": [list(p) for p in pairs],
            "hops": 2, "est_usd": usd, "banked_usd": usd, "valued_by": "banked", "gas_usd": 0.01, "sender": sender, "mm": mm}


class ReportTest(unittest.TestCase):
    def test_value_prefers_what_was_banked(self):
        self.assertEqual(R.value({"est_usd": 3.0, "banked_usd": 15.0, "valued_by": "banked"}), 15.0)
        self.assertEqual(R.value({"est_usd": 3.0, "banked_usd": 0.0, "valued_by": "pool"}), 3.0)
        self.assertEqual(R.value({"est_usd": 3.0}), 3.0)

    def test_a_spike_hour_is_five_times_the_median(self):
        arbs = [arb(h * H + 1, 10.0, ["uniswap_v3"]) for h in range(10)] + [arb(5 * H + 2, 60.0, ["uniswap_v3"])]
        self.assertEqual(R.spike_hours(arbs, 0, 10 * H - 1), {5})

    def test_unlock_counts_arbitrage_needing_only_that_venue(self):
        arbs = [arb(1, 10.0, ["uniswap_v3", "uniswap_v4"]), arb(2, 5.0, ["uniswap_v3"]), arb(3, 7.0, ["uniswap_v4", "curve"])]
        rows = {tuple(r["add"]): r for r in R.unlock_table(arbs, set(), 1)}
        self.assertEqual(rows[("uniswap_v4",)]["gross"], 10.0)
        self.assertEqual(rows[("curve", "uniswap_v4")]["gross"], 17.0)

    def test_market_maker_rows_show_the_top_three_share(self):
        arbs = [arb(1, 90.0, ["tessera", "uniswap_v3"], sender="0x1", mm=True), arb(2, 10.0, ["tessera", "uniswap_v3"], sender="0x2", mm=True)]
        row = [r for r in R.market_maker_table(arbs) if r["venue"] == "tessera"][0]
        self.assertEqual((row["arbs"], row["gross"], row["top3_share"]), (2, 100.0, 1.0))

    def test_pairs_count_the_venues_they_were_traded_on(self):
        arbs = [arb(1, 4.0, ["uniswap_v3", "slipstream_5e7b"], pairs=(("0xa", "0xb"),)), arb(2, 1.0, ["curve"], pairs=(("0xa", "0xb"),))]
        row = R.pair_table(arbs, {"0xa": "AAA", "0xb": "BBB"}, set())[0]
        self.assertEqual((row["pair"], row["gross"], row["venues_seen"]), ("AAA/BBB", 5.0, 3))


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `python3 -m unittest arb_census.test_report`
Expected: `ImportError` (no `report`).

- [ ] **Step 3: Write `report.py`**

```python
"""Rank a census's arbitrage: by venue, by what each venue would add to
ours, against the market-maker venues, and by pair (R23)."""
import argparse
import gzip
import itertools
import json
import statistics
from pathlib import Path

from arb_census.venues import MARKET_MAKERS, OURS

BLOCKS_PER_HOUR = 1800
DATA = Path(__file__).resolve().parents[3] / "data" / "census"


def value(a):
    if a.get("valued_by") == "banked":
        return float(a.get("banked_usd") or 0.0)
    return float(a.get("est_usd") or 0.0)


def _hour(a, lo):
    return (a["block"] - lo) // BLOCKS_PER_HOUR


def spike_hours(arbs, lo, hi):
    n = (hi - lo) // BLOCKS_PER_HOUR + 1
    totals = [0.0] * n
    for a in arbs:
        h = _hour(a, lo)
        if 0 <= h < n:
            totals[h] += value(a)
    med = statistics.median(totals) if totals else 0.0
    return {h for h, t in enumerate(totals) if med > 0 and t > 5 * med}


def _q(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(p * len(xs)))] if xs else 0.0


def _row(arbs, spikes, lo):
    vals = [value(a) for a in arbs]
    spike = sum(value(a) for a in arbs if _hour(a, lo) in spikes)
    gas = sum(a.get("gas_usd") or 0.0 for a in arbs)
    return {"arbs": len(arbs), "gross": round(sum(vals), 2), "net": round(sum(vals) - gas, 2),
            "spike": round(spike, 2), "calm": round(sum(vals) - spike, 2),
            "p50": round(_q(vals, 0.5), 4), "p90": round(_q(vals, 0.9), 2),
            "bots": len({a.get("sender") for a in arbs if a.get("sender")})}


def venue_table(arbs, spikes, lo=0):
    by = {}
    for a in arbs:
        for v in a["venues"]:
            by.setdefault(v, []).append(a)
    return sorted(({"venue": v, **_row(x, spikes, lo)} for v, x in by.items()), key=lambda r: -r["gross"])


def unlock_table(arbs, spikes, hours, lo=0):
    """What adding venues to ours would make reachable: arbitrage needing
    exactly those venues beyond ours, alone and in combinations of up to 3."""
    need = {}
    for a in arbs:
        n = frozenset(a["venues"]) - OURS
        if n:
            need.setdefault(n, []).append(a)
    cands = sorted({v for n in need for v in n})
    rows = []
    for k in (1, 2, 3):
        for combo in itertools.combinations(cands, k):
            got = [a for n, xs in need.items() if n <= set(combo) for a in xs]
            if got:
                rows.append({"add": list(combo), **_row(got, spikes, lo), "calm_per_day": 0.0})
    calm_hours = max(1, hours - len(spikes))
    for r in rows:
        r["calm_per_day"] = round(r["calm"] / calm_hours * 24, 2)
    return sorted(rows, key=lambda r: -r["gross"])


def market_maker_table(arbs):
    rows = []
    for v in sorted(MARKET_MAKERS):
        xs = [a for a in arbs if v in a["venues"]]
        by_sender = {}
        for a in xs:
            by_sender[a.get("sender")] = by_sender.get(a.get("sender"), 0.0) + value(a)
        gross = sum(value(a) for a in xs)
        top3 = sum(sorted(by_sender.values(), reverse=True)[:3])
        rows.append({"venue": v, "arbs": len(xs), "gross": round(gross, 2),
                     "top3_share": round(top3 / gross, 3) if gross else 0.0})
    return rows


def pair_table(arbs, symbols, spikes, lo=0):
    by, venues = {}, {}
    for a in arbs:
        for p in {tuple(p) for p in a["pairs"]}:
            by.setdefault(p, []).append(a)
            venues.setdefault(p, set()).update(a["venues"])
    rows = []
    for p, xs in by.items():
        name = "/".join(symbols.get(t, t[:8]) for t in p)
        reconstructable = bool(venues[p] & (OURS | {"uniswap_v2", "aerodrome_v2", "uniswap_v4"}))
        rows.append({"pair": name, "tokens": list(p), **_row(xs, spikes, lo),
                     "venues_seen": len(venues[p]), "two_venues": len(venues[p]) >= 2,
                     "reconstructable_venue": reconstructable})
    return sorted(rows, key=lambda r: -r["gross"])


def _md_table(rows, cols):
    head = "| " + " | ".join(cols) + " |\n|" + "---|" * len(cols) + "\n"
    return head + "".join("| " + " | ".join(str(r.get(c, "")) for c in cols) + " |\n" for r in rows)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dir", default=str(DATA))
    a = ap.parse_args()
    d = Path(a.dir)
    days = [json.load(gzip.open(p, "rt")) for p in sorted(d.glob("day-*.json.gz"))]
    if not days:
        raise SystemExit("no day files")
    lo, hi = min(x["lo"] for x in days), max(x["hi"] for x in days)
    arbs = [a for x in days for a in x["arbs"]]
    symbols = {}
    for x in days:
        symbols.update(x["symbols"])
    hours = (hi - lo) // BLOCKS_PER_HOUR + 1
    spikes = spike_hours(arbs, lo, hi)
    cov_path = d / "coverage.json"
    coverage = json.loads(cov_path.read_text()) if cov_path.exists() else {}
    out = {"lo": lo, "hi": hi, "hours": hours, "spike_hours": sorted(spikes), "arbs": len(arbs),
           "gross": round(sum(value(x) for x in arbs), 2),
           "venues": venue_table(arbs, spikes, lo), "unlock": unlock_table(arbs, spikes, hours, lo)[:25],
           "market_makers": market_maker_table(arbs), "pairs": pair_table(arbs, symbols, spikes, lo)[:60],
           "coverage": coverage}
    stem = d / f"report-{lo}-{hi}"
    stem.with_suffix(".json").write_text(json.dumps(out, indent=1))
    md = [f"# Base arbitrage census, blocks {lo}–{hi}\n",
          f"{len(arbs):,} arbitrages, ${out['gross']:,.0f} gross over {hours} hours; spike hours: {len(spikes)}.\n",
          "\n## Venues\n", _md_table(out["venues"], ["venue", "arbs", "gross", "net", "calm", "spike", "p50", "p90", "bots"]),
          "\n## What adding venues would make reachable (beyond ours)\n",
          _md_table(out["unlock"], ["add", "arbs", "gross", "spike", "calm_per_day", "p50", "p90"]),
          "\n## Against the market-maker venues\n", _md_table(out["market_makers"], ["venue", "arbs", "gross", "top3_share"]),
          "\n## Pairs\n", _md_table(out["pairs"], ["pair", "arbs", "gross", "calm", "spike", "p50", "p90", "bots", "venues_seen", "two_venues", "reconstructable_venue"]),
          "\n## Coverage\n", _md_table([{"venue": k, **v} for k, v in coverage.items()], ["venue", "checked", "passed", "counts"])]
    stem.with_suffix(".md").write_text("".join(md))
    print(stem.with_suffix(".md"))


if __name__ == "__main__":
    main()
```

- [ ] **Step 4: Run the tests**

Run: `python3 -m unittest discover -s arb_census -p 'test_*.py'`
Expected: all pass.

---

### Task 8: Run the census, commit, report

**Files:**
- Modify: `PLAN.md` (R23 entry after R22)
- Memory: `base-mev-market-size.md`, `MEMORY.md`

- [ ] **Step 1: Commit the package** (gates first): run the scratchpad `gates.sh` (expected: only the two known-red scripts fail; nothing else changed in Rust, but `scripts/ci` scans everything, `secret_scan.sh` and `no_fabricated_venue_sources.sh` included). Then `git add scripts/data/arb_census/__init__.py scripts/data/arb_census/rpc.py scripts/data/arb_census/venues.py scripts/data/arb_census/detect.py scripts/data/arb_census/verify.py scripts/data/arb_census/collect.py scripts/data/arb_census/report.py scripts/data/arb_census/record_fixtures.py scripts/data/arb_census/test_*.py scripts/data/arb_census/fixtures` and commit `feat(data): a census of what Base arbitrage winners bank (Task 8.5 R23)`; check `git diff --stat` on those paths is empty and no `__pycache__` was staged.

- [ ] **Step 2: Run seven days in the background** (about 3–4 hours):

```bash
cd /home/scotty/arbot-main2/arbot-main-main/scripts/data && python3 -m arb_census.collect --days 7 --rate 10 > /home/scotty/arbot-main2/arbot-main-main/data/census/collect.log 2>&1
```

While it runs, check `scripts/shadow-status.sh` each hour: if the shadow's read failures rise above their pre-census count, stop the census by its pid and resume later with `--rate 5` (finished days are kept).

- [ ] **Step 3: Write the report:** `python3 -m arb_census.report`. Read it against the spec's questions: venues, the unlock table, the market-maker venues (and whether a few accounts dominate them), pairs against the gates, coverage.

- [ ] **Step 4: PLAN.md R23 entry** with the headline numbers, coverage, and what they decide; commit `docs(plan): the Base arbitrage census's first week (Task 8.5 R23)`.

- [ ] **Step 5: Memory.** Update `base-mev-market-size.md` with the week's figures and the decision they support; `MEMORY.md` line to match.

- [ ] **Step 6: Tell the operator** the findings and offer the report as a page.
