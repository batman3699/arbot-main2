"""R38: the armed bid's contest sample, its fit and its held-out gates.

Run from scripts/data: python3 -m unittest arb_census.test_contest
"""
import json
import tempfile
import unittest
from pathlib import Path

from arb_census import contest as C

VECTORS = Path(__file__).resolve().parents[3] / "crates/apex-runtime/tests/data/best_share_vectors.json"
USDC = C.USDC


def verdict(arm_id=1, fire=(100, 4, 20), rival=None, priority=None, **extra):
    """A fired verdict: fire and rival as (block, flashblock, tx_index)."""
    v = {"at_ns": 2_000, "arm_id": arm_id, "route_hash": "0xr", "fired": True, "shadowed": False,
         "fire": {"block": fire[0], "flashblock": fire[1], "tx_index": fire[2], "tx": f"0xtrig{fire[0]}-{fire[2]}"},
         "rival": None, "rival_priority_wei": None, "gross_wei": "1000000000000", "quote_error": None,
         "base_fee_wei": "5000000", "weth_usd": 2500.0}
    if rival is not None:
        v["rival"] = {"block": rival[0], "flashblock": rival[1], "tx_index": rival[2], "tx": "0xrival"}
        v["rival_priority_wei"] = None if priority is None else str(priority)
    v.update(extra)
    return v


# Block 100: flashblock 4 holds txs 10-20, 5 holds 21-30. Block 101: 1 holds 1-5.
LAYOUTS = {(100, 4): (10, 11), (100, 5): (21, 10), (100, 10): (40, 5), (101, 1): (1, 5)}


class GoldenVectors(unittest.TestCase):
    def test_the_mirror_matches_the_runtime_s_vectors(self):
        for case in json.loads(VECTORS.read_text())["cases"]:
            with self.subTest(case["name"]):
                args = (case["rows"], int(case["surplus"]), case["gas"], int(case["cap"]))
                if "error" in case:
                    with self.assertRaisesRegex(C.ShareError, case["error"]):
                        C.best_share(*args)
                    continue
                self.assertEqual(C.best_share(*args), (int(case["bid"]), case["below"], case["total"]))

    def test_an_overflow_fails_closed_as_the_runtime_does(self):
        with self.assertRaisesRegex(C.ShareError, "overflow"):
            C.best_share([[2**128, 1]], 2**200, 1, 2**127)


class Units(unittest.TestCase):
    def test_a_share_rounds_up(self):
        self.assertEqual(C.share_ppb(1, 1, 3), 333_333_334)
        self.assertEqual(C.share_ppb(5_000_000, 300_000, 1_500_000_000_000), 10**9)

    def test_surplus_in_wei(self):
        self.assertEqual(C.surplus_wei({"surplus_wei": "777"}, 2500.0), (777, False))
        # 8913 USDC units at 2.337e-9 units a wei (num/den), rounded down.
        rate = ["14668701730534704205905495518764915746093018382336",
                "6277101735386680763835789423207666416102355444464034512896"]
        wei, flagged = C.surplus_wei({"surplus_wei": "8913", "start": USDC, "start_per_eth": rate}, None)
        self.assertEqual((wei, flagged), (8913 * int(rate[1]) // int(rate[0]), False))
        # Before decision 4 an arm has no rate: the verdict's WETH price, flagged.
        self.assertEqual(C.surplus_wei({"surplus_wei": "2500", "start": USDC.upper().replace("0X", "0x")}, 2500.0),
                         (10**12, True))
        self.assertEqual(C.surplus_wei({"surplus_wei": "1", "start": USDC}, None), (None, True))


class Classify(unittest.TestCase):
    def cls(self, v, own=None):
        return C.classify(v, LAYOUTS, own or {})

    def test_what_cannot_be_judged_is_excluded_by_reason(self):
        self.assertEqual(self.cls(verdict(shadowed=True)), ("excluded:shadowed", None))
        self.assertEqual(self.cls(verdict(quote_error="reverted")), ("excluded:not priced", None))
        self.assertEqual(self.cls(verdict(gross_wei=None)), ("excluded:not priced", None))
        self.assertEqual(self.cls(verdict(fire=(100, None, 20))), ("excluded:fire not placed", None))
        self.assertEqual(self.cls(verdict(fire=(100, 7, 20))), ("excluded:fire not placed", None))
        self.assertEqual(self.cls(verdict(rival=(100, None, 25), priority=9)), ("excluded:rival not placed", None))
        self.assertEqual(self.cls(verdict(fire=(100, 4, 15), rival=(100, 4, 16), priority=None)), ("excluded:rival unreadable", None))
        self.assertEqual(self.cls(verdict(rival=(100, 4, 15), priority=9)), ("excluded:rival before our flashblock", None))

    def test_a_mid_flashblock_trigger_is_contested_in_its_own(self):
        self.assertEqual(self.cls(verdict(fire=(100, 4, 15), rival=(100, 4, 16), priority=9)), ("exact", 9))
        self.assertEqual(self.cls(verdict(fire=(100, 4, 15), rival=(100, 5, 22), priority=9)), ("uncontested", 0))

    def test_a_trigger_closing_its_flashblock_is_contested_in_the_next(self):
        # Live arm 3114: the trigger was flashblock 1's last; the rival won flashblock 2 by fee.
        self.assertEqual(self.cls(verdict(fire=(100, 4, 20), rival=(100, 5, 22), priority=53_150_146)), ("exact", 53_150_146))
        self.assertEqual(self.cls(verdict(fire=(100, 10, 44), rival=(101, 1, 2), priority=7)), ("exact", 7))
        self.assertEqual(self.cls(verdict(fire=(100, 4, 20), rival=(101, None, 2), priority=7)), ("uncontested", 0))

    def test_no_rival_is_uncontested(self):
        self.assertEqual(self.cls(verdict()), ("uncontested", 0))

    def test_our_own_landing_is_censored_at_the_bid_it_paid(self):
        mine = verdict(fire=(100, 4, 15), rival=(100, 4, 16), priority=5_000_001)
        self.assertEqual(self.cls(dict(mine, ours=True)), ("censored", 5_000_001))
        self.assertEqual(self.cls(mine, {"0xrival": 1}), ("censored", 5_000_001), "from the live ledger, before R37's flag")
        self.assertEqual(self.cls(mine, {"0xrival": 2}), ("exact", 5_000_001), "a sibling arm's send is a contest")
        self.assertEqual(self.cls(dict(mine, ours=True, rival_priority_wei=None)), ("excluded:own send unpriced", None))


def fire(cls, value, x, cluster=(1, "a"), route="r", gross=10**13, gas=100_000, base=5_000_000, other=0, best_bid=1, hour=3):
    return {"cls": cls, "value": value, "x": x, "cluster": cluster, "route": route, "gross": gross, "gas": gas,
            "base_fee": base, "other": other, "best_bid": best_bid, "surplus": 10**13, "hour": hour, "day": "d"}


class Fit(unittest.TestCase):
    def test_a_market_move_weighs_once_and_the_bounds_place_censored_rows(self):
        sample = [fire("exact", 7, 50, (1, "a")), fire("uncontested", 0, 0, (1, "a")),
                  fire("censored", 5, 30, (2, "b")), fire("exact", 9, 50, (3, "c"))]
        rows, effective = C.fit_rows(sample)
        self.assertEqual(effective, 3)
        self.assertEqual(rows, [[0, 500_000], [30, 1_000_000], [50, 1_500_000]])
        upper, _ = C.fit_rows(sample, bound="upper")
        self.assertEqual(upper, [[0, 1_500_000], [50, 1_500_000]])

    def test_past_the_cap_neighbours_merge_upwards_keeping_every_weight(self):
        sample = [fire("exact", i, i, (i, "c")) for i in range(1, 51)]
        rows, effective = C.fit_rows(sample, max_rows=10)
        self.assertEqual(effective, 50)
        self.assertLessEqual(len(rows), 10)
        self.assertEqual(sum(w for _, w in rows), 50 * 10**6)
        self.assertEqual(rows[-1][0], 50, "a bucket sits at its largest share")
        self.assertEqual([x for x, _ in rows], sorted(x for x, _ in rows))
        self.assertTrue(all(x % 5 == 0 for x, _ in rows), rows)


class Outcome(unittest.TestCase):
    def test_who_wins_and_what_it_nets(self):
        self.assertEqual(C.outcome(fire("uncontested", 0, 0), 0), (False, 0))
        self.assertEqual(C.outcome(fire("uncontested", 0, 0), 1), (True, 10**13 - 100_000 * 5_000_001))
        self.assertEqual(C.outcome(fire("exact", 9, 0), 9)[0], False)
        self.assertEqual(C.outcome(fire("exact", 9, 0), 10)[0], True)
        self.assertEqual(C.outcome(fire("censored", 9, 0), 9)[0], True, "our landing paid 9")
        self.assertEqual(C.outcome(fire("censored", 9, 0), 8)[0], False, "below it, not known to land")
        self.assertEqual(C.outcome(fire("exact", 1, 0, other=7), 2), (True, 10**13 - 100_000 * 5_000_002 - 7))


class Gates(unittest.TestCase):
    def test_calibration_judges_buckets_with_enough_fires(self):
        good = [(0.5, i % 2 == 0) for i in range(40)]
        self.assertTrue(C.calibration(good)["ok"])
        bad = [(0.5, False)] * 39 + [(0.5, True)]
        self.assertFalse(C.calibration(bad)["ok"], "0.025 realised against 0.5")
        small = [(0.5, False)] * 29
        self.assertTrue(C.calibration(small)["ok"], "29 fires are shown, not judged")
        self.assertFalse(C.calibration([(0.0, True)] * 30)["ok"], "a zero against a win")
        self.assertTrue(C.calibration([(0.0, False)] * 30)["ok"])

    def test_the_bootstrap_resamples_clusters_within_routes(self):
        same = [("r", (i, "t"), 10) for i in range(20)]
        self.assertEqual(C.bootstrap(same, reps=200), (200, 200), "identical clusters: no spread")
        mixed = [("r", (i, "t"), 10 if i % 2 else -10) for i in range(40)]
        low, high = C.bootstrap(mixed, reps=500)
        self.assertLess(low, 0)
        self.assertGreater(high, 0)
        self.assertEqual(C.bootstrap(mixed, reps=500), (low, high), "seeded")
        one_move = [("r", (1, "t"), 10)] * 30
        self.assertEqual(C.bootstrap(one_move, reps=50), (300, 300), "thirty rows of one move are one cluster")
        # One move of twenty rows at +10, twenty single moves at -10: drawn by
        # cluster, a third of resamples miss the big move entirely (-210).
        lumpy = [("r", (0, "big"), 10)] * 20 + [("r", (i, "t"), -10) for i in range(1, 21)]
        self.assertEqual(C.bootstrap(lumpy, reps=2_000)[0], -210, "rows drawn singly would never reach -210")

    def test_both_rules_are_scored_on_the_same_fires(self):
        # One share row at 0: best_share bids 1 wei past it; best_bid carried 50.
        held = [fire("exact", 10, 0, (i, "t"), best_bid=50) for i in range(40)]
        r = C.evaluate([[0, 1]], held)
        self.assertEqual(r["rules"]["best_share"]["wins"], 0, "1 wei loses to 10")
        self.assertEqual(r["rules"]["best_bid"]["wins"], 40)
        self.assertEqual(r["paired"]["fires"], 40)
        self.assertLess(r["paired"]["delta_wei"], 0)
        self.assertLess(r["paired"]["ci95_wei"][1], 0)
        self.assertEqual(r["rules"]["base"]["wins"], 0)


def write_lines(path, rows):
    path.write_text("".join(json.dumps(r) + "\n" for r in rows))


class Section(unittest.TestCase):
    def test_a_section_reconciles_its_fires_and_says_why_it_is_unfit(self):
        with tempfile.TemporaryDirectory() as tmp:
            var = Path(tmp)
            arm = {"kind": "arm", "arm_id": 1, "route_hash": "0xr", "at_ns": 1_000, "gas_expected": 100_000,
                   "surplus_wei": "10000000000000", "bid_wei_per_gas": "5000001", "other_wei": "0", "base_fee_wei": "5000000"}
            write_lines(var / "armed-2026-10-29.jsonl", [arm, {"kind": "renewal", "arm_id": 1}])
            write_lines(var / "flashblocks-2026-10-29.jsonl",
                        [{"block": 100, "index": 4, "first_tx": 10, "tx_count": 11}, {"block": 100, "index": 5, "first_tx": 21, "tx_count": 10}])
            write_lines(var / "armed-verdicts-2026-10-29.jsonl", [
                verdict(fire=(100, 4, 15), rival=(100, 4, 16), priority=9),
                verdict(fire=(100, 4, 12)),
                verdict(shadowed=True),
                verdict(arm_id=2),
                {"at_ns": 2_000, "arm_id": 1, "route_hash": "0xr", "fired": False},
            ])
            write_lines(var / "armed-verdicts-2026-10-30.jsonl", [verdict(fire=(100, 4, 15), rival=(100, 4, 16), priority=9)])
            ledger = var / "armed-live.jsonl"
            write_lines(ledger, [{"kind": "submitting", "arm_id": 1, "tx": "0xother"}])
            s = C.contest_section(var, ledger, ["2026-10-29"], "2026-10-30", arm_days=["2026-10-29"])
        counts = s["report"]["fit_counts"]
        self.assertEqual(counts["fired"], 4)
        self.assertEqual((counts["exact"], counts["uncontested"], counts["excluded:shadowed"], counts["excluded:no arm record"]), (1, 1, 1, 1))
        self.assertEqual(counts["fired"], sum(v for k, v in counts.items() if k in ("exact", "uncontested", "censored") or k.startswith("excluded")))
        self.assertEqual(s["scale"], 10**9)
        self.assertEqual(s["effective"], 2)
        self.assertFalse(s["fit_ok"])
        for why in ("1 fit days, 3 needed", "2 effective rows, 300 needed", "1 of 4 fit fires have no arm record"):
            self.assertIn(why, s["reasons"])
        self.assertTrue(s["model_version"].startswith("r38-2026-10-30-"))

    def test_an_arm_id_reused_after_a_restart_matches_the_arm_before_the_verdict(self):
        arms = {(1, "0xr"): [{"at_ns": 100, "x": "old"}, {"at_ns": 300, "x": "new"}]}
        self.assertEqual(C.arm_of(arms, {"arm_id": 1, "route_hash": "0xr", "at_ns": 200})["x"], "old")
        self.assertEqual(C.arm_of(arms, {"arm_id": 1, "route_hash": "0xr", "at_ns": 400})["x"], "new")
        self.assertIsNone(C.arm_of(arms, {"arm_id": 1, "route_hash": "0xr", "at_ns": 50}))



class Schedule(unittest.TestCase):
    def test_the_fit_is_the_three_days_before_the_held_out_one_from_r37_on(self):
        days = [f"2026-10-{d}" for d in range(26, 32)] + ["2026-11-01"]
        self.assertEqual(C.contest_days(days),
                         (["2026-10-29", "2026-10-30", "2026-10-31"], "2026-11-01", ["2026-10-28", "2026-10-29", "2026-10-30", "2026-10-31", "2026-11-01"]))
        fit, held, _ = C.contest_days([f"2026-10-{d}" for d in range(24, 31)])
        self.assertEqual((fit, held), (["2026-10-29"], "2026-10-30"), "nothing before R37's arms")
        self.assertEqual(C.contest_days([]), ([], None, []))

    def test_attaching_a_section_never_breaks_the_targets(self):
        with tempfile.TemporaryDirectory() as tmp:
            targets = {"ok": True, "routes": []}
            out = C.attach_contest(targets, Path(tmp), Path(tmp) / "none.jsonl", ["2026-10-29", "2026-10-30"])
            self.assertIs(out, targets)
            self.assertFalse(targets["contest"]["fit_ok"])
            self.assertIn("no fit fires", targets["contest"]["reasons"])
            self.assertTrue(targets["ok"], "an unfit contest never unpublishes the targets")
            (Path(tmp) / "armed-verdicts-2026-10-29.jsonl").write_text('{"fired": true, "gross_wei": "1", "fire": 5}\n')
            broken = C.attach_contest({"ok": True}, Path(tmp), Path(tmp) / "none.jsonl", ["2026-10-29", "2026-10-30"])
            self.assertNotIn("contest", broken, "a census that cannot measure leaves best_bid alone")
            self.assertTrue(broken["ok"])


if __name__ == "__main__":
    unittest.main()
