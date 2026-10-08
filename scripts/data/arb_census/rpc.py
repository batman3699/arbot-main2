"""JSON-RPC to Base through BlockPI, for the census.

The key is read from .env and never printed: the client's repr hides the
endpoint, and transport errors are reported by type, never with the URL.
Requests are rate-limited because the shadow shares the endpoint.
"""
import json
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]


class RpcError(RuntimeError):
    pass


def endpoint(env_path=REPO / ".env") -> str:
    for line in Path(env_path).read_text().splitlines():
        if line.startswith("BLOCKPI_KEY="):
            key = line.split("=", 1)[1].strip().strip('"').strip("'")
            if key:
                return "https://base.blockpi.network/v1/rpc/" + key
    raise SystemExit("no BLOCKPI_KEY in .env")


class Rpc:
    def __init__(self, url=None, max_per_second=10.0):
        self._url = url or endpoint()
        self._gap = 1.0 / max_per_second
        self._next = 0.0
        self._lock = threading.Lock()

    def __repr__(self):
        return "Rpc(<endpoint hidden>)"

    def _post(self, body):
        with self._lock:
            now = time.monotonic()
            if self._next > now:
                time.sleep(self._next - now)
            self._next = max(now, self._next) + self._gap
        req = urllib.request.Request(
            self._url,
            data=json.dumps(body).encode(),
            headers={"Content-Type": "application/json", "User-Agent": "apex-arb-census/1"},
        )
        for attempt in range(4):
            try:
                with urllib.request.urlopen(req, timeout=90) as r:
                    return json.loads(r.read())
            except (urllib.error.URLError, TimeoutError, ConnectionError, json.JSONDecodeError) as e:
                if attempt == 3:
                    raise RpcError(f"transport failed: {type(e).__name__}") from None
                time.sleep(2 ** attempt)

    def call(self, method, params):
        out = self._post({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})
        if "error" in out:
            raise RpcError(str(out["error"].get("message", "rpc error")))
        return out["result"]

    def batch(self, calls, size=50):
        """Each call's result, or None where it failed, in order."""
        res = []
        for i in range(0, len(calls), size):
            chunk = calls[i:i + size]
            body = [{"jsonrpc": "2.0", "id": j, "method": m, "params": p} for j, (m, p) in enumerate(chunk)]
            try:
                out = self._post(body)
                if not isinstance(out, list):
                    raise RpcError("no batch")
                byid = {o.get("id"): o.get("result") for o in out}
                res.extend(byid.get(j) for j in range(len(chunk)))
            except RpcError:
                for m, p in chunk:
                    try:
                        res.append(self.call(m, p))
                    except RpcError:
                        res.append(None)
        return res
