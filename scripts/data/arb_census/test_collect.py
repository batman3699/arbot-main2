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


if __name__ == "__main__":
    unittest.main()
