"""The rival-bid measurement: the bid each trade makes (the same rule as
apex_runtime::econ::RivalBids::bid, on the same cases), the rival's effective
priority fee, which gaps count, and the holdout check (R31); the transaction
that would have made our trade revert, where we would have landed, and the
class that leaves each gap in (R33).

Run from scripts/data: python3 -m unittest test_rival_bids
"""
import unittest

import race_analysis as RA
import rival_bids as R

RIVALS = sorted([200_000_000, 0, 50_000_000, 1_000_000, 10_000_000])
GAS = 350_000

A, B = "0x" + "a" * 40, "0x" + "b" * 40
S = 2 ** 96                                   # sqrtPriceX96 at a price of 1
HOPS = [R.Hop(A, True, 500), R.Hop(B, False, 500)]
END = (10, 5)                                 # the trigger flashblock's last transaction
IN, NET = 10 ** 18, 10 ** 14                  # a trade with a 1 bps margin


def _log(pool, topic, words, block, tx, li):
    data = "0x" + "".join(f"{w % 2 ** 256:064x}" for w in words)
    return {"address": pool, "topics": [topic], "data": data, "blockNumber": hex(block),
            "transactionIndex": hex(tx), "logIndex": hex(li), "transactionHash": f"0x{block:04x}{tx:04x}"}


def v3(pool, block, tx, sqrt, li=0):
    return _log(pool, R.SWAP_V3, (1, -1, sqrt, 10 ** 18, 0), block, tx, li)


def v2(pool, block, tx, r0, r1, li=0):
    return _log(pool, R.SYNC_V2, (r0, r1), block, tx, li)


def gap(killer, landings, net=10 * GAS, prio=5, block=1):
    return {"run": "run-10", "outcome": '"dispatched"', "net_ev_wei": net, "pre_tip_net_wei": net,
            "trigger_block": block, "killer_offset": killer, "rival_prio_wei_per_gas": prio, "landing_offsets": landings}


def row(outcome, net, block):
    return {"run": "apex", "outcome": outcome, "net_ev_wei": net, "trigger_block": block}


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

    def test_a_ticket_we_sent_passed_our_simulation_however_it_closed(self):
        # A send that reverts on chain closes as failed/Reverted, the class a
        # Tier 2 failure gets: all 21 of the canary's sends did.
        rows = [dict(row('{"failed": {"class": "failed/Reverted"}}', 10**13, b), ticket=b) for b in (1, 2)]
        self.assertEqual([r["trigger_block"] for r in R.counted(rows, sent={2})], [2])
        self.assertEqual(R.counted([dict(rows[0], net_ev_wei=49 * 10**18)], sent={1}), [], "nor is a sent phantom believed")

    def test_the_net_before_any_tip(self):
        self.assertEqual(R.pre_tip_net(8, "apex", R.R31_AT_NS - 1), 10, "the canary lost R28's 20% before R31")
        self.assertEqual(R.pre_tip_net(8, "apex", R.R31_AT_NS), 8, "R31's bid is a fraction of a percent")
        self.assertEqual(R.pre_tip_net(8, "run-10", R.R31_AT_NS - 1), 8, "a shadow run tipped nothing")


class Kill(unittest.TestCase):
    """The first transaction after the trigger's flashblock that leaves our
    trade unable to pay: the first hop asks for exactly its quote, so any move
    against it; an Aerodrome v2 hop is asked for exactly its quote too; a v3
    second hop keeps paying while the move costs less than the margin."""

    def test_any_net_move_against_our_first_hop_kills_it(self):
        logs = [v3(A, 10, 5, S), v3(A, 10, 7, S + 2), v3(A, 11, 2, S - 1)]
        self.assertEqual(R.first_kill(logs, HOPS, IN, NET, END)[:2], (11, 2))
        self.assertIsNone(R.first_kill(logs[:2], HOPS, IN, NET, END), "a move our way kills nothing")

    def test_it_is_the_state_after_a_whole_transaction_that_counts(self):
        logs = [v3(A, 10, 5, S), v3(A, 10, 7, S - 5, li=0), v3(A, 10, 7, S + 1, li=1)]
        self.assertIsNone(R.first_kill(logs, HOPS, IN, NET, END))

    def test_what_happened_by_the_end_of_the_trigger_flashblock_is_what_we_priced(self):
        logs = [v3(A, 10, 4, S), v3(A, 10, 5, S - 9), v3(A, 10, 8, S - 9)]
        self.assertIsNone(R.first_kill(logs, HOPS, IN, NET, END))

    def test_a_v3_second_hop_absorbs_a_move_inside_the_margin(self):
        # Hop 2 sells token1: its output goes as 1 / sqrt^2.
        inside = S + S * 4 // 100_000             # ~0.8 bps less out, under the 1 bps margin
        beyond = S + S // 10_000                  # ~2 bps less out
        logs = [v3(B, 9, 1, S), v3(B, 10, 9, inside)]
        self.assertIsNone(R.first_kill(logs, HOPS, IN, NET, END))
        self.assertEqual(R.first_kill(logs + [v3(B, 11, 0, beyond)], HOPS, IN, NET, END)[:2], (11, 0))

    def test_an_aerodrome_hop_has_no_margin(self):
        r = 10 ** 21
        against = [v2(B, 9, 1, r, r), v2(B, 10, 9, r - 10 ** 12, r + 10 ** 12)]
        self.assertEqual(R.first_kill(against, HOPS, IN, NET, END)[:2], (10, 9))
        # Hop 2's input is hop 1's output at the state we priced: A's.
        burn = [v3(A, 9, 0, S), v2(B, 9, 1, r, r), v2(B, 10, 9, r // 2, r // 2)]
        self.assertEqual(R.first_kill(burn, HOPS, IN, NET, END)[:2], (10, 9), "less depth pays less for the same input")
        ours = [v2(B, 9, 1, r, r), v2(B, 10, 9, r + 10 ** 12, r - 10 ** 12)]
        self.assertIsNone(R.first_kill(ours, HOPS, IN, NET, END))

    def test_a_pool_that_moved_from_no_state_we_saw_is_not_judged(self):
        self.assertEqual(R.first_kill([v3(A, 10, 7, S - 1)], HOPS, IN, NET, END), "no_reference")


class Landing(unittest.TestCase):
    def test_we_land_in_the_first_flashblock_to_arrive_after_the_send_and_its_delay(self):
        ms = 1_000_000
        ledger = RA.Ledger([{"block": 100, "index": i, "arrived_at_ns": (1_000 + 200 * i) * ms, "first_tx": 10 * i, "tx_count": 10}
                            for i in range(5)])
        self.assertEqual(R.landing_offsets(ledger, (100, 0), 1_100 * ms, [70, 150, 5_000]), [1, 2, None])

    def test_the_delays_are_what_our_sends_took(self):
        ms = 1_000_000
        sends = [{"proposed_ns": 0, "sent_ns": 50 * ms, "landed_arrival_ns": 150 * ms},
                 {"proposed_ns": 0, "sent_ns": 40 * ms, "landed_arrival_ns": 340 * ms},
                 {"proposed_ns": 0, "sent_ns": 70 * ms, "landed_arrival_ns": 130 * ms}]
        self.assertEqual(R.send_delays(sends, probe_ms=(99,)), (50, [100, 300, 60, 99]))


class Classes(unittest.TestCase):
    def test_a_gap_is_judged_against_where_we_would_land(self):
        self.assertEqual(R.classify(None, 2), "ahead", "nothing killed it")
        self.assertEqual(R.classify(3, 2), "ahead", "we land first")
        self.assertEqual(R.classify(2, 2), "same", "outbid it")
        self.assertEqual(R.classify(1, 2), "behind", "gone before we land")

    def test_each_measured_delay_is_one_landing(self):
        self.assertEqual([p["class"] for p in R.pairs([gap(2, [1, 2, 3, None])])], ["ahead", "same", "behind"])

    def test_the_bids_are_the_rivals_we_would_meet_in_our_flashblock(self):
        bids, same, total = R.fit(R.pairs([gap(2, [2, 2, 1], prio=7), gap(1, [2], prio=99), gap(None, [1], prio=None)]))
        self.assertEqual((bids, same, total), ([7, 7], 2, 5))


class Holdout(unittest.TestCase):
    def test_the_split_fits_on_the_earlier_gaps_and_scores_on_the_later(self):
        rows = [row('"dispatched"', 10**13, b) for b in (5, 1, 4, 2, 3)]
        fit, test = R.split(rows, 0.6)
        self.assertEqual([r["trigger_block"] for r in fit], [1, 2, 3])
        self.assertEqual([r["trigger_block"] for r in test], [4, 5])

    def test_a_won_gap_keeps_its_net_less_the_tip_and_a_lost_one_pays_the_tip(self):
        pairs = R.pairs([gap(1, [1], prio=3),       # same, outbid at 4: keeps its net less 4 * gas
                         gap(1, [1], prio=9),       # same, lost at 4: pays 4 * gas
                         gap(2, [1], prio=9),       # ahead: won whatever the bid
                         gap(0, [1], prio=0)])      # behind: lost whatever the bid
        kept, won = R.score(pairs, lambda r: 4, GAS)
        self.assertEqual(won, 2)
        self.assertEqual(kept, 2 * 10 * GAS - 4 * 4 * GAS)
        tie = R.pairs([gap(1, [1], prio=4)])
        self.assertEqual(R.score(tie, lambda r: 4, GAS), (-4 * GAS, 0), "a tie is not a win: the runtime bids the rival + 1 wei")


if __name__ == "__main__":
    unittest.main()
