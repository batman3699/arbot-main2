"""R35: how the arbitrage taken in its trigger's own flashblock gets there.

Run from scripts/data: python3 -m unittest arb_census.test_offset0
"""
import unittest

from arb_census import offset0 as O


def tx(prio=None, fee=None, price=None, frm="0xa", to="0xb", typ="0x2"):
    t = {"from": frm, "to": to, "type": typ}
    if typ == "0x0":
        t["gasPrice"] = hex(price)
    else:
        t["maxPriorityFeePerGas"], t["maxFeePerGas"] = hex(prio), hex(fee)
    return t


class Priority(unittest.TestCase):
    def test_what_a_transaction_pays_over_the_base_fee(self):
        self.assertEqual(O.tx_priority(tx(prio=7, fee=1_000), 100), 7)
        self.assertEqual(O.tx_priority(tx(prio=700, fee=150), 100), 50, "capped by the fee cap")
        self.assertEqual(O.tx_priority(tx(price=130, typ="0x0"), 100), 30, "a legacy transaction pays its price")
        self.assertEqual(O.tx_priority(tx(price=90, typ="0x0"), 100), 0)

    def test_a_deposit_has_no_priority(self):
        self.assertIsNone(O.tx_priority({"type": "0x7e", "from": "0xa", "to": "0xb"}, 100))


class Mechanism(unittest.TestCase):
    def test_the_signs_of_how_an_arbitrage_followed_its_trigger(self):
        arb = {"txi": 11, "prio": 50, "sender": "0xs", "to": "0xbot"}
        m = O.mechanism(arb, {"txi": 10, "prio": 20, "from": "0xu", "to": "0xrouter"})
        self.assertEqual(m, {"adjacent": True, "outbid_trigger": True, "same_bot": False})
        m = O.mechanism(arb, {"txi": 4, "prio": 60, "from": "0xS", "to": "0xother"})
        self.assertEqual(m, {"adjacent": False, "outbid_trigger": False, "same_bot": True})
        self.assertTrue(O.mechanism(arb, {"txi": 4, "prio": 60, "from": "0xu", "to": "0xBOT"})["same_bot"])
        self.assertFalse(O.mechanism(arb, {"txi": 10, "prio": 50, "from": "0xu", "to": "0xr"})["outbid_trigger"], "a tie is not outbidding")


class Ordering(unittest.TestCase):
    def test_a_flashblock_out_of_priority_order_is_counted_pair_by_pair(self):
        self.assertEqual(O.inversions([9, 7, 7, 3]), 0)
        self.assertEqual(O.inversions([9, 7, 8, 3, 5]), 2)
        self.assertEqual(O.inversions([None, 9, None, 10]), 1, "deposits are skipped")


class Bots(unittest.TestCase):
    def test_bots_ranked_by_what_they_took(self):
        rows = [{"to": "0xb1", "usd": 5.0, "prio": 10, "gas": 200_000, "outbid_trigger": True, "adjacent": True, "same_bot": False},
                {"to": "0xb1", "usd": 3.0, "prio": 30, "gas": 400_000, "outbid_trigger": False, "adjacent": True, "same_bot": False},
                {"to": "0xb2", "usd": 9.0, "prio": 5, "gas": 100_000, "outbid_trigger": False, "adjacent": False, "same_bot": True}]
        r = O.bots(rows)
        self.assertEqual([b["to"] for b in r], ["0xb2", "0xb1"])
        self.assertEqual((r[1]["arbs"], r[1]["usd"], r[1]["share"]), (2, 8.0, 8.0 / 17.0))
        self.assertEqual((r[1]["outbid_trigger"], r[1]["adjacent"], r[1]["same_bot"]), (0.5, 1.0, 0.0))
        self.assertEqual((r[1]["prio_p50"], r[1]["gas_p50"]), (30, 400_000))


if __name__ == "__main__":
    unittest.main()
