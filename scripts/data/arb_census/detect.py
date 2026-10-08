"""Is a transaction an arbitrage, and what did it bank? (R23)

Closure is judged on decoded legs; value on the receipt's transfers, which
also catch legs on venues no decoder reads.
"""
from arb_census.venues import TRANSFER, WETH, WETH_DEPOSIT, WETH_WITHDRAWAL, decode, holds_at_emitter

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
    if len(legs) < 2:
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
    """
    who = bot_addresses(receipt)
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
    paid_in = native_flow(receipt, legs)
    if paid_in:
        net[WETH] = net.get(WETH, 0) - paid_in
    return {k: v for k, v in net.items() if v}


def bot_addresses(receipt):
    """The sender, its target, and any other address that wraps or unwraps
    WETH in this receipt: a bot's executor, which can hold its profit or pay
    its ETH without the sender or target touching either. (No venue wrapped
    or unwrapped in 500 sampled arbitrages on Curve, Fluid, Balancer, Uniswap
    v4 and the market-maker venues, R23.)"""
    who = {receipt["from"].lower(), (receipt.get("to") or "").lower()}
    for log in receipt["logs"]:
        t = log["topics"]
        if len(t) == 2 and log["address"].lower() == WETH and t[0] in (WETH_DEPOSIT, WETH_WITHDRAWAL):
            who.add("0x" + t[1][-40:])
    return who


def native_flow(receipt, legs):
    """The venues' side of every native-ETH flow in these legs, in wei: what
    decoding marked native, plus the WETH side of any pool that holds its own
    tokens and moved no WETH by Transfer in this receipt (a Curve pool listing
    WETH among its coins can pay native ETH)."""
    touched = set()
    for log in receipt["logs"]:
        t = log["topics"]
        if len(t) == 3 and t[0] == TRANSFER and log["address"].lower() == WETH:
            touched.update(("0x" + t[1][-40:], "0x" + t[2][-40:]))
    total = 0
    for leg in legs:
        if leg.native:
            total += leg.native
        elif holds_at_emitter(leg.venue) and leg.deltas.get(WETH) and leg.pool.lower() not in touched:
            total += leg.deltas[WETH]
    return total


def gas_wei(receipt):
    return int(receipt["gasUsed"], 16) * int(receipt.get("effectiveGasPrice", "0x0"), 16) + int(
        receipt.get("l1Fee") or "0x0", 16
    )
