#!/usr/bin/env bash
set -euo pipefail

# Generates a minimal shadow log sample for local parsing smoke tests.
# Useful when no real shadow-mode run has been executed yet.

LOG_PATH="${1:-logs/shadow-mainnet.jsonl}"
mkdir -p "$(dirname "$LOG_PATH")"

cat >"$LOG_PATH" <<'JSONL'
{"timestamp_ms":1730000000000,"chain_env":"mainnet","tag":"sample","tx_hash":"0x1111111111111111111111111111111111111111111111111111111111111111","to":"0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","gas_limit":"450000","max_fee_per_gas":"35000000000","max_priority_fee_per_gas":"3000000000","gas_price":null,"value":"0","data_len":320,"cycle_start":"0x0000000000000000000000000000000000000000","amount_in_wei":"150000000000000000","est_gross_after_fee_wei":"151200000000000000","net_profit_wei":"900000000000000","gas_cost_wei":"120000000000000","min_profit_wei":"500000000000000","max_slippage_bps":75,"hops":3}
{"timestamp_ms":1730000000500,"chain_env":"arbitrum","tag":"sample","tx_hash":"0x2222222222222222222222222222222222222222222222222222222222222222","to":"0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","gas_limit":"320000","max_fee_per_gas":null,"max_priority_fee_per_gas":null,"gas_price":"450000000","value":"0","data_len":288,"cycle_start":"0x0000000000000000000000000000000000000001","amount_in_wei":"250000000","est_gross_after_fee_wei":"252500000","net_profit_wei":"1500000","gas_cost_wei":"400000","min_profit_wei":"1200000","max_slippage_bps":60,"hops":2}
JSONL

printf "Sample shadow log written to %s\n" "$LOG_PATH"
