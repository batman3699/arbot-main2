import unittest
from arb_census import report as R

H = 1800  # blocks an hour


def arb(block, usd, venues, sender="0xs", pairs=(("0xa", "0xb"),), mm=False):
    return {"tx": f"0x{block}{usd}", "block": block, "venues": list(venues), "pools": [], "pairs": [list(p) for p in pairs],
            "hops": 2, "est_usd": usd, "banked_usd": usd, "valued_by": "banked", "gas_usd": 0.01, "sender": sender, "mm": mm}


class ReportTest(unittest.TestCase):
    def test_value_is_what_was_banked_after_costs(self):
        self.assertEqual(R.value({"est_usd": 15.0, "banked_usd": 3.0, "valued_by": "banked"}), 3.0)
        self.assertEqual(R.value({"est_usd": 3.0, "banked_usd": 0.0, "valued_by": "pool"}), 3.0)
        self.assertEqual(R.value({"est_usd": 3.0}), 3.0)

    def test_value_never_exceeds_what_the_pools_gave_up(self):
        # A bot keeps no more than its legs took from the pools: banked above
        # that is someone else's flow counted as the bot's (ETH moves without
        # logs; 0x2d20f209... read $9,131 against $0.55).
        self.assertEqual(R.value({"est_usd": 0.55, "banked_usd": 9131.06, "valued_by": "banked"}), 0.55)

    def test_a_spike_hour_is_five_times_the_median(self):
        arbs = [arb(h * H + 1, 10.0, ["uniswap_v3"]) for h in range(10)] + [arb(5 * H + 2, 60.0, ["uniswap_v3"])]
        self.assertEqual(R.spike_hours(arbs, 0, 10 * H - 1), {5})

    def test_unlock_counts_arbitrage_needing_only_that_venue(self):
        arbs = [arb(1, 10.0, ["uniswap_v3", "uniswap_v4"]), arb(2, 5.0, ["uniswap_v3"]), arb(3, 7.0, ["uniswap_v4", "curve"])]
        rows = {tuple(r["add"]): r for r in R.unlock_table(arbs, set(), 1)}
        self.assertEqual(rows[("uniswap_v4",)]["gross"], 10.0)
        self.assertEqual(rows[("curve", "uniswap_v4")]["gross"], 17.0)

    def test_unlock_combines_only_the_venues_most_arbitrage_needs(self):
        # 182 venues outside ours in a real week make ~1M triples; only the
        # venues with the most value needing them are combined.
        arbs = [arb(i, float(i + 1), ["uniswap_v3", f"v{i:02d}"]) for i in range(R.UNLOCK_CANDIDATES + 5)]
        rows = R.unlock_table(arbs, set(), 1)
        seen = {v for r in rows for v in r["add"]}
        self.assertEqual(len(seen), R.UNLOCK_CANDIDATES)
        self.assertNotIn("v00", seen)

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
