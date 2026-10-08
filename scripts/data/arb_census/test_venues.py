import unittest
from arb_census import venues as v

T0 = "0x" + "11" * 20
T1 = "0x" + "22" * 20
POOL = "0x" + "aa" * 20


def word(x):
    return format(x % (1 << 256), "064x")


def addr_word(a):
    return a[2:].rjust(64, "0")


def log(address, topics, words, tx="0x01", block=1):
    return {"address": address, "topics": topics, "data": "0x" + "".join(words),
            "transactionHash": tx, "blockNumber": hex(block)}


class DecodeTest(unittest.TestCase):
    def test_a_v3_swap_is_the_pools_own_signed_amounts(self):
        l = log(POOL, [v.UNI_V3, "0x" + "0" * 64, "0x" + "0" * 64], [word(500), word(-300), word(0), word(0), word(0)])
        leg = v.decode(l, {POOL: [T0, T1, "uniswap_v3"]})
        self.assertEqual(leg.deltas, {T0: 500, T1: -300})
        self.assertEqual(leg.venue, "uniswap_v3")

    def test_a_v2_swap_is_in_minus_out(self):
        l = log(POOL, [v.UNI_V2, "0x" + "0" * 64, "0x" + "0" * 64], [word(0), word(70), word(40), word(0)])
        self.assertEqual(v.decode(l, {POOL: [T0, T1, "uniswap_v2"]}).deltas, {T0: -40, T1: 70})

    def test_a_v4_swap_is_negated_and_native_eth_is_weth(self):
        pid = "0x" + "bb" * 32
        l = log(v.POOL_MANAGER, [v.UNI_V4, pid, "0x" + "0" * 64], [word(-1000), word(900), word(0), word(0), word(0), word(0)])
        leg = v.decode(l, {pid: [v.ZERO, T1, "uniswap_v4"]})
        self.assertEqual(leg.deltas, {v.WETH: 1000, T1: -900})

    def test_tessera_carries_its_tokens(self):
        l = log(v.TESSERA_SWAP, [v.TESSERA], [addr_word(T0), addr_word(T1), word(10), word(9), addr_word(POOL)])
        leg = v.decode(l, {})
        self.assertEqual((leg.venue, leg.deltas), ("tessera", {T0: 10, T1: -9}))

    def test_elfomo_carries_its_tokens(self):
        l = log(v.ELFOMO_SWAP, [v.ELFOMO, "0x" + "0" * 64, "0x" + "0" * 64],
                [addr_word(POOL), addr_word(POOL), addr_word(T0), addr_word(T1), word(10), word(9)])
        self.assertEqual(v.decode(l, {}).deltas, {T0: 10, T1: -9})

    def test_maverick_follows_the_side_paid_in(self):
        l = log(POOL, [v.MAVERICK_V2], [addr_word(POOL), addr_word(POOL), word(10), word(0), word(0), word(0), word(10), word(8)])
        self.assertEqual(v.decode(l, {POOL: [T0, T1, "maverick_v2"]}).deltas, {T1: 10, T0: -8})

    def test_fluid_follows_its_direction(self):
        l = log(POOL, [v.FLUID], [word(1), word(10), word(8), addr_word(POOL)])
        self.assertEqual(v.decode(l, {POOL: [T0, T1, "fluid"]}).deltas, {T0: 10, T1: -8})

    def test_curve_reads_its_coins(self):
        l = log(POOL, [v.CURVE_I128, "0x" + "0" * 64], [word(1), word(10), word(0), word(9)])
        self.assertEqual(v.decode(l, {POOL: {"coins": [T0, T1], "venue": "curve"}}).deltas, {T1: 10, T0: -9})

    def test_metric_unpacks_both_amounts_from_one_word(self):
        # token0's pool-side change in the high 128 bits, token1's in the low
        # 128, each two's-complement: here +700 and -650.
        packed = ((700 % (1 << 128)) << 128) | ((-650) % (1 << 128))
        l = log(POOL, [v.METRIC, "0x" + "0" * 64, "0x" + "0" * 64], [word(0), format(packed, "064x"), word(0)])
        self.assertEqual(v.decode(l, {POOL: [T0, T1, "metric"]}).deltas, {T0: 700, T1: -650})

    def test_a_pool_without_metadata_is_not_decoded(self):
        l = log(POOL, [v.UNI_V3, "0x" + "0" * 64, "0x" + "0" * 64], [word(1), word(-1), word(0), word(0), word(0)])
        self.assertIsNone(v.decode(l, {}))

    def test_a_tessera_event_from_another_contract_is_ignored(self):
        l = log(POOL, [v.TESSERA], [addr_word(T0), addr_word(T1), word(10), word(9), addr_word(POOL)])
        self.assertIsNone(v.decode(l, {}))


if __name__ == "__main__":
    unittest.main()
