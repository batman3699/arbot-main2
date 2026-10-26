"""R34: when the arbitrage on a venue is taken -- the trigger inferred
behind each arbitrage, the flashblocks between them, and what our landings
would have kept.

Run from scripts/data: python3 -m unittest arb_census.test_race
"""
import unittest
from unittest import mock

from arb_census import race
from arb_census.venues import Leg, WETH

T = "0x" + "33" * 20          # sorts below WETH: the flow token of a T/WETH pool
U = "0x" + "77" * 20


def leg(pool, t_delta, tx="0xh"):
    """A pool's side of a T/WETH trade: + it took T."""
    return Leg(tx, 1, "v", pool, {T: t_delta, WETH: -t_delta})


class Trigger(unittest.TestCase):
    """The trigger: on one of the arbitrage's pools, counting back from it,
    where the net flow against the arbitrage's leg reached half of what that
    leg moved; the latest such point over its pools."""

    def test_net_flow_against_the_leg_is_counted_back_to_half_of_it(self):
        # The arbitrage sells 100 T into p. Before it: someone bought 30 T,
        # someone sold 10, someone bought 40. Back from the arbitrage: 40, 30
        # net of the 10, then 60 >= 50 at the first.
        history = {"p": [((1, 0, 0), leg("p", -30)), ((1, 1, 0), leg("p", 10)), ((1, 2, 0), leg("p", -40))]}
        self.assertEqual(race.trigger(history, [leg("p", 100)]), (1, 0, 0))
        self.assertEqual(race.trigger(history, [leg("p", 80)]), (1, 2, 0), "40 is half of 80")

    def test_the_latest_of_its_pools_is_the_trigger(self):
        history = {"p": [((1, 0, 0), leg("p", -60))], "q": [((1, 5, 0), leg("q", 70))]}
        self.assertEqual(race.trigger(history, [leg("p", 100), leg("q", -100)]), (1, 5, 0))

    def test_no_trigger_when_the_flow_never_reached_half(self):
        history = {"p": [((1, 0, 0), leg("p", -20)), ((1, 1, 0), leg("p", -20))]}
        self.assertIsNone(race.trigger(history, [leg("p", 100)]))
        self.assertIsNone(race.trigger({}, [leg("p", 100)]))

    def test_a_leg_of_more_than_two_tokens_says_nothing(self):
        three = Leg("0xh", 1, "v", "p", {T: 5, U: -1, WETH: -4})
        self.assertIsNone(race.flow(three))
        self.assertEqual(race.flow(leg("p", -7)), (T, -7))


class Route(unittest.TestCase):
    """R36: each arbitrage's route, keyed WETH-in pool first downstream."""

    def test_the_weth_in_pool_is_the_one_that_took_weth(self):
        a = Leg("0xh", 1, "uniswap_v3", "pa", {WETH: 100, T: -50})
        b = Leg("0xh", 1, "slipstream_5e7b", "pb", {T: 50, WETH: -103})
        self.assertEqual(race.route_of([b, a]), ("pa", ["pb", "pa"], [WETH]))

    def test_no_weth_in_without_exactly_one_pool_taking_weth(self):
        a = Leg("0xh", 1, "v", "pa", {U: 100, T: -50})
        b = Leg("0xh", 1, "v", "pb", {T: 50, U: -103})
        self.assertEqual(race.route_of([a, b])[0], None)


class Denim(unittest.TestCase):
    def test_without_a_flashblock_ledger_the_offset_is_counted_in_blocks(self):
        self.assertEqual(race.block_offset(500, 500), 0)
        self.assertEqual(race.block_offset(502, 500), 2)


class Landing(unittest.TestCase):
    def test_our_landings_are_r33s(self):
        gaps = [{"landing_offsets": [1, 2, 2, None]}, {"landing_offsets": [2]}]
        self.assertEqual(race.landing_distribution(gaps), {1: 0.25, 2: 0.75})


class Captured(unittest.TestCase):
    """What our landing would have kept of an arbitrage taken `offset`
    flashblocks after its trigger: all of it landing earlier, what is left
    after outbidding it landing in its flashblock, nothing later."""
    LAND = {1: 0.25, 2: 0.5, 3: 0.25}

    def test_earlier_wins_level_is_a_contest_later_loses(self):
        # $10 taken; our gas $0.02; its bid 1 gwei on our 350k gas at $3,000 is $1.05.
        kept = race.captured(10.0, 0.02, 10**9, 2, self.LAND, eth_usd=3000.0, gas=350_000)
        self.assertAlmostEqual(kept, 0.25 * 9.98 + 0.5 * (9.98 - (10**9 + 1) * 350_000 / 1e18 * 3000))

    def test_taken_in_its_trigger_flashblock_is_never_ours(self):
        self.assertEqual(race.captured(10.0, 0.02, 0, 0, self.LAND, eth_usd=3000.0), 0.0)

    def test_a_contest_that_costs_more_than_it_pays_is_not_entered(self):
        self.assertAlmostEqual(race.captured(1.0, 0.02, 10**10, 1, self.LAND, eth_usd=3000.0), 0.0)

    def test_an_arbitrage_worth_less_than_our_gas_is_nothing(self):
        self.assertEqual(race.captured(0.01, 0.02, 0, 3, self.LAND, eth_usd=3000.0), 0.0)

    def test_without_a_trigger_there_is_no_timing(self):
        self.assertIsNone(race.captured(10.0, 0.02, 0, None, self.LAND, eth_usd=3000.0))


class Endpoints(unittest.TestCase):
    def test_only_a_transport_failure_or_a_limit_moves_to_the_next_endpoint(self):
        self.assertTrue(race.transient("transport failed: HTTPError"))
        self.assertTrue(race.transient("Rate limit exceeded"))
        self.assertTrue(race.transient("request timeout"))
        self.assertFalse(race.transient("execution reverted"), "the same on every endpoint")
        self.assertFalse(race.transient("invalid opcode"))


class Fake:
    """An endpoint that answers its name, or fails as a spent free tier does."""

    def __init__(self, name, calls, fail=False):
        self.name, self.calls, self.fail = name, calls, fail

    def call(self, method, params):
        self.calls.append(self.name)
        if self.fail:
            raise race.RpcError("You've reached the usage limit for your current plan")
        return self.name


class Bench(unittest.TestCase):
    """R36: 32 h of census spent two of three free tiers, and every other call
    still started on a spent one and paid its retries: a crawl."""

    def test_an_endpoint_that_failed_sits_out_its_cool_down(self):
        calls, clock = [], [1000.0]
        dead, live = Fake("dead", calls, fail=True), Fake("live", calls)
        with mock.patch.object(race.time, "monotonic", lambda: clock[0]), mock.patch.object(race.time, "sleep") as sleep:
            p = race.Public(rpcs=[dead, live])
            self.assertEqual(p.call("eth_blockNumber", []), "live")
            for _ in range(5):
                p.call("eth_blockNumber", [])
            self.assertEqual(calls, ["dead"] + ["live"] * 6, "benched after one failure")
            sleep.assert_not_called()
            clock[0] += race.COOL_DOWN_S + 1
            calls.clear()
            p.call("eth_blockNumber", [])
            p.call("eth_blockNumber", [])
            self.assertEqual(calls.count("dead"), 1, "tried again once its cool-down ends")

    def test_with_every_endpoint_benched_it_backs_off_and_still_answers_the_error(self):
        calls, clock = [], [1000.0]
        with mock.patch.object(race.time, "monotonic", lambda: clock[0]), mock.patch.object(race.time, "sleep") as sleep:
            p = race.Public(rpcs=[Fake("a", calls, fail=True), Fake("b", calls, fail=True)])
            with self.assertRaises(race.RpcError):
                p.call("eth_blockNumber", [])
            self.assertEqual(len(calls), 6, "three rounds, as before")
            self.assertTrue(sleep.called, "nothing ready: back off")


class Shares(unittest.TestCase):
    def test_value_by_flashblocks_after_the_trigger(self):
        arbs = [{"usd": 6.0, "offset": 0}, {"usd": 2.0, "offset": 1}, {"usd": 1.0, "offset": 3}, {"usd": 1.0, "offset": None}]
        self.assertEqual(race.by_offset(arbs), {"0": 0.6, "1": 0.2, "2": 0.0, "3+": 0.1, "none": 0.1})


if __name__ == "__main__":
    unittest.main()
