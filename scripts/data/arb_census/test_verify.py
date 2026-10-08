import json, unittest
from pathlib import Path
from arb_census import detect, verify

FIX = Path(__file__).resolve().parent / "fixtures"


def fixture(name):
    p = FIX / f"{name}.json"
    if not p.exists():
        raise unittest.SkipTest(f"fixture {name} not recorded yet")
    return json.loads(p.read_text())


class VerifyTest(unittest.TestCase):
    def test_the_spike_trades_legs_match_their_pools_transfers(self):
        f = fixture("spike_855")
        legs = detect.legs_of(f["receipt"], f["meta"])
        self.assertTrue(legs)
        self.assertTrue(all(verify.check_leg(l, f["receipt"]) for l in legs))

    def test_a_wrong_sign_fails(self):
        f = fixture("spike_855")
        leg = detect.legs_of(f["receipt"], f["meta"])[0]
        flipped = type(leg)(leg.tx, leg.block, leg.venue, leg.pool, {t: -x for t, x in leg.deltas.items()})
        self.assertFalse(verify.check_leg(flipped, f["receipt"]))

    def test_tessera_and_elfomo_amounts_appear_as_transfers(self):
        for name in ("tessera", "elfomo"):
            f = fixture(name)
            legs = detect.legs_of(f["receipt"], f["meta"])
            self.assertTrue(legs, name)
            self.assertTrue(all(verify.check_leg(l, f["receipt"]) for l in legs), name)


if __name__ == "__main__":
    unittest.main()
