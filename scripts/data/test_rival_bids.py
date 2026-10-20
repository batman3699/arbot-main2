"""R31's rival-bid measurement: the bid each trade makes (the same rule as
apex_runtime::econ::RivalBids::bid, on the same cases), the rival's effective
priority fee, which gaps count, and the holdout check.

Run from scripts/data: python3 -m unittest test_rival_bids
"""
import unittest

import rival_bids as R

RIVALS = sorted([200_000_000, 0, 50_000_000, 1_000_000, 10_000_000])
GAS = 350_000


def row(outcome, net, block, prio=5, cls="same", run="apex"):
    return {"run": run, "outcome": outcome, "net_ev_wei": net, "trigger_block": block, "rival_prio_wei_per_gas": prio, "class": cls}


class Bid(unittest.TestCase):
    def test_the_same_bids_as_the_runtime(self):
        # crates/apex-runtime/tests/live_econ.rs, the_bid_maximises_...: one gap
        # in four contested in our own flashblock.
        self.assertEqual(R.best_bid(RIVALS, 1_000_000_000_000_000, GAS, 1, 4), 50_000_001)
        self.assertEqual(R.best_bid(RIVALS, 10_000_000_000_000, GAS, 1, 4), 1_000_001)
        self.assertEqual(R.best_bid(RIVALS, 100_000_000_000, GAS, 1, 4), 1)
        self.assertEqual(R.best_bid(RIVALS, 1_000_000, GAS, 1, 4), 0)
        self.assertEqual(R.best_bid(RIVALS, 0, GAS, 1, 4), 0)
        self.assertEqual(R.best_bid(RIVALS, -5, GAS, 1, 4), 0)
        self.assertEqual(R.best_bid(RIVALS, 1_000_000_000_000_000, GAS, 1, 1), 200_000_001)


class Measure(unittest.TestCase):
    def test_the_effective_priority_is_what_was_paid_over_the_base_fee(self):
        self.assertEqual(R.effective_priority(1_000_000_123, 1_000_000_000), 123)
        self.assertEqual(R.effective_priority(900, 1_000), 0, "never below zero")

    def test_only_gaps_our_simulation_passed_count_and_phantoms_do_not(self):
        rows = [row('{"failed": {"class": "failed/RiskRejected"}}', 10**13, 1),
                row('"dispatched"', 10**13, 2),
                row('{"failed": {"class": "failed/SignerUnavailable"}}', 10**13, 3),
                row('{"failed": {"class": "failed/Reverted"}}', 10**13, 4),        # simulation failed
                row('{"declined": {"reason": "LOW_EV/NoProfitableSize"}}', 10**13, 5),
                row('"dispatched"', 49 * 10**18, 6),                              # a phantom's net
                row('"dispatched"', None, 7)]
        self.assertEqual([r["trigger_block"] for r in R.counted(rows)], [1, 2, 3])


class Holdout(unittest.TestCase):
    def test_the_split_fits_on_the_earlier_gaps_and_scores_on_the_later(self):
        rows = [row('"dispatched"', 10**13, b) for b in (5, 1, 4, 2, 3)]
        fit, test = R.split(rows, 0.6)
        self.assertEqual([r["trigger_block"] for r in fit], [1, 2, 3])
        self.assertEqual([r["trigger_block"] for r in test], [4, 5])

    def test_a_won_gap_keeps_its_net_less_the_tip_and_a_lost_one_pays_the_tip(self):
        rows = [row('"dispatched"', 10 * GAS, 1, prio=3, cls="same"),     # outbid at 4: keeps its net less 4 * gas
                row('"dispatched"', 10 * GAS, 2, prio=9, cls="same"),     # lost at 4: pays 4 * gas
                row('"dispatched"', 10 * GAS, 3, prio=9, cls="ahead"),    # won whatever the bid
                row('"dispatched"', 10 * GAS, 4, prio=0, cls="behind")]   # lost whatever the bid
        kept, won = R.score(rows, lambda r: 4, GAS)
        pre = 10 * GAS / 0.8   # the canary run's nets are after R28's 20% tip
        self.assertEqual(won, 2)
        self.assertAlmostEqual(kept, 2 * pre - 4 * 4 * GAS)
        shadow = [dict(r, run="run-10") for r in rows]
        self.assertAlmostEqual(R.score(shadow, lambda r: 4, GAS)[0], 2 * 10 * GAS - 4 * 4 * GAS, msg="a shadow run's nets had no tip")


if __name__ == "__main__":
    unittest.main()
