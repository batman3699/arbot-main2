"""Record the census tests' fixtures: each transaction's receipt and the
tokens and venue of every pool its swaps touch, read from Base. Run once;
the files are committed so the tests need no network."""
import json
from pathlib import Path

from arb_census.rpc import Rpc

FIXTURES = {
    "spike_855": "0xe8c6c2802e2fbd55b883a3a048ef95ce2f8d2b9643c8059493de0fbec3ff0469",
    "omi_10hop": "0xd7b4462124903cde0ddaf0601be5789dae87d602812f3d3cc4a0aab15831342f",
    "v4_2hop": "0x3dbfd72f53c0239f9e1074a67c8cd5742c59231bebc34ff1c612e9c3a6967d89",
    "tessera": "0x85d0ae824691a6eb43f4b9cc64ef8e1f4e7b017ce436c1d1b21f61d0d70f8e8f",
    "elfomo": "0x128acc59243c77034868bfde1db0ea0c131ca1036bfca0386733e90e0d80badd",
    "user_route": "0x0a801f75b94e7501133eb742233ae374d21e02fc6083dca50224ec1050ca6c13",
    # Pays native ETH into a Uniswap v4 pool: no Transfer log carries it.
    "native_v4": "0x3e548fc63a4eceb533021056ddc3331d696881e07f18621f15e35384b3480aeb",
    # An executor that is neither sender nor target unwraps WETH and pays the
    # ETH into a v4 pool; the profit goes to a third address.
    "native_in_by_executor": "0x38803fccf4344f9a919562c3dd21c2a5e98b410f0864d57ef8fe1e58bdc983b0",
    # The mirror: a v4 pool pays ETH that a third-party executor wraps.
    "native_out_by_executor": "0x9175d5ac765351261c45570a8c5e70b28e0afb29446c39060e588f462f0af41a",
    # The executor keeps the profit and unwraps it; the target pays a fee.
    "profit_at_executor": "0x8a90e4098bd6520fb56132febee824d2ca9fca558e95de9ed6687fc76c46b7bc",
}
OUT = Path(__file__).resolve().parent / "fixtures"


def main():
    from arb_census.collect import Resolver
    rpc = Rpc()
    OUT.mkdir(exist_ok=True)
    resolver = Resolver(rpc)
    for name, tx in FIXTURES.items():
        receipt = rpc.call("eth_getTransactionReceipt", [tx])
        meta = resolver.meta_for_logs(receipt["logs"])
        (OUT / f"{name}.json").write_text(json.dumps({"tx": tx, "receipt": receipt, "meta": meta}, indent=1))
        print(name, len(receipt["logs"]), "logs,", len(meta), "pools", flush=True)


if __name__ == "__main__":
    main()
