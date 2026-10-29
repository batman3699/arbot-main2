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


    def test_a_body_cut_short_is_retried_like_any_transport_failure(self):
        # 2026-10-29 00:20 UTC: a public endpoint cut a 900 kB answer short,
        # http.client.IncompleteRead escaped the retry, and the day's census died.
        import http.client
        from unittest import mock

        class Answer:
            def __init__(self, body):
                self.body = body
            def __enter__(self):
                return self
            def __exit__(self, *a):
                return False
            def read(self):
                if self.body is None:
                    raise http.client.IncompleteRead(b"partial")
                return self.body

        answers = [Answer(None), Answer(b'{"jsonrpc":"2.0","id":1,"result":"0x1"}')]
        client = rpc.Rpc(url="https://rpc.example", max_per_second=1e6)
        with mock.patch.object(rpc.urllib.request, "urlopen", side_effect=lambda *a, **k: answers.pop(0)), \
                mock.patch.object(rpc.time, "sleep"):
            self.assertEqual(client.call("eth_blockNumber", []), "0x1")
        with mock.patch.object(rpc.urllib.request, "urlopen", side_effect=lambda *a, **k: Answer(None)), \
                mock.patch.object(rpc.time, "sleep"):
            with self.assertRaises(rpc.RpcError) as e:
                client.call("eth_blockNumber", [])
            self.assertEqual(str(e.exception), "transport failed: IncompleteRead")


if __name__ == "__main__":
    unittest.main()
