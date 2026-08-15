#!/usr/bin/env bash
set -euo pipefail

OPS_INPUTS="${OPS_INPUTS:-ops/inputs.yaml}"

if [[ ! -f "$OPS_INPUTS" ]]; then
  echo "missing ops inputs at $OPS_INPUTS" >&2
  exit 1
fi

python3 - <<'PY'
import os
import sys

ops_path = os.environ.get("OPS_INPUTS", "ops/inputs.yaml")

try:
    import yaml
except Exception as exc:
    sys.stderr.write("python yaml module is required (pip install pyyaml)\n")
    sys.exit(1)

with open(ops_path, "r", encoding="utf-8") as fh:
    data = yaml.safe_load(fh) or {}

chains = data.get("chains", []) or []


def is_placeholder(v: str) -> bool:
    return isinstance(v, str) and "${" in v and "}" in v

chain_list = []
for chain in chains:
    name = (chain.get("chain_name") or "").strip()
    prefix = (chain.get("env_prefix") or "").strip()
    if not name or not prefix:
        continue
    chain_list.append(name)
    rpc_http = chain.get("rpc_http_urls", []) or []
    rpc_ws = chain.get("rpc_ws_urls", []) or []
    print(f'export {prefix}_RPC_URLS="{",".join(rpc_http)}"')
    if rpc_ws:
        print(f'export {prefix}_WS_RPC_URLS="{",".join(rpc_ws)}"')
    val = chain.get("executor_address")
    if val and not is_placeholder(val):
        print(f'export {prefix}_EXECUTOR_ADDRESS="{val}"')
    val = chain.get("executor_owner")
    if val and not is_placeholder(val):
        print(f'export {prefix}_EXECUTOR_OWNER="{val}"')
    if chain.get("permit2_address"):
        print(f'export {prefix}_PERMIT2_ADDRESS="{chain["permit2_address"]}"')

    for venue in chain.get("venues", []) or []:
        kind = (venue.get("kind") or "").lower()
        if kind == "univ3_like":
            if venue.get("quoter"):
                print(f'export {prefix}_UNIV3_QUOTER="{venue["quoter"]}"')
            if venue.get("factory"):
                print(f'export {prefix}_UNIV3_FACTORY="{venue["factory"]}"')
            if venue.get("router"):
                print(f'export {prefix}_UNIV3_ROUTER="{venue["router"]}"')
        if kind == "balancer_like" and venue.get("vault"):
            print(f'export {prefix}_BAL_VAULT="{venue["vault"]}"')

    for loan in chain.get("flashloans", []) or []:
        kind = (loan.get("kind") or "").lower()
        if kind == "aave_v3_like" and loan.get("pool"):
            print(f'export {prefix}_AAVE_POOL="{loan["pool"]}"')
        if kind == "balancer_vault_like" and loan.get("vault"):
            print(f'export {prefix}_BAL_VAULT="{loan["vault"]}"')

if chain_list:
    print(f'export CHAIN_LIST="{",".join(chain_list)}"')
PY
