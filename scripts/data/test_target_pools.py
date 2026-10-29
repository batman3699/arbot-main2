"""R37: a targeted pool the inventories do not hold gets its record, read
from the pool and proven by its own factory; a pool whose factory is not one
of ours, or which its factory does not name, is refused.

Run from scripts/data: python3 -m unittest test_target_pools
"""
import json
import os
import tempfile
import unittest

import target_pools as T

AXIS = "0x5be75e3b0aff5d1a042a5363ae36acb1d6d755ea"
USDC = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"
POOL = "0xe1bfc5ada1125718b53b2affc638ba9981f6070f"
AERO = "0xffd4ec4bd2211cbfd58c209fdecc65f63f2b9e4c"
UNI_FACTORY = "0x33128a8fc17869897dce68ed026d694621f6fdfd"
AERO_FACTORY = "0x420dd381b31aef6683db6b902084cb0ffece40da"


def word(v) -> str:
    return "0x" + (v[2:].rjust(64, "0") if isinstance(v, str) else "%064x" % v)


def sqrt_at(tick: int) -> int:
    """A tick's sqrtPriceX96, by floating point: close enough to price a depth."""
    return int((1.0001 ** tick) ** 0.5 * 2**96)


def chain(answers):
    """A node answering eth_call from `answers`, keyed (to, data)."""
    def call(method, params):
        assert method == "eth_call"
        q = params[0]
        return answers.get((q["to"], q["data"]), "0x")
    return call


def uni(pool=POOL, named=POOL, factory=UNI_FACTORY):
    return {
        (pool, "0xc45a0155"): word(factory),
        (pool, "0x0dfe1681"): word(AXIS),
        (pool, "0xd21220a7"): word(USDC),
        (pool, "0xddca3f43"): word(500),
        (UNI_FACTORY, "0x1698ee82" + word(AXIS)[2:] + word(USDC)[2:] + word(500)[2:]): word(named),
        (USDC, "0x70a08231" + word(pool)[2:]): word(250_000_000_000),   # $250,000 of USDC
    }


class Records(unittest.TestCase):
    def test_a_targeted_v3_pool_gets_its_record(self):
        venue, rec = T.record_for(chain(uni()), POOL)
        self.assertEqual(venue, "uniswap_v3")
        self.assertEqual(rec, {"pool": POOL, "token0": AXIS, "token1": USDC, "fee": 500, "created_block": 0,
                               "fee_ppm_onchain": 500, "targeted": True,
                               "hub_usd_liquidity": 250000.0, "hub_symbol": "USDC"})

    def test_an_aerodrome_v2_pool_carries_its_curve_and_fee(self):
        answers = {
            (AERO, "0xc45a0155"): word(AERO_FACTORY),
            (AERO, "0x0dfe1681"): word(AXIS),
            (AERO, "0xd21220a7"): word(USDC),
            (AERO, "0x22be3de1"): word(0),
            (AERO_FACTORY, "0x79bc57d5" + word(AXIS)[2:] + word(USDC)[2:] + word(0)[2:]): word(AERO),
            (AERO_FACTORY, "0xcc56b2c5" + word(AERO)[2:] + word(0)[2:]): word(30),
            (USDC, "0x70a08231" + word(AERO)[2:]): word(1_500_000),
        }
        venue, rec = T.record_for(chain(answers), AERO)
        self.assertEqual(venue, "aerodrome_v2")
        self.assertEqual(rec, {"pool": AERO, "token0": AXIS, "token1": USDC, "stable": False, "fee": 30,
                               "fee_ppm_onchain": 3000, "created_block": None, "targeted": True,
                               "hub_usd_liquidity": 1.5, "hub_symbol": "USDC"})

    def test_a_weth_pool_s_depth_is_its_weth_at_the_mid_price(self):
        # Every record ranks in the legacy loader only with a depth and its
        # hub (memory: geckoterminal-tvl-in-hub-field). WETH at the WETH/USDC
        # pool's price: tick -197,350 is about $2,688.
        weth = "0x4200000000000000000000000000000000000006"
        answers = {
            (T.WETH_USDC_UNI, "0x3850c7bd"): word(sqrt_at(-197_350)),
            (weth, "0x70a08231" + word(POOL)[2:]): word(2 * 10**18),
        }
        hub = T.depth(chain(answers), POOL, weth, AXIS)
        self.assertEqual(hub[1], "WETH")
        self.assertTrue(5_370 < hub[0] < 5_380, hub)
        self.assertIsNone(T.depth(chain({}), POOL, AXIS, "0x" + "44" * 20), "no hub token: no depth")

    def test_a_pool_its_factory_does_not_name_is_refused(self):
        # A contract that answers factory() with ours, but is not its pool.
        self.assertEqual(T.record_for(chain(uni(named="0x" + "99" * 20)), POOL), (None, "not its factory's pool"))

    def test_an_unknown_factory_is_refused(self):
        self.assertEqual(T.record_for(chain(uni(factory="0x" + "77" * 20)), POOL), (None, "not one of our venues' factories"))


class Fold(unittest.TestCase):
    def test_only_missing_pools_are_added_once(self):
        with tempfile.TemporaryDirectory() as d:
            os.makedirs(os.path.join(d, "uniswap_v3"))
            held = {"pool": "0x" + "11" * 20, "token0": AXIS, "token1": USDC, "fee": 100}
            with open(os.path.join(d, "uniswap_v3", "pools.jsonl"), "w") as f:
                f.write(json.dumps(held) + "\n")
            targets = {"routes": [{"pools": [POOL, "0x" + "11" * 20]}, {"pools": ["0x" + "c1" * 32, POOL]}]}
            self.assertEqual(T.missing(targets, d), [POOL], "held pools and v4 IDs are not asked")
            report = T.add(chain(uni()), targets, d)
            self.assertEqual(report, {"added": {"uniswap_v3": [POOL]}, "refused": {}})
            with open(os.path.join(d, "uniswap_v3", "pools.jsonl")) as f:
                rows = [json.loads(l) for l in f if l.strip()]
            self.assertEqual([r["pool"] for r in rows], ["0x" + "11" * 20, POOL])
            self.assertEqual(T.add(chain(uni()), targets, d), {"added": {}, "refused": {}}, "a second run adds nothing")


if __name__ == "__main__":
    unittest.main()
