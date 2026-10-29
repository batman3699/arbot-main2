"""R37: the v4 inventory, folded from PoolManager's Initialize logs into the
records the runtime's loader reads (crates/apex-runtime/src/live/inventory.rs,
`load_v4`).

Run from scripts/data: python3 -m unittest test_v4_inventory
"""
import json
import os
import tempfile
import unittest
from pathlib import Path

import v4_inventory as V

AXIS = "0x5be75e3b0aff5d1a042a5363ae36acb1d6d755ea"
USDC = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"
AXIS_ID = "0xc19d91d99afc0332e5013a569c9721cd9ecb51a14c851f7a189a140950a627fa"
ALPHA = "0xb0b24b89db0dafbe43c5b40226b63a179f592880"

# The AXIS/USDC pool's Initialize, as Basescan's getLogs returns it (read
# 2026-10-28, again 2026-10-29).
AXIS_LOG = {
    "address": "0x498581ff718922c3f8e6a244956af099b2652b2b",
    "topics": [
        "0xdd466e674ea557f56295e2d0218a125ea4b4f0f6f3307b95f85e6110838d6438",
        AXIS_ID,
        "0x0000000000000000000000005be75e3b0aff5d1a042a5363ae36acb1d6d755ea",
        "0x000000000000000000000000833589fcd6edb6e08f4c7c32d4f71b54bda02913",
    ],
    "data": "0x0000000000000000000000000000000000000000000000000000000000000064"
            "0000000000000000000000000000000000000000000000000000000000000001"
            "000000000000000000000000b0b24b89db0dafbe43c5b40226b63a179f592880"
            "0000000000000000000000000000000000000000000003c0624630639848aa6a"
            "fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffb5395",
    "blockNumber": "0x325acbb",
    "logIndex": "0x205",
    "transactionHash": "0x224fc12f548f5398785b2e5daa798b04f25ff04f2699cdffa00eb5d7feebfb72",
}

AXIS_RECORD = {"pool_id": AXIS_ID, "currency0": AXIS, "currency1": USDC, "fee": 100, "tick_spacing": 1,
               "hooks": ALPHA, "init_block": 0x325acbb}


def lines(path):
    with open(path) as f:
        return [json.loads(l) for l in f if l.strip()]


class Fold(unittest.TestCase):
    def test_initialize_logs_become_pool_records(self):
        with tempfile.TemporaryDirectory() as d:
            out = os.path.join(d, "uniswap_v4", "pools.jsonl")
            self.assertEqual(V.fold([AXIS_LOG], out), (1, 0))
            self.assertEqual(lines(out), [AXIS_RECORD])

    def test_a_second_fold_changes_nothing(self):
        with tempfile.TemporaryDirectory() as d:
            out = os.path.join(d, "pools.jsonl")
            # A record already held keeps what else it carries: its depth.
            with open(out, "w") as f:
                f.write(json.dumps({**AXIS_RECORD, "hub_usd_liquidity": 1.2e6}) + "\n")
            before = Path(out).read_bytes()
            self.assertEqual(V.fold([AXIS_LOG, AXIS_LOG], out), (0, 0))
            self.assertEqual(Path(out).read_bytes(), before)

    def test_a_log_from_another_address_is_refused(self):
        with tempfile.TemporaryDirectory() as d:
            out = os.path.join(d, "pools.jsonl")
            elsewhere = {**AXIS_LOG, "address": "0x" + "11" * 20}
            other_event = {**AXIS_LOG, "topics": ["0x" + "22" * 32] + AXIS_LOG["topics"][1:]}
            # An ID that is not its key's hash: a fee the log did not carry.
            forged = {**AXIS_LOG, "data": AXIS_LOG["data"].replace("0064", "01f4", 1)}
            removed = {**AXIS_LOG, "removed": True}
            self.assertEqual(V.fold([elsewhere, other_event, forged, removed], out), (0, 4))
            self.assertFalse(os.path.exists(out) and lines(out))

    def test_the_pool_id_is_the_key_s_hash(self):
        self.assertEqual(V.pool_id(AXIS, USDC, 100, 1, ALPHA), AXIS_ID)
        # A negative spacing is sign-extended, as abi.encode(int24) is.
        self.assertNotEqual(V.pool_id(AXIS, USDC, 100, -1, ALPHA), V.pool_id(AXIS, USDC, 100, 1, ALPHA))


class Sources(unittest.TestCase):
    def test_logs_files_in_any_shape_are_read(self):
        with tempfile.TemporaryDirectory() as d:
            shapes = {
                "array.json": json.dumps([AXIS_LOG]),
                "rpc.json": json.dumps({"jsonrpc": "2.0", "id": 1, "result": [AXIS_LOG]}),
                "explorer.json": json.dumps({"data": {"ok": True, "result": [AXIS_LOG]}}),
                "lines.jsonl": json.dumps(AXIS_LOG) + "\n\n" + json.dumps(AXIS_LOG) + "\n",
            }
            for name, text in shapes.items():
                path = os.path.join(d, name)
                with open(path, "w") as f:
                    f.write(text)
                logs = V.load_logs(path)
                self.assertTrue(logs and all(l == AXIS_LOG for l in logs), name)

    def test_a_scan_asks_in_2000_block_chunks(self):
        asked = []

        def call(method, params):
            self.assertEqual(method, "eth_getLogs")
            q = params[0]
            self.assertEqual((q["address"], q["topics"]), (V.POOL_MANAGER, [V.INITIALIZE]))
            asked.append((int(q["fromBlock"], 16), int(q["toBlock"], 16)))
            return [AXIS_LOG] if asked[-1][0] == 0 else []

        self.assertEqual(V.scan(call, 0, 4_500), [AXIS_LOG])
        self.assertEqual(asked, [(0, 1_999), (2_000, 3_999), (4_000, 4_500)])


if __name__ == "__main__":
    unittest.main()
