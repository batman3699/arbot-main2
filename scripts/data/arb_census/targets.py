"""Build the Armed Lane v2 daily route-target file (Task 8.5 R36).

Run from ``scripts/data`` so the existing ``arb_census`` package and
``race_analysis`` module are importable::

    python3 -m arb_census.targets \
      --race /opt/apex/var/census --days 7 --top 50 \
      --out /opt/apex/var/apex/targets.json \
      --share-out /opt/apex/var/apex/armed-share.json \
      --var /opt/apex/var/apex \
      --meta /opt/apex/var/census/meta.json \
      --gaps /opt/apex/var/census/r33-gaps.json \
      --live-ledger /opt/apex/var/apex/armed-live.jsonl

The target universe is deliberately strict: two-hop routes over
``race.OURS`` and Uniswap v4 that start in, and gain only, WETH or USDC
(R37), whose arbitrage was taken in its trigger block.
The script never guesses missing route fields. If Task 1's route-aware census
schema is unavailable, it fails rather than silently writing an empty target
set.
"""
from __future__ import annotations

import argparse
import gzip
import json
import math
import os
import sys
from collections import defaultdict
from datetime import date, datetime, time, timedelta, timezone
from pathlib import Path
from typing import Any, Iterable, Mapping, Sequence

from arb_census import race
from arb_census.venues import USDC, WETH

UTC = timezone.utc
CACHE_SCHEMA_VERSION = 1
MIN_ROUTE_BIDS = 20
MAX_U128 = (1 << 128) - 1
# R37: the tokens a route may start in, and gain.
STARTS = frozenset({WETH, USDC})
VENUES = race.OURS | {race.V4}


def _normalise(value: Any) -> str | None:
    """Return a normalised string identifier, or None for invalid input."""
    if not isinstance(value, str) or not value.strip():
        return None
    return value.strip().lower()


def _finite_number(value: Any) -> float | None:
    """Coerce a JSON numeric value to a finite float without accepting bools."""
    if isinstance(value, bool) or value is None:
        return None
    try:
        number = float(value)
    except (TypeError, ValueError, OverflowError):
        return None
    return number if math.isfinite(number) else None


def _integer(value: Any) -> int | None:
    """Accept integer values and decimal/0x strings found in JSON ledgers."""
    if isinstance(value, bool) or value is None:
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, str):
        try:
            return int(value, 0) if value.lower().startswith("0x") else int(value)
        except ValueError:
            return None
    try:
        converted = int(value)
    except (TypeError, ValueError, OverflowError):
        return None
    return converted if converted == value else None


def route_start(arb: Mapping[str, Any]) -> str | None:
    """The token a route starts in: its sole gain token (R37)."""
    gains = arb.get("gain")
    if not isinstance(gains, (list, tuple)) or len(gains) != 1:
        return None
    return _normalise(gains[0])


def route_key(arb: Mapping[str, Any]) -> tuple[str, str] | None:
    """Return (start-in pool, other pool), preserving route direction.

    The start-in pool took the start token (R37's ``start_in``). A cache
    written before R37 has none: a WETH route falls back to ``weth_in``,
    and any other route is skipped, never guessed."""
    pools = arb.get("pools")
    start_in = _normalise(arb.get("start_in"))
    if start_in is None and "start_in" not in arb and route_start(arb) == _normalise(WETH):
        start_in = _normalise(arb.get("weth_in"))
    if not isinstance(pools, (list, tuple)) or len(pools) != 2 or start_in is None:
        return None
    normalised = [_normalise(pool) for pool in pools]
    if any(pool is None for pool in normalised):
        return None
    if normalised[0] == normalised[1] or normalised.count(start_in) != 1:
        return None
    other = normalised[1] if normalised[0] == start_in else normalised[0]
    return (start_in, other)


def _build_key(arb: Mapping[str, Any]) -> tuple[str, str, str] | None:
    """(start-in pool, other pool, start token): one pool order in two start
    tokens is two opposite trades."""
    key, start = route_key(arb), route_start(arb)
    return None if key is None or start is None else (key[0], key[1], start)


def _route_key(arb: Mapping[str, Any]) -> tuple[str, str] | None:
    """Backward-compatible private alias for route_key."""
    return route_key(arb)


def select(arb: Mapping[str, Any]) -> bool:
    """Whether an observed arbitrage belongs in the phase-1 target universe.

    Eligibility matches the R36 implementation-plan contract, as R37
    widened it: exactly two hops, both venues in ``race.OURS`` or Uniswap
    v4, WETH or USDC as the sole positive gain token, a known start-in pool,
    and offset zero (the trigger block itself).
    """
    if not isinstance(arb, Mapping):
        return False

    pools = arb.get("pools")
    if not isinstance(pools, (list, tuple)) or len(pools) != 2:
        return False
    hops = _integer(arb.get("hops", len(pools)))
    if hops != 2:
        return False

    venues = arb.get("venues")
    if not isinstance(venues, (list, tuple)) or not venues:
        return False
    our_venues = {str(venue).lower() for venue in VENUES}
    if any(not isinstance(venue, str) or venue.lower() not in our_venues for venue in venues):
        return False

    if route_start(arb) not in {_normalise(token) for token in STARTS}:
        return False

    if _integer(arb.get("offset")) != 0:
        return False
    return _route_key(arb) is not None


def _observed_bid(arb: Mapping[str, Any]) -> int | None:
    """Return a receipt-backed effective priority fee in wei per gas.

    R34's ``prio`` field is already ``effectiveGasPrice - baseFeePerGas``;
    do not multiply it by gas or convert it into a share of arbitrage value.
    ``gas`` is still required to establish that the record has a usable
    execution receipt rather than a placeholder observation.
    """
    bid = _integer(arb.get("prio"))
    gas = _integer(arb.get("gas"))
    if bid is None or gas is None or bid < 0 or bid > MAX_U128 or gas <= 0:
        return None
    return bid


def _day_label(day: Mapping[str, Any]) -> str | None:
    value = day.get("date") or day.get("day")
    if not isinstance(value, str):
        return None
    candidate = value[:10]
    try:
        return date.fromisoformat(candidate).isoformat()
    except ValueError:
        return None


def calibration(fit_bids: Sequence[int], held_out_bids: Sequence[int]) -> list[dict[str, Any]]:
    """Compare empirical bid-CDF fit with the last-day holdout.

    The candidate bids are the fit sample's 25th, 50th, 75th and 90th
    percentile order statistics using index ``floor(n * p)`` (clamped to the
    final element). ``fit`` and ``held_out`` are the shares of observations
    *strictly below* that candidate bid, matching the runtime's F(b) contract.
    """
    fit = [_integer(x) for x in fit_bids]
    held_out = [_integer(x) for x in held_out_bids]
    if any(x is None or x < 0 for x in fit + held_out):
        raise ValueError("calibration bids must be non-negative integers")
    fit_values = sorted(x for x in fit if x is not None)
    held_values = [x for x in held_out if x is not None]
    if not fit_values or not held_values:
        return []

    rows: list[dict[str, Any]] = []
    for percentile in (0.25, 0.50, 0.75, 0.90):
        index = min(len(fit_values) - 1, int(len(fit_values) * percentile))
        bid = fit_values[index]
        fit_share = sum(value < bid for value in fit_values) / len(fit_values)
        held_share = sum(value < bid for value in held_values) / len(held_values)
        rows.append({"bid": bid, "fit": fit_share, "held_out": held_share})
    return rows


def _selected_route_observations(
    days: Sequence[Mapping[str, Any]], selected_routes: set[tuple[str, str, str]],
) -> tuple[list[int], dict[tuple[str, str, str], list[int]], list[list[int]]]:
    """Return bids for the selected route universe, with one list per day."""
    all_bids: list[int] = []
    by_route: dict[tuple[str, str, str], list[int]] = defaultdict(list)
    by_day: list[list[int]] = []
    for day in days:
        daily: list[int] = []
        for arb in day.get("arbs", []):
            if not select(arb):
                continue
            key = _build_key(arb)
            if key not in selected_routes:
                continue
            # Keep calibration and the aggregate bid CDF consistent with the
            # observations admitted to route ranking and per-route samples.
            usd = _finite_number(arb.get("usd"))
            if usd is None or usd < 0:
                continue
            bid = _observed_bid(arb)
            if bid is None:
                continue
            daily.append(bid)
            all_bids.append(bid)
            by_route[key].append(bid)
        by_day.append(daily)
    return all_bids, by_route, by_day


def _all_finite(value: Any) -> bool:
    """Recursively ensure the JSON payload contains no NaN or infinities."""
    if isinstance(value, float):
        return math.isfinite(value)
    if isinstance(value, Mapping):
        return all(_all_finite(k) and _all_finite(v) for k, v in value.items())
    if isinstance(value, (list, tuple)):
        return all(_all_finite(item) for item in value)
    return True


def build(days: Sequence[Mapping[str, Any]], top: int = 50) -> dict[str, Any]:
    """Build Task 2's ``targets.json`` from daily R34 race results.

    Route ranking uses accumulated trigger-block arbitrage value. Bid samples
    are raw effective priority fees per gas in wei and are drawn only from the
    selected top-K routes, not from routes the runtime will never arm.
    """
    if isinstance(top, bool) or not isinstance(top, int) or top < 0:
        raise ValueError("top must be a non-negative integer")

    by_route: dict[tuple[str, str, str], dict[str, Any]] = {}
    day_labels: list[str] = []
    finite_source_data = True

    for day in days:
        label = _day_label(day)
        if label and label not in day_labels:
            day_labels.append(label)
        arbs = day.get("arbs", [])
        if not isinstance(arbs, list):
            raise ValueError(f"day {label or '<unknown>'} has a non-list arbs field")

        for arb in arbs:
            if not select(arb):
                continue
            key = _build_key(arb)
            if key is None:
                continue
            raw_usd = arb.get("usd")
            usd = _finite_number(raw_usd)
            if usd is None:
                # Don't silently mark a partial, malformed census live-ready.
                finite_source_data = False
                continue
            if usd < 0:
                finite_source_data = False
                continue

            raw_prio, raw_gas = arb.get("prio"), arb.get("gas")
            if (raw_prio is None) != (raw_gas is None):
                finite_source_data = False
            if raw_prio is not None:
                parsed_prio = _integer(raw_prio)
                if parsed_prio is None or parsed_prio < 0 or parsed_prio > MAX_U128:
                    finite_source_data = False
            if raw_gas is not None:
                parsed_gas = _integer(raw_gas)
                if parsed_gas is None or parsed_gas <= 0:
                    finite_source_data = False

            item = by_route.setdefault(key, {"pools": [key[0], key[1]], "start": key[2], "usd": 0.0,
                                             "arbs": 0, "_bids": []})
            item["usd"] += usd
            item["arbs"] += 1
            bid = _observed_bid(arb)
            if bid is not None:
                item["_bids"].append(bid)
            if not math.isfinite(item["usd"]):
                finite_source_data = False

    ordered = sorted(by_route.items(), key=lambda pair: (-pair[1]["usd"], pair[0]))
    selected_pairs = ordered[:top]
    selected_keys = {key for key, _ in selected_pairs}

    routes: list[dict[str, Any]] = []
    for key, item in selected_pairs:
        row: dict[str, Any] = {"pools": item["pools"], "start": item["start"], "usd": item["usd"], "arbs": item["arbs"]}
        if len(item["_bids"]) >= MIN_ROUTE_BIDS:
            row["bids_wei_per_gas"] = sorted(item["_bids"])
        routes.append(row)

    all_bids, _, bids_by_day = _selected_route_observations(days, selected_keys)
    # The last complete day is held out. With one day there is no fit/holdout
    # split; with no bid samples on either side calibration is explicitly empty.
    fit_bids = [bid for daily in bids_by_day[:-1] for bid in daily]
    held_out_bids = bids_by_day[-1] if bids_by_day else []
    calibration_rows = calibration(fit_bids, held_out_bids)

    # Production passes distinct, dated days. Keep no-date unit fixtures useful,
    # but do not call duplicated dated inputs seven-day coverage.
    labels_for_input = [_day_label(day) for day in days]
    if any(label is not None for label in labels_for_input):
        enough_days = (
            all(label is not None for label in labels_for_input)
            and len(set(labels_for_input)) >= 3
            and len(set(labels_for_input)) == len(labels_for_input)
        )
    else:
        # Unlabelled synthetic fixtures stand for distinct input days. The
        # production CLI always attaches an exact UTC date to every day.
        enough_days = len(days) >= 3

    now = datetime.now(UTC).isoformat(timespec="seconds").replace("+00:00", "Z")
    payload: dict[str, Any] = {
        "generated_at": now,
        "days": day_labels,
        "top": top,
        "ok": False,
        "routes": routes,
        "bids_wei_per_gas": sorted(all_bids),
        "calibration": calibration_rows,
    }
    payload["ok"] = bool(enough_days and routes and finite_source_data and _all_finite(payload))
    return payload


def ours_vs_market(days: Sequence[Mapping[str, Any]], landings: Iterable[Mapping[str, Any]]) -> list[dict[str, Any]]:
    """Compare trigger-block market value with realised Landed records by day/route.

    Task 8 records route pools WETH-in first. Matching preserves that order,
    so reverse-direction opportunities remain distinct. If test/legacy input
    has no day labels at all, the function falls back to a route-level aggregate
    without a ``day`` field; production census data is date-labelled.
    """
    labelled_days = {label for day in days if (label := _day_label(day))}
    use_days = bool(labelled_days)
    market: dict[tuple[str | None, tuple[str, str]], float] = defaultdict(float)
    ours: dict[tuple[str | None, tuple[str, str]], dict[str, int]] = defaultdict(
        lambda: {"ours_landed": 0, "ours_net_wei": 0}
    )

    for day in days:
        label = _day_label(day)
        arbs = day.get("arbs", [])
        if not isinstance(arbs, list):
            continue
        for arb in arbs:
            if not select(arb):
                continue
            key = route_key(arb)
            usd = _finite_number(arb.get("usd"))
            if key is not None and usd is not None and usd >= 0:
                market[(label if use_days else None, key)] += usd

    for record in landings:
        if not isinstance(record, Mapping) or record.get("kind") != "landed":
            continue
        pools = record.get("pools")
        if not isinstance(pools, (list, tuple)) or len(pools) != 2:
            continue
        route = (_normalise(pools[0]), _normalise(pools[1]))
        if route[0] is None or route[1] is None or route[0] == route[1]:
            continue
        at_ms = _integer(record.get("at_ms"))
        if use_days:
            if at_ms is None:
                continue
            landed_day = datetime.fromtimestamp(at_ms / 1000, tz=UTC).date().isoformat()
            if landed_day not in labelled_days:
                continue
            label_for_key: str | None = landed_day
        else:
            label_for_key = None

        net = _integer(record.get("net_wei"))
        if net is None:
            raise ValueError(f"Landed record for pools {pools!r} has invalid net_wei")
        ours[(label_for_key, route)]["ours_landed"] += 1
        ours[(label_for_key, route)]["ours_net_wei"] += net

    keys = set(market) | set(ours)
    rows: list[dict[str, Any]] = []
    for day_label, pools in keys:
        row: dict[str, Any] = {
            "pools": [pools[0], pools[1]],
            "market_usd": market.get((day_label, pools), 0.0),
            "ours_landed": ours[(day_label, pools)]["ours_landed"],
            "ours_net_wei": ours[(day_label, pools)]["ours_net_wei"],
        }
        if use_days and day_label is not None:
            row["day"] = day_label
        rows.append(row)
    if use_days:
        rows.sort(key=lambda row: (row.get("day", ""), -row["market_usd"], row["pools"][0], row["pools"][1]))
    else:
        rows.sort(key=lambda row: (-row["market_usd"], row["pools"][0], row["pools"][1]))
    return rows

def _first_block_at_or_after(rpc: Any, head: int, timestamp: int) -> int:
    """Binary-search the first Base block whose timestamp >= ``timestamp``.

    Returns ``head + 1`` if the requested timestamp is beyond the current
    head. This handles Denim's shorter block cadence without assuming a fixed
    blocks-per-day constant.
    """
    low, high = 0, head + 1
    while low < high:
        mid = (low + high) // 2
        if mid > head:
            high = mid
            continue
        block = rpc.call("eth_getBlockByNumber", [hex(mid), False])
        if not block or "timestamp" not in block:
            raise RuntimeError(f"cannot read timestamp for block {mid}")
        observed = int(block["timestamp"], 16)
        if observed >= timestamp:
            high = mid
        else:
            low = mid + 1
    return low


def _utc_day_block_range(rpc: Any, day: str) -> tuple[int, int]:
    """Find exact block bounds for a complete UTC day, independent of cadence."""
    day_date = date.fromisoformat(day)
    start = int(datetime.combine(day_date, time.min, tzinfo=UTC).timestamp())
    end = int(datetime.combine(day_date + timedelta(days=1), time.min, tzinfo=UTC).timestamp())
    head = int(rpc.call("eth_blockNumber", []), 16)
    first = _first_block_at_or_after(rpc, head, start)
    stop = _first_block_at_or_after(rpc, head, end)
    if first > head or stop > head:
        raise RuntimeError(f"UTC day {day} is not complete at the current chain head")
    if stop <= first:
        raise RuntimeError(f"no blocks found for UTC day {day}")
    return first, stop - 1


def _route_schema_present(data: Mapping[str, Any]) -> bool:
    arbs = data.get("arbs")
    required = ("pools", "weth_in", "gain", "venues", "hops", "offset", "usd")
    return isinstance(arbs, list) and (not arbs or all(
        isinstance(arb, Mapping) and all(key in arb for key in required)
        for arb in arbs
    ))


def _block_offsets_match(data: Mapping[str, Any]) -> bool:
    """Verify every cached offset is a block delta when no flashblock ledger exists."""
    for arb in data.get("arbs", []):
        trigger = arb.get("trigger")
        offset = arb.get("offset")
        if trigger is None:
            if offset is not None:
                return False
            continue
        if not isinstance(trigger, (list, tuple)) or not trigger:
            return False
        block = _integer(arb.get("block"))
        trigger_block = _integer(trigger[0])
        offset_int = _integer(offset)
        if block is None or trigger_block is None or offset_int != block - trigger_block:
            return False
    return True


def _cache_current(data: Mapping[str, Any], day: str, has_flashblocks: bool) -> bool:
    """Avoid rereading chain history only when cache day/schema/offset semantics match."""
    expected_mode = "flashblock" if has_flashblocks else "block_number"
    if data.get("_targets_cache_schema") == CACHE_SCHEMA_VERSION:
        if data.get("date") != day or not _route_schema_present(data):
            return False
        if data.get("_targets_offset_mode") != expected_mode:
            return False
        return has_flashblocks or _block_offsets_match(data)

    # If another cache writer stamped a mode but the available evidence has
    # changed (for example, the flashblock ledger has disappeared at Denim),
    # do not trust its former offset semantics. Recompute once in the new mode.
    if data.get("_targets_offset_mode") in ("flashblock", "block_number") and data.get("_targets_offset_mode") != expected_mode:
        return False
    if not _route_schema_present(data):
        return False
    if has_flashblocks:
        return True
    return _block_offsets_match(data)


def _load_race_cache(path: Path) -> dict[str, Any] | None:
    if not path.exists():
        return None
    try:
        with gzip.open(path, "rt", encoding="utf-8") as stream:
            result = json.load(stream)
        return result if isinstance(result, dict) else None
    except (OSError, EOFError, json.JSONDecodeError):
        return None


def _atomic_json(path: Path, payload: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".tmp")
    try:
        with temporary.open("w", encoding="utf-8") as stream:
            json.dump(payload, stream, separators=(",", ":"), ensure_ascii=True, allow_nan=False)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        _fsync_directory(path.parent)
    finally:
        try:
            temporary.unlink()
        except FileNotFoundError:
            pass


def write(obj: Mapping[str, Any], path: str | os.PathLike[str]) -> None:
    """Atomically write JSON and reject NaN/Infinity (R36 safety contract)."""
    _atomic_json(Path(path), obj)


def _atomic_gzip_json(path: Path, payload: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".tmp")
    try:
        with gzip.open(temporary, "wt", encoding="utf-8") as stream:
            json.dump(payload, stream, separators=(",", ":"), ensure_ascii=True, allow_nan=False)
            stream.write("\n")
        os.replace(temporary, path)
        _fsync_directory(path.parent)
    finally:
        try:
            temporary.unlink()
        except FileNotFoundError:
            pass


def _fsync_directory(path: Path) -> None:
    """Best-effort directory sync so an atomic rename survives a host crash."""
    flags = getattr(os, "O_DIRECTORY", 0) | os.O_RDONLY
    try:
        fd = os.open(path, flags)
    except OSError:
        return
    try:
        os.fsync(fd)
    except OSError:
        pass
    finally:
        os.close(fd)


def _ensure_race_day(day: str, race_dir: Path, var_dir: Path,
                     rpc: Any, resolver: Any) -> dict[str, Any]:
    cache_path = race_dir / f"race-{day}.json.gz"
    flashblocks_path = var_dir / f"flashblocks-{day}.jsonl"
    has_flashblocks = flashblocks_path.is_file() and flashblocks_path.stat().st_size > 0
    cached = _load_race_cache(cache_path)
    if cached is not None and _cache_current(cached, day, has_flashblocks):
        cached["date"] = day
        cached["_targets_cache_schema"] = CACHE_SCHEMA_VERSION
        cached["_targets_offset_mode"] = "flashblock" if has_flashblocks else "block_number"
        _atomic_gzip_json(cache_path, cached)
        return cached

    if has_flashblocks:
        lo, hi = race.day_range(str(var_dir), day)
    else:
        # Denim does not provide the old flashblock ledger. Block timestamps
        # establish exact UTC-day bounds; R36 Task 1 must use block offsets.
        lo, hi = _utc_day_block_range(rpc, day)
    if lo is None or hi is None or hi < lo:
        raise RuntimeError(f"cannot determine valid block range for {day}: {lo}..{hi}")

    print(f"{day}: rebuilding race census for blocks {lo}..{hi} "
          f"({'flashblock ledger' if has_flashblocks else 'block-offset fallback'})", flush=True)
    ledger = race.day_ledger(str(var_dir), lo, hi)
    data = race.race_day(rpc, resolver, ledger, lo, hi)
    if not isinstance(data, dict) or not _route_schema_present(data):
        raise RuntimeError(
            "race census output lacks Task 1 fields ('pools', 'weth_in', 'gain'); "
            "merge and test R36 Task 1 before running Task 2"
        )
    data["date"] = day
    data["_targets_cache_schema"] = CACHE_SCHEMA_VERSION
    data["_targets_offset_mode"] = "flashblock" if has_flashblocks else "block_number"
    _atomic_gzip_json(cache_path, data)
    print(f"{day}: cached {len(data.get('arbs', []))} arbitrages", flush=True)
    return data


def _load_landings(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    records: list[dict[str, Any]] = []
    with path.open("r", encoding="utf-8") as stream:
        for line_number, line in enumerate(stream, 1):
            if not line.strip():
                continue
            try:
                value = json.loads(line)
            except json.JSONDecodeError as exc:
                raise ValueError(f"invalid JSON in {path}:{line_number}: {exc.msg}") from exc
            if isinstance(value, dict) and value.get("kind") == "landed":
                records.append(value)
    return records


def _complete_utc_days(count: int, now: datetime | None = None) -> list[str]:
    if count < 1:
        raise ValueError("days must be >= 1")
    current = (now or datetime.now(UTC)).astimezone(UTC).date()
    # Exclude the current UTC day: partial-day data biases both route ranking
    # and bid-share observations downward.
    return [(current - timedelta(days=delta)).isoformat() for delta in range(count, 0, -1)]


def publish(targets: Mapping[str, Any], out: Path) -> bool:
    """Write fit targets to ``out``. An unfit file goes beside it, as
    ``<stem>.unfit.json``, and never replaces the last good one: the job then
    fails, so its ExecStartPost does not restart the canary into a live boot
    that would refuse it."""
    if targets.get("ok") is True:
        _atomic_json(out, targets)
        return True
    _atomic_json(out.with_name(f"{out.stem}.unfit.json"), targets)
    return False


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Build Armed Lane v2 route targets")
    parser.add_argument("--race", required=True, help="directory holding race-YYYY-MM-DD.json.gz")
    parser.add_argument("--days", type=int, default=7, help="number of complete trailing UTC days (default: 7)")
    parser.add_argument("--top", type=int, default=50, help="maximum number of routes (default: 50)")
    parser.add_argument("--out", required=True, help="destination targets.json file")
    parser.add_argument("--share-out", default=None, help="optional per-day/per-route market-versus-our-landings JSON")
    parser.add_argument("--var", default="/opt/apex/var/apex", help="directory containing flashblock ledgers")
    parser.add_argument("--meta", required=True, help="pool metadata cache used by arb_census.race")
    parser.add_argument("--gaps", default=None, help="legacy R33 gaps path accepted for ops compatibility; not used by the R36 bid model")
    parser.add_argument("--live-ledger", required=True, help="Task 8 armed-live.jsonl path")
    parser.add_argument("--rate", type=float, default=6.0, help="RPC calls per second per endpoint")
    args = parser.parse_args(argv)

    if args.days < 1:
        parser.error("--days must be >= 1")
    if args.top < 1:
        parser.error("--top must be >= 1")
    if not math.isfinite(args.rate) or args.rate <= 0:
        parser.error("--rate must be finite and > 0")

    race_dir, var_dir, meta_path = Path(args.race), Path(args.var), Path(args.meta)
    race_dir.mkdir(parents=True, exist_ok=True)
    if args.gaps:
        print("R36 targets: --gaps accepted for host-command compatibility; bid calibration uses prio per gas.", flush=True)

    days_to_process = _complete_utc_days(args.days)
    rpc = race.Public(args.rate)
    resolver = race.Resolving(rpc, str(meta_path))
    days: list[dict[str, Any]] = []
    for day in days_to_process:
        data = _ensure_race_day(day, race_dir, var_dir, rpc, resolver)
        data["date"] = day
        days.append(data)

    targets = build(days, args.top)
    # Readiness is the file's own ok, never merely successful JSON IO; an
    # unfit file is kept for the operator beside the last good one.
    published = publish(targets, Path(args.out))
    print(f"{'wrote' if published else 'NOT published (unfit)'}: {len(targets['routes'])} routes / "
          f"{len(targets['bids_wei_per_gas'])} bid observations; ok={str(targets['ok']).lower()}", flush=True)

    if args.share_out:
        landings = _load_landings(Path(args.live_ledger))
        comparison = ours_vs_market(days, landings)
        _atomic_json(Path(args.share_out), comparison)
        print(f"wrote {len(comparison)} per-day/per-route market comparisons to {args.share_out}", flush=True)
    return 0 if published else 1


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, RuntimeError, ValueError, OverflowError, gzip.BadGzipFile) as exc:
        print(f"arb_census.targets: {exc}", file=sys.stderr)
        raise SystemExit(1)
