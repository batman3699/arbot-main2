"""R36 Task 2: phase-1 targets, per-gas bid samples, calibration and share report.

Run from scripts/data: python3 -m unittest arb_census.test_targets
"""
import json
import math
import os
import tempfile
import unittest
from pathlib import Path

from arb_census import race, targets as T
from arb_census.venues import USDC, WETH


def arb(pools=("pa", "pb"), weth_in="pa", venues=("uniswap_v3", "aerodrome_v2"),
        gain=(WETH,), offset=0, usd=10.0, prio=None, gas=None, hops=2):
    return {"pools": list(pools), "weth_in": weth_in, "venues": list(venues), "gain": list(gain),
            "offset": offset, "usd": usd, "prio": prio, "gas": gas, "hops": hops}


class Select(unittest.TestCase):
    def test_only_two_hop_weth_routes_on_our_venues_taken_in_the_trigger_block(self):
        self.assertTrue(T.select(arb()))
        self.assertFalse(T.select(arb(offset=1)))
        self.assertFalse(T.select(arb(venues=("uniswap_v3", "uniswap_v4"))))
        self.assertFalse(T.select(arb(gain=(USDC,))))
        self.assertFalse(T.select(arb(hops=3)))
        self.assertFalse(T.select(arb(weth_in=None)))

    def test_route_keys_are_weth_in_pool_first_and_preserve_direction(self):
        self.assertEqual(T.route_key(arb(pools=("pb", "pa"), weth_in="pa")), ("pa", "pb"))
        self.assertNotEqual(T.route_key(arb(pools=("pa", "pb"), weth_in="pa")),
                            T.route_key(arb(pools=("pa", "pb"), weth_in="pb")))
        self.assertIsNone(T.route_key(arb(pools=("pa", "pa"))))
        self.assertIsNone(T.route_key(arb(pools=("pa", "pb"), weth_in="pc")))


class Build(unittest.TestCase):
    def test_routes_keyed_weth_in_first_whatever_the_log_order(self):
        day = {"eth_usd": 2500.0, "arbs": [
            arb(usd=5.0),
            arb(pools=("pb", "pa"), weth_in="pa", usd=4.0),
            arb(pools=("pc", "pd"), weth_in="pd", usd=20.0),
        ]}
        out = T.build([day] * 3, top=2)
        self.assertEqual([(r["pools"], r["usd"], r["arbs"]) for r in out["routes"]],
                         [(["pd", "pc"], 60.0, 3), (["pa", "pb"], 27.0, 6)])

    def test_bids_are_the_winners_priority_per_gas(self):
        day = {"eth_usd": 2500.0, "arbs": [
            arb(prio=3_000_000, gas=300_000), arb(prio=1_000_000, gas=500_000)
        ]}
        out = T.build([day] * 3, top=50)
        self.assertEqual(out["bids_wei_per_gas"], [1_000_000] * 3 + [3_000_000] * 3)

    def test_bids_come_only_from_routes_selected_for_the_top_k(self):
        day = {"arbs": [arb(pools=("pa", "pb"), weth_in="pa", usd=20.0, prio=9, gas=1),
                        arb(pools=("pc", "pd"), weth_in="pc", usd=10.0, prio=1, gas=1)]}
        out = T.build([day] * 3, top=1)
        self.assertEqual(out["bids_wei_per_gas"], [9, 9, 9])

    def test_a_route_carries_its_own_bids_only_from_twenty(self):
        rows = [arb(prio=10**8, gas=300_000) for _ in range(19)]
        out = T.build([{"eth_usd": 2500.0, "arbs": rows}], top=50)
        self.assertNotIn("bids_wei_per_gas", out["routes"][0])
        rows.append(arb(prio=10**8, gas=300_000))
        out = T.build([{"eth_usd": 2500.0, "arbs": rows}], top=50)
        self.assertEqual(out["routes"][0]["bids_wei_per_gas"], [10**8] * 20)

    def test_unfit_files_say_so(self):
        self.assertFalse(T.build([], top=50)["ok"])
        self.assertFalse(T.build([{"eth_usd": 2500.0, "arbs": [arb()]}] * 2, top=50)["ok"])
        self.assertTrue(T.build([{"eth_usd": 2500.0, "arbs": [arb()]}] * 3, top=50)["ok"])
        self.assertFalse(T.build([{"eth_usd": 2500.0, "arbs": [arb(offset=1)]}] * 3, top=50)["ok"],
                         "a target file with no eligible route is not live-ready")
        with self.assertRaises(ValueError):
            T.write({"x": float("nan")}, os.path.join(tempfile.mkdtemp(), "t.json"))

    def test_an_unfit_file_never_replaces_the_good_one(self):
        out = Path(tempfile.mkdtemp()) / "targets.json"
        good = T.build([{"eth_usd": 2500.0, "arbs": [arb()]}] * 3, top=50)
        self.assertTrue(T.publish(good, out))
        self.assertFalse(T.publish(T.build([], top=50), out), "unfit: the job fails, so the canary is not restarted")
        self.assertTrue(json.loads(out.read_text())["ok"], "the last good targets stay")
        self.assertFalse(json.loads((out.parent / "targets.unfit.json").read_text())["ok"])

    def test_bad_value_cannot_be_marked_live_ready(self):
        days = [{"arbs": [arb(usd=math.nan)]} for _ in range(3)]
        out = T.build(days, top=50)
        self.assertFalse(out["ok"])
        self.assertEqual(out["routes"], [])

    def test_non_finite_bid_inputs_cannot_be_marked_live_ready(self):
        days = [{"arbs": [arb(prio=math.inf, gas=300_000)]} for _ in range(3)]
        self.assertFalse(T.build(days, top=50)["ok"])

    def test_dated_coverage_must_be_three_distinct_days(self):
        day = {"day": "2026-10-23", "arbs": [arb()]}
        self.assertFalse(T.build([day, day, day], top=50)["ok"])
        distinct = [dict(day, day=f"2026-10-{n}") for n in (21, 22, 23)]
        self.assertTrue(T.build(distinct, top=50)["ok"])
        mixed = [distinct[0], distinct[1], {"arbs": [arb()]}]
        self.assertFalse(T.build(mixed, top=50)["ok"])

    def test_calibration_compares_fit_with_last_day_holdout(self):
        rows = T.calibration([1, 2, 3, 4], [1, 1, 5, 5])
        self.assertEqual([r["bid"] for r in rows], [2, 3, 4, 4])
        self.assertEqual((rows[0]["fit"], rows[0]["held_out"]), (0.25, 0.5))

    def test_empty_fit_or_holdout_has_no_calibration_claim(self):
        self.assertEqual(T.calibration([], [1, 2]), [])
        self.assertEqual(T.calibration([1, 2], []), [])

    def test_json_writer_is_atomic_and_finite_only(self):
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "targets.json"
            T.write({"ok": True, "bids_wei_per_gas": [1, 2]}, path)
            self.assertEqual(path.read_text().strip(), '{"ok":true,"bids_wei_per_gas":[1,2]}')
            self.assertFalse(path.with_name("targets.json.tmp").exists())
            with self.assertRaises(ValueError):
                T.write({"x": float("inf")}, path)
            self.assertEqual(path.read_text().strip(), '{"ok":true,"bids_wei_per_gas":[1,2]}')


class MarketShare(unittest.TestCase):
    def test_our_landings_are_set_against_market_per_route_and_day(self):
        day = {"day": "2026-10-25", "eth_usd": 2500.0,
               "arbs": [arb(usd=10.0), arb(usd=5.0)]}
        landed = [{"kind": "landed", "pools": ["pa", "pb"], "net_wei": "1000", "at_ms": 1792900000000}]
        self.assertEqual(T.ours_vs_market([day], landed), [
            {"day": "2026-10-25", "pools": ["pa", "pb"], "market_usd": 15.0,
             "ours_landed": 1, "ours_net_wei": 1000}
        ])

    def test_landings_on_a_different_day_are_excluded(self):
        day = {"day": "2026-10-25", "arbs": [arb(usd=10.0)]}
        landed = [{"kind": "landed", "pools": ["pa", "pb"], "net_wei": "5000", "at_ms": 1792813600000}]
        row = T.ours_vs_market([day], landed)[0]
        self.assertEqual((row["ours_landed"], row["ours_net_wei"]), (0, 0))

    def test_undated_unit_fixture_can_be_compared_by_route(self):
        self.assertEqual(T.ours_vs_market([], [
            {"kind": "landed", "pools": ["pa", "pb"], "net_wei": "1000"}
        ]), [{"pools": ["pa", "pb"], "market_usd": 0.0,
              "ours_landed": 1, "ours_net_wei": 1000}])


class UtcBlockRange(unittest.TestCase):
    def test_block_range_uses_timestamps_not_fixed_blocks_per_day(self):
        class FakeRpc:
            def call(self, method, params):
                if method == "eth_blockNumber":
                    return hex(5)
                block_number = int(params[0], 16)
                return {"timestamp": hex(block_number * 86_400)}

        self.assertEqual(T._utc_day_block_range(FakeRpc(), "1970-01-01"), (0, 0))

    def test_cache_is_rebuilt_when_offsets_are_not_block_deltas(self):
        data = {"arbs": [{"pools": ["pa", "pb"], "weth_in": "pa", "gain": [WETH],
                          "venues": ["uniswap_v3"], "hops": 2, "offset": 0,
                          "trigger": [100, 0], "block": 103, "usd": 1.0}]}
        self.assertFalse(T._cache_current(data, "2026-10-23", has_flashblocks=False))
        data["arbs"][0]["offset"] = 3
        self.assertTrue(T._cache_current(data, "2026-10-23", has_flashblocks=False))

    def test_versioned_cache_still_checks_block_offset_semantics(self):
        data = {"date": "2026-10-23", "_targets_cache_schema": T.CACHE_SCHEMA_VERSION,
                "_targets_offset_mode": "block_number",
                "arbs": [{"pools": ["pa", "pb"], "weth_in": "pa", "gain": [WETH],
                          "venues": ["uniswap_v3"], "hops": 2, "offset": 0,
                          "trigger": [100, 0], "block": 103, "usd": 1.0}]}
        self.assertFalse(T._cache_current(data, "2026-10-23", has_flashblocks=False))
        data["arbs"][0]["offset"] = 3
        self.assertTrue(T._cache_current(data, "2026-10-23", has_flashblocks=False))
        self.assertFalse(T._cache_current(data, "2026-10-24", has_flashblocks=False))


if __name__ == "__main__":
    unittest.main()
