import json
import tempfile
import unittest
from pathlib import Path

from arb_census.collect import Resolver

QUICKSWAP_FACTORY = "0xc5396866754799b9720125b104ae01d935ab9c7b"
UNNAMED_FACTORY = "0x" + "ab" * 20


class ResolverCacheTest(unittest.TestCase):
    def test_a_cached_pool_takes_its_factorys_name_once_the_factory_is_named(self):
        """A pool resolved before its factory had a venue name was cached as
        `factory:<address>`; naming the factory later must reach it."""
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "meta.json"
            path.write_text(json.dumps({
                "0x01": ["0xa", "0xb", f"factory:{QUICKSWAP_FACTORY}"],
                "0x02": ["0xa", "0xb", f"factory:{UNNAMED_FACTORY}"],
            }))
            meta = Resolver(rpc=None, cache_path=path).meta
        self.assertEqual(meta["0x01"][2], "quickswap")
        self.assertEqual(meta["0x02"][2], f"factory:{UNNAMED_FACTORY}")

    def test_slipstreams_third_factory_has_a_name(self):
        # R44: 640 pools and 45% of Base's arbitrage value were `factory:0x9cb2...`.
        from arb_census import venues as V
        self.assertEqual(V.VENUE_OF_FACTORY["0x9cb220581b91b0c83223012fe69688b92ad07a76"], "slipstream_9cb2")
        self.assertTrue(V.holds_at_emitter("slipstream_9cb2"))


if __name__ == "__main__":
    unittest.main()
