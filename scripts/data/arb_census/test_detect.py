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
        # The trader pays 51 T to b for the 50 T a gave it: 1 T spent (1% of
        # its flow, far past dust), whatever it made in WETH.
        legs = [Leg("0x1", 1, "a", "p", {WETH: 100, T: -50}), Leg("0x1", 1, "b", "q", {T: 51, WETH: -101})]
        self.assertFalse(detect.is_arbitrage(legs))

    def test_rounding_dust_is_not_spending(self):
        # WETH: 10 spent of ~2,000,000 flow (0.0005%) is dust; T: 10 gained.
        legs = [Leg("0x1", 1, "a", "p", {WETH: 1_000_000, T: -60}), Leg("0x1", 1, "b", "q", {T: 50, WETH: -999_990})]
        self.assertTrue(detect.is_arbitrage(legs))

    def test_a_pools_weth_side_with_no_weth_transfer_was_native_eth(self):
        # A Curve pool paid 40 "WETH" as native ETH: no WETH log at all. The
        # bot received it, so banked value counts it; a pool that did move
        # WETH by Transfer is not touched.
        pool, bot = "0x" + "aa" * 20, "0x" + "bb" * 20
        receipt = {"from": bot, "to": bot, "logs": []}
        legs = [Leg("0x1", 1, "curve", pool, {T: 30, WETH: -40})]
        self.assertEqual(detect.banked(receipt, legs).get(WETH), 40)

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

    def test_native_eth_paid_into_v4_is_counted_against_what_was_banked(self):
        # The bot paid ~0.141 ETH into a native-ETH v4 pool, which no Transfer
        # log shows, and received ~0.141 WETH back: it made ~0.0000059 WETH,
        # not 0.141. Counting the v4 leg's native flow says so.
        f = fixture("native_v4")
        legs = detect.legs_of(f["receipt"], f["meta"])
        self.assertGreater(detect.banked(f["receipt"]).get(WETH, 0), 10**17, "the transfers alone overstate it")
        kept = detect.banked(f["receipt"], legs).get(WETH, 0)
        self.assertTrue(0 < kept < 10**14, kept)

    def test_eth_an_executor_unwrapped_and_paid_in_is_not_charged_to_the_sender(self):
        # Executor 0x1c96... (neither sender nor target) unwrapped 0.335 WETH
        # and paid it into a v4 pool; the 0.0059 WETH profit went to a third
        # address. The sender and target kept nothing, and lost nothing.
        f = fixture("native_in_by_executor")
        legs = detect.legs_of(f["receipt"], f["meta"])
        self.assertTrue(detect.is_arbitrage(legs))
        self.assertEqual(detect.banked(f["receipt"], legs), {})

    def test_eth_a_v4_pool_paid_to_an_executor_that_wrapped_it_is_not_credited_to_the_sender(self):
        # A v4 pool paid 0.11413 ETH to executor 0x8f10..., which wrapped
        # 0.11411 of it; only the 0.0000197 left over is the bot's gain.
        f = fixture("native_out_by_executor")
        legs = detect.legs_of(f["receipt"], f["meta"])
        self.assertEqual(detect.banked(f["receipt"], legs).get(WETH), 19691080887285)

    def test_profit_an_executor_keeps_and_unwraps_is_the_bots(self):
        # Executor 0x20cc... took the 0.0083 WETH gain and unwrapped it; the
        # target paid a 0.0021 WETH fee. The bot kept the difference.
        f = fixture("profit_at_executor")
        legs = detect.legs_of(f["receipt"], f["meta"])
        self.assertEqual(detect.banked(f["receipt"], legs).get(WETH), 8297473281402047 - 2077151399059272)

    def test_eth_a_venue_wraps_is_not_the_bots(self):
        # The v4 PoolManager wrapped the 0.748 ETH a v4 pool paid out and sent
        # it on as WETH; the 3.63 USDC profit went to a third address.
        f = fixture("venue_wraps")
        legs = detect.legs_of(f["receipt"], f["meta"])
        self.assertEqual(detect.banked(f["receipt"], legs), {})

    def test_eth_an_adapter_unwraps_for_a_venue_is_not_charged_twice(self):
        # 0xa537... unwrapped the bot's 0.1004 WETH to pay a v4 pool and
        # wrapped the 0.1019 ETH another paid back; it trades with no venue by
        # Transfer. The bot kept the 0.0015 WETH difference.
        f = fixture("other_unwraps")
        legs = detect.legs_of(f["receipt"], f["meta"])
        self.assertEqual(detect.banked(f["receipt"], legs), {WETH: 101940629516092826 - 100413320800297764})

    def test_an_address_that_unwraps_what_the_bot_paid_it_is_a_payee(self):
        # The bot wrapped 0.239 ETH and paid it to 0xf4b1..., which unwrapped
        # it; later it bought the WETH back. It kept nothing.
        f = fixture("payee_unwraps")
        legs = detect.legs_of(f["receipt"], f["meta"])
        self.assertEqual(detect.banked(f["receipt"], legs), {})

    def test_a_pool_drained_through_its_own_pricing_is_not_an_arbitrage(self):
        # 12 swaps in one Curve pool took 15.1 WETH and 13.0 wstETH out of it:
        # an exploit, with no second price to arbitrage against.
        f = fixture("single_pool_drain")
        legs = detect.legs_of(f["receipt"], f["meta"])
        self.assertEqual(len(legs), 12)
        self.assertFalse(detect.is_arbitrage(legs))

    def test_gas_includes_the_l1_fee(self):
        f = fixture("spike_855")
        r = f["receipt"]
        expected = int(r["gasUsed"], 16) * int(r["effectiveGasPrice"], 16) + int(r.get("l1Fee", "0x0"), 16)
        self.assertEqual(detect.gas_wei(r), expected)


if __name__ == "__main__":
    unittest.main()
