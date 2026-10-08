"""Each venue's trade event, decoded into what the venue's side gained and
lost of each token (R23).

A `Leg` is one trade: `deltas[token]` is positive when the venue received the
token and negative when it paid it out. Native ETH is folded into WETH.
`decode` returns None for a log it cannot attribute: an unknown pool, or a
fixed-contract venue's event emitted by some other contract.
"""
from dataclasses import dataclass, field

WETH = "0x4200000000000000000000000000000000000006"
USDC = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"
ZERO = "0x" + "0" * 40
# Native ETH, as Uniswap v4 (the zero address) and Curve and Fluid (0xEeee...)
# name it. No Transfer log carries it, so each leg also records how much of it
# the venue took or paid (`Leg.native`), for `detect.banked`.
NATIVE = frozenset({ZERO, "0x" + "e" * 40})

UNI_V3 = "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67"
PANCAKE_V3 = "0x19b47279256b2a23a1665c810c8d55a1758940ee09377d4f8d26497a3577dc83"
ALGEBRA_INTEGRAL = "0x121cb44ee54098b1a04743c487e7460d8dd429b27f88b1f4d4767396e1a59f79"
UNI_V2 = "0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822"
AERO_V2 = "0xb3e2773606abfd36b5bd91394b3a54d1398336c65005baf7bf7a05efeffaf75b"
UNI_V4 = "0x40e9cecb9f5f1f1c5b9c97dec2917b7ee92e57ba5563708daca94dd84ad7112f"
BAL_V2 = "0x2170c741c41531aec20e7c107c24eecfdd15e69c9bb0a8dd37b1840b9e0b207b"
BAL_V3 = "0x0874b2d545cb271cdbda4e093020c452328b24af12382ed62c4d00f5c26709db"
TESSERA = "0x97ba0cd8ff13f074b3b1aeace7fa3bf7fe54bdf2d728b6a097e901073b2bad6a"
ELFOMO = "0xbe65a3f1f381da16732df786f571604a72b7c122cff3ae2b355566ddf01e2528"
MAVERICK_V2 = "0x103ed084e94a44c8f5f6ba8e3011507c41063177e29949083c439777d8d63f60"
FLUID = "0xdc004dbca4ef9c966218431ee5d9133d337ad018dd5b5c5493722803f75c64f7"
CURVE_I128 = "0x8b3e96f2b889fa771c53c981b40daf005f63f637f1869f707052d15a3dd97140"
CURVE_U256 = "0xb2e76ae99761dc136e598d4a629bb347eccb9532a5f8bbd72e18467c3c34cc98"
# Metric v1's current pools (factory 0x2a53833c...): Swap(address,address,uint256,uint256,uint256).
METRIC = "0x9734819749a91fc3be03ea83205f924ee08479bd3f0da48efc91d94d050cac1e"
HANJI_ORDER = "0x1047930a29721c0069210227eeb58936b0a4667fe473b91765b069d71b1e3d7d"
TRANSFER = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"

POOL_MANAGER = "0x498581ff718922c3f8e6a244956af099b2652b2b"
BAL_V2_VAULT = "0xba12222222228d8ba445958a75a0704d566bf2c8"
BAL_V3_VAULT = "0xba1333333333a1ba1108e8412f11850a5c319ba9"
TESSERA_SWAP = "0x55555522005bcae1c2424d474bfd5ed477749e3e"
ELFOMO_SWAP = "0xf0f0f0f0fb0d738452efd03a28e8be14c76d5f73"

# v2/v3-style pools are attributed by their factory().
VENUE_OF_FACTORY = {
    "0x33128a8fc17869897dce68ed026d694621f6fdfd": "uniswap_v3",
    "0x0bfbcf9fa4f9c56b0f40a671ad40e0805a091865": "pancakeswap_v3",
    "0x5e7bb104d84c7cb9b682aac2f3d509f5f406809a": "slipstream_5e7b",
    "0xf8f2eb4940cfe7d13603dddd87f123820fc061ef": "slipstream_f8f2",
    "0xade65c38cd4849adba595a4323a8c7ddfe89716a": "cl_ade6",
    "0x8909dc15e40173ff4699343b6eb8132c65e18ec6": "uniswap_v2",
    "0x420dd381b31aef6683db6b902084cb0ffece40da": "aerodrome_v2",
    # QuickSwap's Base pools: v4 is Algebra Integral, v2 a Uniswap v2 fork.
    "0xc5396866754799b9720125b104ae01d935ab9c7b": "quickswap",
    "0xec6540261aaae13f236a032d454dc9287e52e56a": "quickswap_v2",
}
# Venues whose tokens sit at the address that emits the trade event, so a
# trade's tokens move to and from that address by Transfer.
HOLDS_AT_EMITTER = frozenset({"uniswap_v3", "pancakeswap_v3", "slipstream_5e7b", "slipstream_f8f2", "cl_ade6",
                              "uniswap_v2", "aerodrome_v2", "maverick_v2", "curve", "metric", "quickswap",
                              "quickswap_v2"})


def holds_at_emitter(venue):
    return venue in HOLDS_AT_EMITTER or venue.startswith("factory:")


# The venues the shadow prices (R22).
OURS = frozenset({"uniswap_v3", "pancakeswap_v3", "slipstream_5e7b", "slipstream_f8f2"})
MARKET_MAKERS = frozenset({"tessera", "elfomofi", "metric", "hanji"})

# Hanji is decoded but not collected: its fills' amounts matched transfers in
# 11 of 12 receipts, but their direction cannot be verified from transfers (a
# book forwards funds through its market maker, and the taker is usually a
# router), so it is reported as not covered rather than counted (R23).
POOL_TOPICS = (UNI_V3, PANCAKE_V3, ALGEBRA_INTEGRAL, UNI_V2, AERO_V2, MAVERICK_V2, FLUID,
               CURVE_I128, CURVE_U256, METRIC)
ALL_TOPICS = POOL_TOPICS + (UNI_V4, BAL_V2, BAL_V3, TESSERA, ELFOMO)


@dataclass(frozen=True)
class Leg:
    tx: str
    block: int
    venue: str
    pool: str
    deltas: dict = field(hash=False, compare=True)
    # The venue's side of any native-ETH flow, in wei: positive when it took ETH.
    native: int = field(default=0, compare=False)


def _s256(h):
    x = int(h, 16)
    return x - (1 << 256) if x >= 1 << 255 else x


def _i128(x):
    x &= (1 << 128) - 1
    return x - (1 << 128) if x >= 1 << 127 else x


def _words(data):
    d = data[2:]
    return [d[i:i + 64] for i in range(0, len(d), 64)]


def _addr(word):
    return "0x" + word[-40:].lower()


def _add(deltas, token, x, nat):
    if token in NATIVE:
        nat[0] += x
        token = WETH
    deltas[token] = deltas.get(token, 0) + x


def pool_key(log):
    """Where a log's pool metadata is stored; None for a fixed-contract venue."""
    t0, a = log["topics"][0], log["address"].lower()
    if t0 == UNI_V4:
        return log["topics"][1] if a == POOL_MANAGER else None
    if t0 == BAL_V2:
        return "bal2:" + log["topics"][1] if a == BAL_V2_VAULT else None
    if t0 == BAL_V3:
        return "bal3:" + _addr(log["topics"][1]) if a == BAL_V3_VAULT else None
    if t0 in (TESSERA, ELFOMO):
        return None
    return a


def decode(log, meta):
    """One log as a Leg, or None."""
    t0, a = log["topics"][0], log["address"].lower()
    tx, block = log["transactionHash"], int(log["blockNumber"], 16)
    try:
        w = _words(log["data"])
        d, nat = {}, [0]

        def add(token, x):
            _add(d, token, x, nat)

        if t0 == TESSERA:
            if a != TESSERA_SWAP:
                return None
            add(_addr(w[0]), int(w[2], 16))
            add(_addr(w[1]), -int(w[3], 16))
            return Leg(tx, block, "tessera", a, d, nat[0])
        if t0 == ELFOMO:
            if a != ELFOMO_SWAP:
                return None
            add(_addr(w[2]), int(w[4], 16))
            add(_addr(w[3]), -int(w[5], 16))
            return Leg(tx, block, "elfomofi", a, d, nat[0])
        if t0 in (BAL_V2, BAL_V3):
            key = pool_key(log)
            if key is None:
                return None
            add(_addr(log["topics"][2]), int(w[0], 16))
            add(_addr(log["topics"][3]), -int(w[1], 16))
            return Leg(tx, block, "balancer_v2" if t0 == BAL_V2 else "balancer_v3", key, d, nat[0])
        key = pool_key(log)
        m = meta.get(key) if key else None
        if not m:
            return None
        if t0 == CURVE_I128 or t0 == CURVE_U256:
            coins = m["coins"]
            sold, bought = int(w[0], 16), int(w[2], 16)
            if sold >= len(coins) or bought >= len(coins):
                return None
            add(coins[sold], int(w[1], 16))
            add(coins[bought], -int(w[3], 16))
            return Leg(tx, block, m["venue"], key, d, nat[0])
        if t0 == HANJI_ORDER:
            # topics: owner, initiator, isAsk; data: order_id, quantity, price,
            # passive_shares, passive_fee, aggressive_shares, aggressive_value,
            # aggressive_fee, market_only, post_only. The taker's fill is the
            # aggressive part; verification decides whether this reading counts.
            shares, value, fee = int(w[5], 16), int(w[6], 16), int(w[7], 16)
            if shares == 0:
                return None
            is_ask = int(log["topics"][3], 16) == 1
            x, y = shares * m["sx"], value * m["sy"]
            if is_ask:
                add(m["x"], x)
                add(m["y"], -(y - fee * m["sy"]))
            else:
                add(m["y"], y + fee * m["sy"])
                add(m["x"], -x)
            return Leg(tx, block, "hanji", key, d, nat[0])
        tok0, tok1, venue = m[0], m[1], m[2]
        if t0 in (UNI_V3, PANCAKE_V3, ALGEBRA_INTEGRAL):
            add(tok0, _s256(w[0]))
            add(tok1, _s256(w[1]))
        elif t0 == UNI_V4:
            add(tok0, -_s256(w[0]))
            add(tok1, -_s256(w[1]))
        elif t0 in (UNI_V2, AERO_V2):
            add(tok0, int(w[0], 16) - int(w[2], 16))
            add(tok1, int(w[1], 16) - int(w[3], 16))
        elif t0 == MAVERICK_V2:
            # sender, recipient, (amount, tokenAIn, exactOutput, tickLimit), amountIn, amountOut
            a_in = int(w[3], 16) == 1
            ain, aout = int(w[6], 16), int(w[7], 16)
            add(tok0 if a_in else tok1, ain)
            add(tok1 if a_in else tok0, -aout)
        elif t0 == FLUID:
            zero_for_one = int(w[0], 16) == 1
            add(tok0 if zero_for_one else tok1, int(w[1], 16))
            add(tok1 if zero_for_one else tok0, -int(w[2], 16))
        elif t0 == METRIC:
            # details, amountDeltas, platformFees: amountDeltas packs token0's
            # change in its high 128 bits and token1's in its low 128, each
            # two's-complement, positive when the pool gained (metric-core
            # Packed2Int128; verification checks it).
            packed = int(w[1], 16)
            add(tok0, _i128(packed >> 128))
            add(tok1, _i128(packed & ((1 << 128) - 1)))
        else:
            return None
        return Leg(tx, block, venue, key, d, nat[0])
    except (IndexError, ValueError, KeyError, TypeError):
        return None
