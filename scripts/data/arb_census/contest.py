"""R38: the armed bid's contest sample, measured at our own judged fires.

Each fire armed-eval judged becomes at most one row: the share of the arm's
surplus a bid had to exceed to win it, in parts per ``SCALE``. The fit is
three complete UTC days of verdicts, and a fourth, untouched day is held out.
On it both rules are scored, on the same fires:

- ``best_bid``: the bid the arm carried, from R36's measured winners;
- ``best_share``: the bid ``best_share`` chooses from the fit.

The result is the ``contest`` section of ``targets.json``. The runtime's
``best_share`` (``crates/apex-runtime/src/armed/bid.rs``) bids against its
rows, and sends on them only when ``fit_ok``.

The design is ``docs/superpowers/specs/2026-10-29-armed-bid-from-own-fires-design.md``.
Every class and gate below is from it.
"""
from __future__ import annotations

import hashlib
import json
import random
from collections import Counter, defaultdict
from fractions import Fraction
from pathlib import Path
from typing import Any, Iterable, Iterator, Mapping, Sequence

SCALE = 10**9
MIN_EFFECTIVE = 300
MAX_ROWS = 1_000
BUCKET_MIN = 30
CALIBRATION_RATIO = 2
BOOTSTRAP = 2_000
SEED = 38
# The share a fit is checked at: the canary's armed.max_priority_wei_per_gas.
CAP_WEI_PER_GAS = 20_000_000_000
U256 = 1 << 256
USDC = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"
BUCKETS = (0.0, 0.05, 0.15, 0.30, 0.60, 1.0000001)
SEGMENTS = ((0, 8), (8, 16), (16, 24))
# More than this share of fires without their arm record fails integrity.
MAX_UNMATCHED = 0.01


class ShareError(ValueError):
    """``best_share`` failing closed, with the runtime's reason."""


def _mul(a: int, b: int) -> int:
    v = a * b
    if v >= U256:
        raise ShareError("overflow")
    return v


def best_share(rows: Sequence[Sequence[int]], surplus: int, gas: int, cap: int) -> tuple[int, int, int]:
    """The runtime's ``best_share``, to the wei: ``(bid, weight below, total)``.

    ``rows`` ascend by share. ``F(b)`` is the weight of rows with
    ``x * surplus < b * gas * SCALE``. Each row's candidate is the smallest
    integer bid past it. A bid over ``cap``, or whose gas leaves no surplus,
    is no candidate. A tie keeps the lower bid.
    """
    if gas <= 0:
        raise ShareError("zero gas")
    if surplus <= 0:
        raise ShareError("zero surplus")
    if not rows:
        raise ShareError("empty sample")
    total = sum(w for _, w in rows)
    per = _mul(gas, SCALE)
    best_b, best_below, best = 0, 0, 0
    below = i = 0
    for x, _ in rows:
        b = _mul(x, surplus) // per + 1
        if b > cap:
            break
        tip = _mul(b, gas)
        if tip >= surplus:
            break
        bar = _mul(b, per)
        while i < len(rows) and _mul(rows[i][0], surplus) < bar:
            below += rows[i][1]
            i += 1
        value = _mul(below, surplus - tip)
        if value > best:
            best_b, best_below, best = b, below, value
    return best_b, best_below, total


def share_ppb(bid: int, gas: int, surplus_wei: int) -> int:
    """The share of ``surplus_wei`` a bid per gas takes, in parts per
    ``SCALE``, rounded up: a rival rounded the stronger way."""
    return -(-bid * gas * SCALE // surplus_wei)


def surplus_wei(arm: Mapping[str, Any], weth_usd: float | None) -> tuple[int | None, bool]:
    """The arm's threshold surplus in wei, rounded down, and whether its
    rate came from the verdict's WETH price.

    - A USDC arm uses its recorded ``start_per_eth``, USDC units a wei as
      ``[num, den]``.
    - Arms signed before decision 4 (before 2026-10-29 15:36 UTC) carry no
      rate, so they use the verdict's WETH price instead.
    - A WETH arm's surplus is wei already.
    """
    units = int(arm["surplus_wei"])
    if str(arm.get("start") or "").lower() != USDC:
        return units, False
    rate = arm.get("start_per_eth")
    if rate:
        num, den = int(rate[0]), int(rate[1])
        return (units * den // num if num > 0 else None), False
    if not weth_usd or weth_usd <= 0:
        return None, True
    return int(Fraction(units * 10**12) / Fraction(weth_usd)), True


def _lines(path: Path) -> Iterator[dict[str, Any]]:
    if not path.exists():
        return
    with path.open("r", encoding="utf-8") as stream:
        for line in stream:
            if not line.strip():
                continue
            try:
                value = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(value, dict):
                yield value


ARM_FIELDS = ("at_ns", "route_hash", "gas_expected", "surplus_wei", "start", "start_per_eth", "bid_wei_per_gas",
              "alt_bid_wei_per_gas", "bid_model", "other_wei", "base_fee_wei")


def load_arms(var: Path, days: Iterable[str]) -> dict[tuple[int, str], list[dict[str, Any]]]:
    """Arm records by ``(arm_id, route_hash)``, each list by time: IDs
    restart at every boot."""
    arms: dict[tuple[int, str], list[dict[str, Any]]] = defaultdict(list)
    for day in days:
        for a in _lines(var / f"armed-{day}.jsonl"):
            if a.get("kind") == "arm" and "arm_id" in a:
                arms[(a["arm_id"], a.get("route_hash", ""))].append({k: a.get(k) for k in ARM_FIELDS})
    for v in arms.values():
        v.sort(key=lambda a: a["at_ns"])
    return arms


def arm_of(arms: Mapping[tuple[int, str], list[dict[str, Any]]], verdict: Mapping[str, Any]) -> dict[str, Any] | None:
    """The arm a verdict judged: the latest record of its ID and route
    signed before the verdict was written."""
    best = None
    for a in arms.get((verdict.get("arm_id"), verdict.get("route_hash", "")), []):
        if a["at_ns"] <= verdict.get("at_ns", 0):
            best = a
    return best


def load_layouts(var: Path, days: Iterable[str]) -> dict[tuple[int, int], tuple[int, int]]:
    """``(block, flashblock) -> (first tx, tx count)`` from the flashblock
    ledgers, where each was seen whole."""
    out: dict[tuple[int, int], tuple[int, int]] = {}
    for day in days:
        for f in _lines(var / f"flashblocks-{day}.jsonl"):
            if f.get("first_tx") is not None and "block" in f and "index" in f:
                out[(f["block"], f["index"])] = (f["first_tx"], f.get("tx_count", 0))
    return out


def load_own_sends(live_ledger: Path) -> dict[str, int | None]:
    """Our armed-live transactions, by hash, with the arm that sent each."""
    sends: dict[str, int | None] = {}
    for r in _lines(live_ledger):
        if r.get("kind") in ("submitting", "landed") and r.get("tx"):
            sends[str(r["tx"]).lower()] = r.get("arm_id")
    return sends


def _next(position: tuple[int, int]) -> tuple[int, int]:
    block, index = position
    return (block + 1, 1) if index >= 10 else (block, index + 1)


def classify(verdict: Mapping[str, Any], layouts: Mapping[tuple[int, int], tuple[int, int]],
             own_sends: Mapping[str, int | None]) -> tuple[str, int | None]:
    """A fired verdict's contest, under the corrected rule (R37's evaluator
    fix): ``exact`` with the rival's bid per gas, ``uncontested`` (0),
    ``censored`` with the bid our own landing paid, or ``excluded:<reason>``.

    Our earliest flashblock is the trigger's own, or the next one when the
    trigger closed it.
    """
    if verdict.get("shadowed"):
        return "excluded:shadowed", None
    if verdict.get("quote_error") is not None or not verdict.get("gross_wei"):
        return "excluded:not priced", None
    fire = verdict.get("fire") or {}
    if fire.get("flashblock") is None:
        return "excluded:fire not placed", None
    layout = layouts.get((fire["block"], fire["flashblock"]))
    if layout is None:
        return "excluded:fire not placed", None
    first, count = layout
    here = (fire["block"], fire["flashblock"])
    earliest = _next(here) if fire["tx_index"] == first + count - 1 else here
    rival = verdict.get("rival")
    if rival is None:
        return "uncontested", 0
    priority = verdict.get("rival_priority_wei")
    tx = str(rival.get("tx", "")).lower()
    if verdict.get("ours") or (tx in own_sends and own_sends[tx] == verdict.get("arm_id")):
        return ("censored", int(priority)) if priority is not None else ("excluded:own send unpriced", None)
    if rival.get("flashblock") is None:
        if rival["block"] > earliest[0]:
            return "uncontested", 0
        return "excluded:rival not placed", None
    at = (rival["block"], rival["flashblock"])
    if at > earliest:
        return "uncontested", 0
    if at < earliest:
        return "excluded:rival before our flashblock", None
    if priority is None:
        return "excluded:rival unreadable", None
    return "exact", int(priority)


def fires(var: Path, days: Iterable[str], arms: Mapping[tuple[int, str], list[dict[str, Any]]],
          layouts: Mapping[tuple[int, int], tuple[int, int]], own_sends: Mapping[str, int | None]
          ) -> tuple[list[dict[str, Any]], Counter]:
    """Every fired verdict of ``days`` as a scored fire, and the count of
    each class and exclusion."""
    out: list[dict[str, Any]] = []
    counts: Counter = Counter()
    for day in days:
        for v in _lines(var / f"armed-verdicts-{day}.jsonl"):
            if not v.get("fired"):
                continue
            counts["fired"] += 1
            cls, value = classify(v, layouts, own_sends)
            arm = arm_of(arms, v)
            if cls.startswith("excluded"):
                counts[cls] += 1
                continue
            if arm is None:
                counts["excluded:no arm record"] += 1
                continue
            surplus, from_verdict = surplus_wei(arm, v.get("weth_usd"))
            gas = int(arm.get("gas_expected") or 0)
            if not surplus or surplus <= 0 or gas <= 0:
                counts["excluded:no surplus or gas"] += 1
                continue
            counts[cls] += 1
            counts["rate_from_verdict"] += int(from_verdict)
            fire = v["fire"]
            out.append({
                "day": day,
                "cls": cls,
                "value": value,
                "x": share_ppb(value, gas, surplus),
                "cluster": (fire["block"], str(fire.get("tx", "")).lower()),
                "route": arm.get("route_hash", ""),
                "hour": (int(arm["at_ns"]) // 1_000_000_000 % 86_400) // 3_600,
                "surplus": surplus,
                "gas": gas,
                "gross": int(v["gross_wei"]),
                "base_fee": int(v.get("base_fee_wei") or arm.get("base_fee_wei") or 0),
                "other": int(arm.get("other_wei") or 0),
                "best_bid": _best_bid_of(arm),
            })
    return out, counts


def _best_bid_of(arm: Mapping[str, Any]) -> int | None:
    """The ``best_bid`` rule's bid the arm carried: the one it sent, or the
    alternative when it sent ``best_share``."""
    if arm.get("bid_model") == "best_share":
        alt = arm.get("alt_bid_wei_per_gas")
        return int(alt) if alt is not None else None
    sent = arm.get("bid_wei_per_gas")
    return int(sent) if sent not in (None, "") else None


def fit_rows(sample: Sequence[Mapping[str, Any]], bound: str = "lower", max_rows: int = MAX_ROWS) -> tuple[list[list[int]], int]:
    """The weighted rows the runtime bids against, and the effective count.

    - One market move, one weight: a cluster's ``k`` rows weigh ``10**6 // k``
      each.
    - A censored row sits at its bound under ``lower``, the pessimistic case
      the arms bid on, and at 0 under ``upper``.
    - Past ``max_rows`` distinct shares, neighbours merge into buckets of
      about equal weight, each at its largest share: rounded the stronger way.
    """
    sizes = Counter(f["cluster"] for f in sample)
    by_x: dict[int, int] = defaultdict(int)
    for f in sample:
        x = 0 if (f["cls"] == "censored" and bound == "upper") else f["x"]
        by_x[x] += 10**6 // sizes[f["cluster"]]
    rows = sorted([x, w] for x, w in by_x.items() if w > 0)
    if len(rows) > max_rows:
        # Each row joins the weight quantile its start falls in; a bucket
        # takes its largest share and the sum of its weights.
        total = sum(w for _, w in rows)
        merged: list[list[int]] = []
        acc = 0
        for x, w in rows:
            bucket = min(max_rows - 1, acc * max_rows // total)
            acc += w
            if merged and merged[-1][2] == bucket:
                merged[-1][0] = x
                merged[-1][1] += w
            else:
                merged.append([x, w, bucket])
        rows = [[x, w] for x, w, _ in merged]
    return rows, len(sizes)


def outcome(f: Mapping[str, Any], bid: int) -> tuple[bool, int]:
    """Whether ``bid`` wins the fire, and its net in wei when it does.

    - Uncontested: any positive bid wins.
    - Exact: a bid above the rival's wins.
    - Censored: our landing at the bid it paid, so a bid at least as high
      wins, and a lower one counts as lost.
    """
    if f["cls"] == "uncontested":
        won = bid > 0
    elif f["cls"] == "censored":
        won = bid >= f["value"]
    else:
        won = bid > f["value"]
    if not won:
        return False, 0
    return True, f["gross"] - f["gas"] * (f["base_fee"] + bid) - f["other"]


def _bucket(p: float) -> int:
    for i in range(len(BUCKETS) - 1):
        if BUCKETS[i] <= p < BUCKETS[i + 1]:
            return i
    return len(BUCKETS) - 2


def _within(predicted: float, realised: float) -> bool:
    if predicted == 0 or realised == 0:
        return predicted == realised
    return max(predicted, realised) / min(predicted, realised) <= CALIBRATION_RATIO


def calibration(scored: Sequence[tuple[float, bool]]) -> dict[str, Any]:
    """Predicted against realised ``p_win`` per bucket; a bucket under
    ``BUCKET_MIN`` fires is shown, not judged."""
    groups: dict[int, list[tuple[float, bool]]] = defaultdict(list)
    for p, won in scored:
        groups[_bucket(p)].append((p, won))
    rows, ok = [], True
    for i in sorted(groups):
        g = groups[i]
        predicted = sum(p for p, _ in g) / len(g)
        realised = sum(w for _, w in g) / len(g)
        judged = len(g) >= BUCKET_MIN
        within = _within(predicted, realised)
        ok &= within or not judged
        rows.append({"bucket": [BUCKETS[i], min(BUCKETS[i + 1], 1.0)], "n": len(g), "predicted": predicted,
                     "realised": realised, "judged": judged, "within_2x": within})
    return {"ok": ok, "buckets": rows}


def bootstrap(deltas: Sequence[tuple[str, tuple[int, str], int]], reps: int = BOOTSTRAP, seed: int = SEED) -> tuple[int, int]:
    """A 95% interval for the summed paired delta, resampling whole clusters
    within each route."""
    by_route: dict[str, dict[tuple[int, str], int]] = defaultdict(lambda: defaultdict(int))
    for route, cluster, d in deltas:
        by_route[route][cluster] += d
    strata = [list(c.values()) for _, c in sorted(by_route.items())]
    rng = random.Random(seed)
    sums = []
    for _ in range(reps):
        total = 0
        for clusters in strata:
            total += sum(rng.choice(clusters) for _ in clusters)
        sums.append(total)
    sums.sort()
    if not sums:
        return 0, 0
    return sums[int(0.025 * reps)], sums[min(reps - 1, int(0.975 * reps))]


def evaluate(fit: Sequence[list[int]], held: Sequence[Mapping[str, Any]], cap: int = CAP_WEI_PER_GAS) -> dict[str, Any]:
    """Both rules on the same held-out fires: wins, net after costs, losing
    wins, calibration of ``best_share``'s ``p_win``, and the paired delta
    with its cluster-bootstrap interval."""
    tally = {rule: Counter() for rule in ("best_bid", "best_share", "base")}
    scored, by_segment, by_route = [], defaultdict(list), defaultdict(list)
    deltas = []
    for f in held:
        try:
            b_share, below, total = best_share(fit, f["surplus"], f["gas"], cap)
        except ShareError as exc:
            tally["best_share"][f"fallback:{exc}"] += 1
            continue
        p = below / total if total else 0.0
        won_share, net_share = outcome(f, b_share)
        scored.append((p, won_share))
        by_segment[f["hour"] // 8].append((p, won_share))
        by_route[f["route"]].append((p, won_share))
        nets = {"best_share": (won_share, net_share)}
        if f["best_bid"] is not None:
            nets["best_bid"] = outcome(f, f["best_bid"])
        nets["base"] = outcome(f, 1)
        for rule, (won, net) in nets.items():
            t = tally[rule]
            t["fires"] += 1
            t["wins"] += int(won)
            t["net_wei"] += net
            t["losing_wins"] += int(won and net < 0)
        if "best_bid" in nets:
            deltas.append((f["route"], f["cluster"], net_share - nets["best_bid"][1]))
    low, high = bootstrap(deltas)
    return {
        "rules": {rule: dict(t) for rule, t in tally.items()},
        "paired": {"fires": len(deltas), "delta_wei": sum(d for _, _, d in deltas), "ci95_wei": [low, high],
                   "clusters": len({c for _, c, _ in deltas})},
        "calibration": calibration(scored),
        "by_segment": {f"{SEGMENTS[s][0]:02d}-{SEGMENTS[s][1]:02d}": calibration(v) for s, v in sorted(by_segment.items())},
        "by_route": {r: calibration(v) for r, v in sorted(by_route.items()) if len(v) >= BUCKET_MIN},
    }


def contest_section(var: Path, live_ledger: Path, fit_days: Sequence[str], held_out_day: str,
                    arm_days: Sequence[str] | None = None) -> dict[str, Any]:
    """The ``contest`` section: the fit's rows, and whether they passed.

    The runtime bids ``best_share`` on ``rows`` only when ``fit_ok``. Every
    reason it is not fit is listed.
    """
    days = list(fit_days) + [held_out_day]
    arm_days = list(arm_days) if arm_days is not None else days
    arms = load_arms(var, arm_days)
    layouts = load_layouts(var, days)
    own_sends = load_own_sends(live_ledger)
    fit_fires, fit_counts = fires(var, fit_days, arms, layouts, own_sends)
    held_fires, held_counts = fires(var, [held_out_day], arms, layouts, own_sends)
    rows, effective = fit_rows(fit_fires)
    upper, _ = fit_rows(fit_fires, bound="upper")
    lower_eval = evaluate(rows, held_fires)
    upper_eval = evaluate(upper, held_fires)

    reasons = []
    if len(fit_days) < 3:
        reasons.append(f"{len(fit_days)} fit days, 3 needed")
    if effective < MIN_EFFECTIVE:
        reasons.append(f"{effective} effective rows, {MIN_EFFECTIVE} needed")
    for name, counts in (("fit", fit_counts), ("held-out", held_counts)):
        unmatched = counts["excluded:no arm record"]
        if counts["fired"] == 0:
            reasons.append(f"no {name} fires")
        elif unmatched > MAX_UNMATCHED * counts["fired"]:
            reasons.append(f"{unmatched} of {counts['fired']} {name} fires have no arm record")
    if not lower_eval["calibration"]["ok"]:
        reasons.append("held-out calibration outside 2x (censored rows at their bound)")
    if not upper_eval["calibration"]["ok"]:
        reasons.append("held-out calibration outside 2x (censored rows at 0)")
    low = lower_eval["paired"]["ci95_wei"][0]
    if lower_eval["paired"]["fires"] == 0 or low <= 0:
        reasons.append(f"paired net against best_bid not above zero at 95% (low {low} wei)")

    digest = hashlib.sha256(json.dumps(rows).encode()).hexdigest()[:8]
    return {
        "model_version": f"r38-{held_out_day}-{digest}",
        "scale": SCALE,
        "rows": rows,
        "effective": effective,
        "fit_ok": not reasons,
        "reasons": reasons,
        "fit_days": list(fit_days),
        "held_out_day": held_out_day,
        "report": {
            "fit_counts": dict(fit_counts),
            "held_out_counts": dict(held_counts),
            "held_out": lower_eval,
            "held_out_censored_at_zero": upper_eval,
        },
    }
