//! R37: a v4 route's other token is admitted only if it moves exactly. One
//! `eth_simulateV1`: 1,000 units out of a pool to a fresh address and back,
//! the fresh address's balance read before, between and after. A token that
//! takes a fee on transfer, or whose transfer reverts or returns false, does
//! not, and its pools are left unarmed: a hop would receive less than quoted.

use alloy_primitives::{address, hex, Address, U256};
use apex_chain::rpc::RpcTransport;
use serde_json::{json, Value};

/// Where the round trip's units go and come back from: an address of no
/// one's. A balance already there does not fail the check.
pub const FRESH: Address = address!("00000000000000000000000000000000a9e57037");
const UNITS: u64 = 1_000;

/// Whether `token` moves exactly at `block`: `holder` (a pool of the token,
/// or PoolManager for a v4 one) sends 1,000 units to [`FRESH`], which sends
/// them back, each transfer succeeding and each landing whole. An answer
/// that does not read is an error.
pub async fn moves_exactly(rpc: &dyn RpcTransport, token: Address, holder: Address, block: u64) -> Result<bool, String> {
    let t = format!("{token:#x}");
    let balance = json!({ "to": t, "data": format!("0x70a08231{:0>64}", hex::encode(FRESH)) });
    let transfer = |from: Address, to: Address| {
        json!({ "from": format!("{from:#x}"), "to": t, "data": format!("0xa9059cbb{:0>64}{UNITS:064x}", hex::encode(to)) })
    };
    let calls = json!([balance, transfer(holder, FRESH), balance, transfer(FRESH, holder), balance]);
    let params = json!([{ "blockStateCalls": [{ "calls": calls }], "validation": false }, format!("{block:#x}")]);
    let out = rpc.call("eth_simulateV1", params).await.map_err(|e| e.to_string())?;
    let results = out.get(0).and_then(|b| b.get("calls")).and_then(Value::as_array).ok_or("eth_simulateV1: no calls")?;
    if results.len() != 5 {
        return Err(format!("eth_simulateV1 answered {} of 5 calls", results.len()));
    }
    let ok = |c: &Value| c.get("status").and_then(Value::as_str) == Some("0x1");
    let data = |c: &Value| c.get("returnData").and_then(Value::as_str).and_then(|s| hex::decode(s.trim_start_matches("0x")).ok());
    let balance_of = |c: &Value| data(c).filter(|d| ok(c) && d.len() == 32).map(|d| U256::from_be_slice(&d));
    // A transfer that returns nothing moved, as tokens without the bool do;
    // one that returns false did not.
    let moved = |c: &Value| ok(c) && data(c).is_some_and(|d| d.is_empty() || (d.len() == 32 && d.iter().any(|b| *b != 0)));
    let (Some(before), Some(between), Some(after)) = (balance_of(&results[0]), balance_of(&results[2]), balance_of(&results[4])) else {
        return Ok(false);
    };
    let units = U256::from(UNITS);
    Ok(moved(&results[1]) && moved(&results[3]) && between.checked_sub(before) == Some(units) && between.checked_sub(after) == Some(units))
}
