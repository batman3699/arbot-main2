"""Minimal keccak-256 (the pre-NIST padding Ethereum uses), pure stdlib.

Ethereum needs keccak-256, which is NOT hashlib's sha3_256: the two differ only
in the domain-separation byte (0x01 here, 0x06 for SHA3), but that changes every
digest. No keccak package is installed in this environment, so rather than add a
dependency for a few function selectors, this implements keccak-f[1600] directly.

`selftest()` checks it against selectors already verified against live contracts
elsewhere in this repo (balanceOf, fee, tickSpacing, token0) plus the empty-input
vector, so a wrong implementation cannot go unnoticed.
"""

_RC = [
    0x0000000000000001, 0x0000000000008082, 0x800000000000808A, 0x8000000080008000,
    0x000000000000808B, 0x0000000080000001, 0x8000000080008081, 0x8000000000008009,
    0x000000000000008A, 0x0000000000000088, 0x0000000080008009, 0x000000008000000A,
    0x000000008000808B, 0x800000000000008B, 0x8000000000008089, 0x8000000000008003,
    0x8000000000008002, 0x8000000000000080, 0x000000000000800A, 0x800000008000000A,
    0x8000000080008081, 0x8000000000008080, 0x0000000080000001, 0x8000000080008008,
]
_ROT = [
    [0, 36, 3, 41, 18], [1, 44, 10, 45, 2], [62, 6, 43, 15, 61],
    [28, 55, 25, 21, 56], [27, 20, 39, 8, 14],
]
_MASK = (1 << 64) - 1


def _rotl(x, n):
    return ((x << n) | (x >> (64 - n))) & _MASK


def _keccak_f(a):
    for rnd in range(24):
        c = [a[x][0] ^ a[x][1] ^ a[x][2] ^ a[x][3] ^ a[x][4] for x in range(5)]
        d = [c[(x - 1) % 5] ^ _rotl(c[(x + 1) % 5], 1) for x in range(5)]
        for x in range(5):
            for y in range(5):
                a[x][y] ^= d[x]
        b = [[0] * 5 for _ in range(5)]
        for x in range(5):
            for y in range(5):
                b[y][(2 * x + 3 * y) % 5] = _rotl(a[x][y], _ROT[x][y])
        for x in range(5):
            for y in range(5):
                a[x][y] = b[x][y] ^ ((~b[(x + 1) % 5][y] & _MASK) & b[(x + 2) % 5][y])
        a[0][0] ^= _RC[rnd]
    return a


def keccak256(data: bytes) -> bytes:
    rate = 136  # 1088 bits, the rate for 256-bit output
    # Ethereum's keccak pads with 0x01, where SHA3-256 uses 0x06.
    padded = bytearray(data) + b"\x01" + b"\x00" * ((-len(data) - 1) % rate)
    padded[-1] |= 0x80
    a = [[0] * 5 for _ in range(5)]
    for off in range(0, len(padded), rate):
        blk = padded[off:off + rate]
        for i in range(rate // 8):
            x, y = i % 5, i // 5
            a[x][y] ^= int.from_bytes(blk[8 * i:8 * i + 8], "little")
        a = _keccak_f(a)
    out = bytearray()
    while len(out) < 32:
        for i in range(rate // 8):
            x, y = i % 5, i // 5
            out += a[x][y].to_bytes(8, "little")
            if len(out) >= 32:
                break
        else:
            a = _keccak_f(a)
            continue
        break
    return bytes(out[:32])


def selector(signature: str) -> str:
    return "0x" + keccak256(signature.encode()).hex()[:8]


def topic0(signature: str) -> str:
    return "0x" + keccak256(signature.encode()).hex()


def selftest():
    """Against selectors already confirmed against live Base contracts."""
    assert keccak256(b"").hex() == (
        "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"
    ), "empty-input vector"
    for sig, want in [
        ("balanceOf(address)", "0x70a08231"),
        ("fee()", "0xddca3f43"),
        ("tickSpacing()", "0xd0c93a7c"),
        ("token0()", "0x0dfe1681"),
        ("token1()", "0xd21220a7"),
        ("liquidity()", "0x1a686502"),
        ("aggregate3((address,bool,bytes)[])", "0x82ad56cb"),
    ]:
        got = selector(sig)
        assert got == want, f"{sig}: got {got}, want {want}"
    return True


if __name__ == "__main__":
    selftest()
    print("keccak selftest OK")
