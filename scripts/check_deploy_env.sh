#!/usr/bin/env bash
set -euo pipefail

chain_raw="${ENV_PREFIX:-${CHAIN:-}}"
if [[ -z "$chain_raw" ]]; then
  echo "error: set ENV_PREFIX or CHAIN so deploy env keys can be resolved"
  exit 1
fi

chain_upper="$(echo "$chain_raw" | tr '[:lower:]' '[:upper:]' | tr ' -' '__')"
case "$chain_upper" in
  ETHEREUM) prefix="ETH" ;;
  ARBITRUM) prefix="ARB" ;;
  OPTIMISM) prefix="OPT" ;;
  BASE|LINEA|ABSTRACT|INK) prefix="$chain_upper" ;;
  *) prefix="$chain_upper" ;;
esac

required_missing=0

prefix_alias=""
case "$prefix" in
  ETH) prefix_alias="ETHEREUM" ;;
  ETHEREUM) prefix_alias="ETH" ;;
  ARB) prefix_alias="ARBITRUM" ;;
  ARBITRUM) prefix_alias="ARB" ;;
  OPT) prefix_alias="OPTIMISM" ;;
  OPTIMISM) prefix_alias="OPT" ;;
esac

has_any() {
  local k
  for k in "$@"; do
    if [[ -n "${!k:-}" ]]; then
      return 0
    fi
  done
  return 1
}

require_any() {
  local label="$1"
  shift
  if has_any "$@"; then
    echo "ok: ${label}"
  else
    echo "missing: ${label} (expected one of: $*)"
    required_missing=1
  fi
}

inputs_file="${INPUTS_YAML:-ops/inputs.yaml}"
if [[ ! -f "$inputs_file" ]]; then
  echo "error: inputs file not found: ${inputs_file}"
  exit 1
fi

read -r require_balancer require_aave <<<"$(ruby - "$inputs_file" "$prefix" "$prefix_alias" "$chain_upper" <<'RUBY'
require "yaml"

inputs_file, prefix, prefix_alias, chain_upper = ARGV

def canonicalize(value)
  value.to_s.upcase.tr(" -", "__")
end

aliases = {
  "ETH" => "ETHEREUM",
  "ETHEREUM" => "ETH",
  "ARB" => "ARBITRUM",
  "ARBITRUM" => "ARB",
  "OPT" => "OPTIMISM",
  "OPTIMISM" => "OPT"
}

wanted = [prefix, prefix_alias, chain_upper, aliases[chain_upper]].compact.map { |v| canonicalize(v) }.uniq

inputs = YAML.load_file(inputs_file) || {}
chains = inputs.fetch("chains", [])
selected = chains.find do |chain|
  env_prefix = canonicalize(chain["env_prefix"])
  chain_name = canonicalize(chain["chain_name"])
  wanted.include?(env_prefix) || wanted.include?(chain_name)
end

if selected.nil?
  warn "error: chain '#{chain_upper}' not found in #{inputs_file}"
  exit 2
end

kinds = selected.fetch("flashloans", []).map { |f| f["kind"].to_s.downcase }
require_balancer = kinds.any? { |k| k.start_with?("balancer") }
require_aave = kinds.any? { |k| k.start_with?("aave") }

puts "#{require_balancer} #{require_aave}"
RUBY
)"

if [[ "$require_balancer" == "true" ]]; then
  export REQUIRE_BALANCER=true
  export "${prefix}_REQUIRE_BALANCER"=true
  echo "ok: REQUIRE_BALANCER=true (derived from ${inputs_file} flashloans[*].kind)"
else
  echo "ok: REQUIRE_BALANCER=false (no balancer flashloan configured in ${inputs_file})"
fi

if [[ "$require_aave" == "true" ]]; then
  export REQUIRE_AAVE=true
  export "${prefix}_REQUIRE_AAVE"=true
  echo "ok: REQUIRE_AAVE=true (derived from ${inputs_file} flashloans[*].kind)"
else
  echo "ok: REQUIRE_AAVE=false (no aave flashloan configured in ${inputs_file})"
fi

require_any "executor owner" "${prefix}_EXECUTOR_OWNER" "${prefix_alias}_EXECUTOR_OWNER" "EXECUTOR_OWNER"
require_any "uniswap v3 router" "${prefix}_UNIV3_ROUTER" "${prefix_alias}_UNIV3_ROUTER" "${prefix}_SWAPROUTER02" "${prefix_alias}_SWAPROUTER02" "UNIV3_ROUTER" "SWAPROUTER02"
if [[ "$prefix" == "ETH" || "$prefix" == "ETHEREUM" ]]; then
  if has_any "${prefix}_PERMIT2_ADDRESS" "${prefix_alias}_PERMIT2_ADDRESS" "${prefix}_PERMIT2" "${prefix_alias}_PERMIT2" "PERMIT2_ADDRESS" "PERMIT2"; then
    echo "ok: permit2"
  else
    echo "ok: permit2 (using deploy built-in Ethereum fallback 0x000000000022D473030F116dDEE9F6B43aC78BA3)"
  fi
else
  require_any "permit2" "${prefix}_PERMIT2_ADDRESS" "${prefix_alias}_PERMIT2_ADDRESS" "${prefix}_PERMIT2" "${prefix_alias}_PERMIT2" "PERMIT2_ADDRESS" "PERMIT2"
fi

if [[ "$require_balancer" == "true" ]]; then
  if [[ "$prefix" == "ETH" || "$prefix" == "ETHEREUM" ]]; then
    if has_any "${prefix}_BAL_VAULT" "${prefix_alias}_BAL_VAULT" "${prefix}_BALANCER_VAULT" "${prefix_alias}_BALANCER_VAULT" "BAL_VAULT" "BALANCER_VAULT"; then
      echo "ok: balancer vault"
    else
      echo "ok: balancer vault (using deploy built-in Ethereum fallback 0xBA12222222228d8Ba445958a75a0704d566BF2C8)"
    fi
  else
    require_any "balancer vault" "${prefix}_BAL_VAULT" "${prefix_alias}_BAL_VAULT" "${prefix}_BALANCER_VAULT" "${prefix_alias}_BALANCER_VAULT" "BAL_VAULT" "BALANCER_VAULT"
  fi
else
  echo "ok: balancer vault not required for ${prefix}"
fi

if [[ "$require_aave" == "true" ]]; then
  if [[ "$prefix" == "ETH" || "$prefix" == "ETHEREUM" ]]; then
    if has_any "${prefix}_AAVE_POOL" "${prefix_alias}_AAVE_POOL" "AAVE_POOL"; then
      echo "ok: aave pool"
    else
      echo "ok: aave pool (using deploy built-in Ethereum fallback 0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2)"
    fi
  else
    require_any "aave pool" "${prefix}_AAVE_POOL" "${prefix_alias}_AAVE_POOL" "AAVE_POOL"
  fi
else
  echo "ok: aave pool not required for ${prefix}"
fi

if [[ "$required_missing" -ne 0 ]]; then
  echo "error: missing required deploy environment keys"
  exit 1
fi

echo "deploy env preflight passed for prefix=${prefix}"
