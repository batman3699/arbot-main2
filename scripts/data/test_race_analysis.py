"""R25's contest analyzer, on a synthetic ledger in ticket 263's shape: the
trigger at transaction 303, a rival at 304.

Run from scripts/data: python3 -m unittest test_race_analysis
"""
import json
import os
import tempfile
import unittest

import race_analysis as R

MS = 1_000_000
POOL_A, POOL_B = "0xb90fe999be6869af0afc557dccfbe169ea3403d6", "0xa6da283cf7d06b2279626e787c329d6acf7e2994"
SWAP = "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67"


def fb(block, index, first, n, at_ms, ts=None):
    return {"block": block, "index": index, "first_tx": first, "tx_count": n, "arrived_at_ns": at_ms * MS, "block_timestamp": ts}


# Block 100: index 2 ends at tx 303 (the trigger), index 3 carries 304-310.
LINES = [fb(100, 0, 0, 1, 0, 1), fb(100, 1, 1, 200, 200), fb(100, 2, 201, 103, 370),
         fb(100, 3, 304, 7, 540), fb(100, 4, 311, 5, 710), fb(101, 0, 0, 1, 2000, 3)]
LEDGER = R.Ledger(LINES)


class FakeRpc:
    def __init__(self):
        self.calls = []

    def call(self, method, params):
        self.calls.append(method)
        if method == "eth_getTransactionReceipt":
            return {"transactionIndex": hex(303), "blockNumber": hex(100)}
        if method == "eth_getLogs":
            def log(pool, txi, tx):
                return {"address": pool, "topics": [SWAP], "blockNumber": hex(100), "transactionIndex": hex(txi), "transactionHash": tx}
            # The trigger itself swaps through both pools: it is not its own rival.
            return [log(POOL_A, 303, "0xt"), log(POOL_B, 303, "0xt"), log(POOL_A, 304, "0xr"), log(POOL_B, 304, "0xr")]
        if method == "eth_getTransactionByHash":
            return {"gasPrice": hex(1_802_500_000), "maxPriorityFeePerGas": None}
        raise AssertionError(method)


def race_line(**over):
    line = {"route_hash": "0xroute", "trigger": {"block": 100, "log_index": 51, "tx": "0xt"},
            "observed_ns": 380 * MS, "received_ns": 400 * MS, "proposed_ns": 410 * MS,
            "picked_ns": 411 * MS, "handled_ns": 650 * MS, "outcome": {"failed": {"class": "SIM_FAIL/MinOutNotMet"}}, "ticket": 263}
    line.update(over)
    return line


class AnalyzerTest(unittest.TestCase):
    def test_a_transaction_is_found_in_the_flashblock_that_added_it(self):
        self.assertEqual(R.Ledger.index_of(LEDGER.locate(100, 303)), 2)
        self.assertEqual(R.Ledger.index_of(LEDGER.locate(100, 304)), 3)
        self.assertIsNone(LEDGER.locate(100, 999))

    def test_would_land_is_the_first_flashblock_after_send_plus_delay_and_margin(self):
        # Sent at 420 ms with delta 5 and eps 20: 445 ms -> index 3 (540 ms).
        self.assertEqual(LEDGER.would_land(420 * MS, 5 * MS, 20 * MS), (100, 3))
        # Sent at 530 ms: 555 ms -> index 4.
        self.assertEqual(LEDGER.would_land(530 * MS, 5 * MS, 20 * MS), (100, 4))
        # Due exactly at an arrival: that flashblock.
        self.assertEqual(LEDGER.would_land(515 * MS, 5 * MS, 20 * MS), (100, 3))
        self.assertIsNone(LEDGER.would_land(2001 * MS, 0, 0), "past the ledger's end")

    def test_offsets_count_flashblocks_across_blocks(self):
        self.assertEqual(LEDGER.offset((100, 2), (100, 4)), 2)
        self.assertEqual(LEDGER.offset((100, 4), (101, 0)), 1)
        self.assertIsNone(LEDGER.offset((100, 2), (102, 0)))

    def test_the_rival_is_the_first_later_transaction_touching_both_pools(self):
        r = R.rival(FakeRpc(), [POOL_A, POOL_B], 100, 303)
        self.assertEqual((r["block"], r["tx_index"], r["tx"]), (100, 304, "0xr"))
        self.assertAlmostEqual(r["priority_gwei"], 1.8025, places=4)

    def test_a_contested_gap_reports_offsets_from_the_trigger_flashblock(self):
        journal = {263: {"exacting_ns": 412 * MS, "closed_ns": 642 * MS}}
        rows, summary = R.analyze([race_line()], LEDGER, journal, FakeRpc(), {"0xroute": [POOL_A, POOL_B]}, delta_ns=5 * MS)
        row = rows[0]
        self.assertEqual(row["trigger_flashblock"], (100, 2))
        self.assertEqual(row["rival_offset"], 1)              # rival in index 3
        self.assertEqual(row["ours_at_proposed"][20], 1)      # 410+25 -> index 3
        self.assertEqual(row["ours_after_tier2"][20], 2)      # Tier 2 ends 642: 667 -> index 4 (710), offset 2
        self.assertEqual(row["g1_ms"], 30)                    # 400 - 370
        self.assertEqual(row["stage_ms"]["tier2"], 230)
        self.assertEqual(summary["g2_share"][20], 1.0)
        self.assertEqual(summary["ahead_share"][20], 0.0, "the rival's flashblock, not before it")
        self.assertEqual(summary["rival_offsets"], {1: 1})

    def test_one_trigger_is_resolved_once(self):
        rpc = FakeRpc()
        race = [race_line(), race_line(proposed_ns=420 * MS, ticket=None)]
        rows, summary = R.analyze(race, LEDGER, {}, rpc, {"0xroute": [POOL_A, POOL_B]}, delta_ns=5 * MS)
        self.assertEqual(len(rows), 2)
        self.assertEqual(rpc.calls.count("eth_getTransactionReceipt"), 1)
        self.assertEqual(rpc.calls.count("eth_getLogs"), 1)
        self.assertIsNone(rows[1]["ours_after_tier2"][20], "no ticket, no Tier 2 end")

    def test_lines_without_a_route_or_a_trigger_transaction_are_skipped(self):
        race = [race_line(route_hash="0xother"), race_line(trigger={"block": 100, "log_index": 1, "tx": ""})]
        rows, summary = R.analyze(race, LEDGER, {}, FakeRpc(), {"0xroute": [POOL_A, POOL_B]}, delta_ns=0)
        self.assertEqual(rows, [])
        self.assertEqual(summary["contested"], 0)
        self.assertFalse(summary["g1_pass"] or summary["g2_pass"])

    def test_only_the_blocks_near_a_trigger_are_loaded(self):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "flashblocks-2026-10-12.jsonl")
            with open(path, "w") as f:
                for x in LINES + [fb(500, 0, 0, 1, 9000, 9)]:
                    f.write(json.dumps(x) + "\n")
            ledger = R.load_ledger([path], {100})
        self.assertEqual(sorted(ledger.by_block), [100, 101])


if __name__ == "__main__":
    unittest.main()
