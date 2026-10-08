import tempfile, unittest
from pathlib import Path
from arb_census import rpc


class RpcTest(unittest.TestCase):
    def test_the_endpoint_comes_from_env_and_the_repr_hides_it(self):
        with tempfile.TemporaryDirectory() as d:
            env = Path(d) / ".env"
            env.write_text("OTHER=1\nBLOCKPI_KEY='abc123secret'\n")
            url = rpc.endpoint(env)
            self.assertTrue(url.endswith("/abc123secret"))
            self.assertNotIn("abc123secret", repr(rpc.Rpc(url=url)))

    def test_a_missing_key_is_refused(self):
        with tempfile.TemporaryDirectory() as d:
            env = Path(d) / ".env"
            env.write_text("OTHER=1\n")
            with self.assertRaises(SystemExit):
                rpc.endpoint(env)


if __name__ == "__main__":
    unittest.main()
