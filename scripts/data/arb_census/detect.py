"""Is a transaction an arbitrage, and what did it bank? (R23)

Closure is judged on decoded legs; value on the receipt's transfers, which
also catch legs on venues no decoder reads.
"""
from arb_census.venues import (BAL_V2_VAULT, BAL_V3_VAULT, ELFOMO_SWAP, POOL_MANAGER, TESSERA_SWAP, TRANSFER, WETH,
                               WETH_DEPOSIT, WETH_WITHDRAWAL, decode, holds_at_emitter)

# Spending under this share of a token's flow is rounding, not spending.
DUST = 1e-4


def trader_change(legs):
    """What the trader gained (+) or spent (-) of each token: the opposite
    of the venues' side, summed."""
    out = {}
    for leg in legs:
        for t, x in leg.deltas.items():
            out[t] = out.get(t, 0) - x
    return out


def is_arbitrage(legs):
    # One pool has no second price to trade against: what it gives up is its
    # own mispricing (a drain), not arbitrage.
    if len({leg.pool for leg in legs}) < 2:
        return False
    change = trader_change(legs)
    flow = {}
    for leg in legs:
        for t, x in leg.deltas.items():
            flow[t] = flow.get(t, 0) + abs(x)
    spent = any(x < 0 and -x > flow[t] * DUST for t, x in change.items())
    return not spent and any(x > 0 for x in change.values())


def legs_of(receipt, meta):
    out = []
    for log in receipt["logs"]:
        if not log["topics"]:
            continue
        log = dict(log, transactionHash=receipt["transactionHash"], blockNumber=receipt["blockNumber"])
        leg = decode(log, meta)
        if leg:
            out.append(leg)
    return out


def banked(receipt, legs=()):
    """Net ERC-20 transfers into the bot's addresses (`bot_addresses`): what
    the bot kept.

    Native ETH paid to or taken from a venue shows in no Transfer log, so the
    decoded legs' native flows are counted against WETH: without them, a bot
    that paid ETH into a v4 pool and took WETH back looks to have kept it all.
    ETH that an address outside the bot wrapped after a venue paid it out, or
    unwrapped before a venue took it, was not the bot's.
    """
    who = bot_addresses(receipt, legs)
    net = {}
    for log in receipt["logs"]:
        t = log["topics"]
        if len(t) != 3 or t[0] != TRANSFER or log["data"] in ("0x", ""):
            continue
        src, dst = "0x" + t[1][-40:], "0x" + t[2][-40:]
        token, x = log["address"].lower(), int(log["data"][:66], 16)
        if dst in who and src not in who:
            net[token] = net.get(token, 0) + x
        elif src in who and dst not in who:
            net[token] = net.get(token, 0) - x
    took, paid = native_flow(receipt, legs)
    unwrapped = sum(u for a, (u, _) in _wraps(receipt).items() if a not in who)
    wrapped = sum(w for a, (_, w) in _wraps(receipt).items() if a not in who)
    mine = max(0, paid - wrapped) - max(0, took - unwrapped)
    if mine:
        net[WETH] = net.get(WETH, 0) + mine
    return {k: v for k, v in net.items() if v}


# Contracts that hold venues' tokens without being a pool address.
VENUE_CONTRACTS = frozenset({POOL_MANAGER, BAL_V2_VAULT, BAL_V3_VAULT, TESSERA_SWAP, ELFOMO_SWAP})


def bot_addresses(receipt, legs=()):
    """The sender, its target, and the bot's executors: any other address
    that wraps or unwraps WETH in this receipt and trades directly with a
    decoded venue. An executor can hold the profit or pay the ETH without the
    sender or target touching either. A venue that wraps (the v4 PoolManager
    does) is not the bot, nor is an address that only unwraps what it was
    paid (a payee)."""
    who = {receipt["from"].lower(), (receipt.get("to") or "").lower()}
    venues = VENUE_CONTRACTS | {leg.pool.lower() for leg in legs}
    traded = set()
    for log in receipt["logs"]:
        t = log["topics"]
        if len(t) == 3 and t[0] == TRANSFER:
            src, dst = "0x" + t[1][-40:], "0x" + t[2][-40:]
            if src in venues or dst in venues:
                traded.update((src, dst))
    return who | {a for a in _wraps(receipt) if a in traded and a not in venues}


def _wraps(receipt):
    """WETH each address unwrapped and wrapped in this receipt: {address: (unwrapped, wrapped)}."""
    out = {}
    for log in receipt["logs"]:
        t = log["topics"]
        if len(t) == 2 and log["address"].lower() == WETH and t[0] in (WETH_DEPOSIT, WETH_WITHDRAWAL):
            a, x = "0x" + t[1][-40:], int(log["data"][:66], 16)
            u, w = out.get(a, (0, 0))
            out[a] = (u + x, w) if t[0] == WETH_WITHDRAWAL else (u, w + x)
    return out


def native_flow(receipt, legs):
    """The native ETH the venues in these legs took and paid, in wei, as
    (took, paid): what decoding marked native, plus the WETH side of any pool
    that holds its own tokens and moved no WETH by Transfer in this receipt (a
    Curve pool listing WETH among its coins can pay native ETH)."""
    touched = set()
    for log in receipt["logs"]:
        t = log["topics"]
        if len(t) == 3 and t[0] == TRANSFER and log["address"].lower() == WETH:
            touched.update(("0x" + t[1][-40:], "0x" + t[2][-40:]))
    took = paid = 0
    for leg in legs:
        x = 0
        if leg.native:
            x = leg.native
        elif holds_at_emitter(leg.venue) and leg.deltas.get(WETH) and leg.pool.lower() not in touched:
            x = leg.deltas[WETH]
        took, paid = took + max(0, x), paid + max(0, -x)
    return took, paid


def gas_wei(receipt):
    return int(receipt["gasUsed"], 16) * int(receipt.get("effectiveGasPrice", "0x0"), 16) + int(
        receipt.get("l1Fee") or "0x0", 16
    )
