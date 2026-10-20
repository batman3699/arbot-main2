"""R30's pool refresh: the records it writes, how it merges them into an
inventory, and the universe count it reports, which must follow the runtime's
loader (crates/apex-runtime/src/live/inventory.rs).

Run from scripts/data: python3 -m unittest test_refresh_weth_pools
"""
import json
import unittest

import refresh_weth_pools as R

WETH = R.WETH
TOK = "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf"  # cbBTC, above WETH
LOW = "0x04c0599ae5a44757c0af6f9ec3b93da8976c150a"  # weETH, below WETH
POOL_A = "0x9520e1a3bfd86da6c1e9e5ee4b9c2f11c413358f"
POOL_B = "0x3f0296bf652e19bca772ec3df08b32732f93014a"
ETH_USD = 2500.0


def found(pool, venue, tier, fee_ppm, weth_wei, token=TOK):
    return R.Found(pool=pool, venue=venue, token=token, tier=tier, fee_ppm=fee_ppm, weth_wei=weth_wei)


class Records(unittest.TestCase):
    def test_a_uniswap_record_carries_its_tier_and_the_weth_side(self):
        r = R.record(found(POOL_A, "uniswap_v3", 500, 500, 80 * 10**18), ETH_USD, block=52_800_000)
        # created_block 0, "not read": the legacy loader needs a number there
        # (arb-exec-legacy pool_store), and the runtime does not read it.
        self.assertEqual(r, {"pool": POOL_A, "token0": WETH, "token1": TOK, "fee": 500, "created_block": 0,
                             "hub_usd_liquidity": 200000.0, "hub_symbol": "WETH", "fee_ppm_onchain": 500})

    def test_tokens_are_in_the_pools_order(self):
        r = R.record(found(POOL_A, "pancakeswap_v3", 2500, 2500, 10**18, token=LOW), ETH_USD, block=1)
        self.assertEqual((r["token0"], r["token1"]), (LOW, WETH))

    def test_a_slipstream_record_keeps_its_spacing_and_the_fee_it_measured(self):
        r = R.record(found(POOL_A, "aerodrome_slipstream_v3", 100, 400, 10**18), ETH_USD, block=52_800_123)
        self.assertEqual((r["fee"], r["fee_ppm_onchain"], r["fee_measured_block"]), (100, 400, 52_800_123))

    def test_an_aerodrome_record_is_volatile_with_its_fee_in_basis_points(self):
        r = R.record(found(POOL_A, "aerodrome_v2", None, 3000, 10**18), ETH_USD, block=1)
        self.assertEqual((r["stable"], r["fee"], r["fee_ppm_onchain"]), (False, 30, 3000))
        self.assertIsNone(r["created_block"], "as every Aerodrome v2 record has it")


class DynamicFees(unittest.TestCase):
    def test_a_dynamic_fee_is_the_median_of_its_readings(self):
        # Three WETH/VVV Slipstream pools read 3,216-3,222 ppm at one block and
        # 2,700-2,750 a week earlier: one reading must not decide admission.
        self.assertEqual(R.median_fee([3222, 2700, 2750, 2690, 2800]), 2750)
        self.assertEqual(R.median_fee([400]), 400)

    def test_the_readings_span_two_hours_before_the_pinned_block(self):
        self.assertEqual(R.fee_blocks(52_835_268), [52_835_268, 52_834_368, 52_833_468, 52_832_568, 52_831_668])


class Merge(unittest.TestCase):
    def existing(self):
        return [
            {"pool": POOL_A.upper().replace("0X", "0x"), "token0": WETH, "token1": TOK, "fee": 500, "created_block": 7,
             "hub_usd_liquidity": 900000.0, "hub_symbol": "WETH", "fee_ppm_onchain": 500},
            {"pool": "0x1111111111111111111111111111111111111111", "token0": WETH, "token1": TOK, "fee": 3000,
             "created_block": 9, "hub_usd_liquidity": 5.0, "hub_symbol": "WETH", "fee_ppm_onchain": 3000},
        ]

    def test_a_record_found_again_takes_the_new_depth_and_keeps_its_place(self):
        out, summary = R.merge(self.existing(), [found(POOL_A, "uniswap_v3", 500, 500, 20 * 10**18)], ETH_USD, block=5)
        self.assertEqual(out[0]["hub_usd_liquidity"], 50000.0)
        self.assertEqual(out[0]["created_block"], 7, "what the record knew is kept")
        self.assertEqual(out[1]["hub_usd_liquidity"], 5.0, "a record not found again is left alone")
        self.assertEqual((summary.updated, summary.added, summary.kept), (1, 0, 1))

    def test_new_pools_follow_deepest_first_and_once(self):
        new = [found(POOL_B, "uniswap_v3", 100, 100, 10**18), found("0x2222222222222222222222222222222222222222", "uniswap_v3", 3000, 3000, 5 * 10**18),
               found(POOL_B, "uniswap_v3", 100, 100, 10**18)]
        out, summary = R.merge(self.existing(), new, ETH_USD, block=5)
        self.assertEqual([r["pool"] for r in out[2:]], ["0x2222222222222222222222222222222222222222", POOL_B])
        self.assertEqual(summary.added, 2)

    def test_a_pool_measured_under_1000_dollars_is_not_kept(self):
        # 561 of the Uniswap pools found held no WETH at all; WETH/bsdETH was
        # drained. Neither can be admitted at any floor in use, and the legacy
        # ranker refuses a shipped record under $1.
        new = [found(POOL_B, "uniswap_v3", 100, 100, 10**14)]  # $0.25
        empty = [found(POOL_A, "uniswap_v3", 500, 500, 0)]
        out, summary = R.merge(self.existing(), new + empty, ETH_USD, block=5)
        self.assertEqual([r["pool"].lower() for r in out], ["0x1111111111111111111111111111111111111111"])
        self.assertEqual((summary.added, summary.removed, summary.kept), (0, 1, 1))

    def test_a_weth_pool_found_again_is_measured_on_its_weth_side_whatever_hub_it_named(self):
        # 37 Slipstream WETH records named no hub (scraped depths), and some
        # named USDC, cbBTC or AUKI: one rule, read on chain, for every one.
        for hub in ("cbBTC", None):
            rows = self.existing()
            if hub is None:
                del rows[0]["hub_symbol"]
            else:
                rows[0]["hub_symbol"] = hub
            out, summary = R.merge(rows, [found(POOL_A, "uniswap_v3", 500, 500, 10**18)], ETH_USD, block=5)
            self.assertEqual((out[0]["hub_usd_liquidity"], out[0]["hub_symbol"]), (2500.0, "WETH"), hub)
            self.assertEqual(summary.updated, 1)


class Universe(unittest.TestCase):
    """The runtime's rules: fee 1..max, depth >= min, Aerodrome volatile only, a
    Slipstream record only with a measured fee, then pairs of two or more pools,
    and only WETH pairs make cycles."""

    def test_the_loader_rules_and_the_pairs(self):
        def rec(pool, t1, fee, ppm, usd, **kw):
            r = {"pool": pool, "token0": WETH, "token1": t1, "fee": fee, "hub_usd_liquidity": usd}
            if ppm is not None:
                r["fee_ppm_onchain"] = ppm
            r.update(kw)
            return r
        inv = {
            "uniswap_v3": [rec("0xa1", TOK, 500, None, 2e5), rec("0xa2", TOK, 10000, None, 9e6), rec("0xa3", LOW, 500, None, 9e6)],
            "pancakeswap_v3": [rec("0xb1", TOK, 2500, 2500, 1e5)],
            "aerodrome_slipstream": [rec("0xc1", TOK, 100, None, 9e6), rec("0xc2", LOW, 100, 400, 9e6)],
            "aerodrome_slipstream_v3": [],
            "aerodrome_v2": [rec("0xd1", TOK, 30, 3000, 9e6, stable=True), rec("0xd2", TOK, 30, 3000, 99_999.0, stable=False)],
        }
        pools, cycles = R.universe(inv, max_fee_ppm=3000, min_depth_usd=100_000)
        # TOK: 0xa1 and 0xb1 (0xa2's fee, 0xc1's unmeasured fee, 0xd1's curve and
        # 0xd2's depth leave them out); LOW: 0xa3 and 0xc2.
        self.assertEqual(sorted(pools), ["0xa1", "0xa3", "0xb1", "0xc2"])
        self.assertEqual(cycles, 4, "two directions for each of two pairs")


if __name__ == "__main__":
    unittest.main()
