//! Diagnostic: does a `logs` subscription deliver through ethers' `Provider<Ws>`,
//! and does sharing one connection with `newHeads` starve it?
//!
//! wscat proved the SERVER handles our exact filter — array address, array
//! topics — on this endpoint. It cannot exercise the client stack, which is
//! where the fault now sits: production multiplexes `newHeads` (block head
//! monitor) and `logs` (pool monitor) onto ONE `Arc<Provider<Ws>>`, while every
//! wscat test used one subscription per connection.
//!
//! This reproduces both topologies against the same endpoint:
//!
//!   cargo run --bin ws_probe -- "wss://host/path" shared
//!   cargo run --bin ws_probe -- "wss://host/path" dedicated
//!
//! `shared`    — one provider, both subscriptions (production today)
//! `dedicated` — one provider each (the proposed fix)
//! `logs-only` — logs alone, as a control matching the passing wscat test
//!
//! A non-zero `logs` count in `dedicated` but zero in `shared` confirms the
//! multiplexing hypothesis and the fix. Zero in both, with `logs-only` working,
//! points at ethers' subscription routing rather than the provider.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use ethers::prelude::*;
use ethers::types::{Filter, H256};
use futures_util::StreamExt;
use std::str::FromStr;

/// Two Base pools verified to emit Sync and Swap several times per minute.
const POOLS: [&str; 2] = [
    "0x3f413fccaea59b8053d605aea7ae847c02ed5d95",
    "0x17a3ad8c74c4947005afeda9965305ae2eb2518a",
];

const OBSERVE: Duration = Duration::from_secs(60);

fn filter() -> Result<Filter> {
    let addresses: Vec<Address> = POOLS
        .iter()
        .map(|p| Address::from_str(p).context("pool address"))
        .collect::<Result<_>>()?;
    Ok(Filter::new().address(addresses).topic0(vec![
        H256::from_str("0x1c411e9a96e071241c2f21f7726b17ae89e3cab4c78be50e062b03a9fffbbad1")?,
        H256::from_str("0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822")?,
    ]))
}

async fn connect(url: &str) -> Result<Arc<Provider<Ws>>> {
    Ok(Arc::new(
        Provider::<Ws>::connect(url).await.context("ws connect")?,
    ))
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let url = args
        .next()
        .context("usage: ws_probe <wss-url> [shared|dedicated|logs-only]")?;
    let mode = args.next().unwrap_or_else(|| "shared".into());

    let f = filter()?;
    println!("mode      : {mode}");
    println!("filter    : {}", serde_json::to_string(&f)?);
    println!("observing : {}s\n", OBSERVE.as_secs());

    let heads = Arc::new(AtomicU64::new(0));
    let logs = Arc::new(AtomicU64::new(0));

    let (log_provider, head_provider) = match mode.as_str() {
        "shared" => {
            let p = connect(&url).await?;
            (p.clone(), Some(p))
        }
        "dedicated" => (connect(&url).await?, Some(connect(&url).await?)),
        "logs-only" => (connect(&url).await?, None),
        other => anyhow::bail!("unknown mode {other}; use shared|dedicated|logs-only"),
    };

    // Subscribe to logs FIRST, matching production ordering: the pool monitor
    // subscribed 8ms before the block head monitor, and logs is the dead one.
    let mut log_sub = log_provider
        .subscribe_logs(&f)
        .await
        .context("subscribe_logs")?;
    println!("logs subscription established");

    let mut head_sub = match head_provider.as_ref() {
        Some(hp) => {
            let s = hp.subscribe_blocks().await.context("subscribe_blocks")?;
            println!("newHeads subscription established");
            Some(s)
        }
        None => None,
    };

    println!();

    // Both streams borrow their provider, so they are driven here rather than
    // in spawned tasks. `pending()` parks the newHeads arm in logs-only mode.
    let deadline = tokio::time::Instant::now() + OBSERVE;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            Some(log) = log_sub.next() => {
                let n = logs.fetch_add(1, Ordering::SeqCst) + 1;
                println!(
                    "  [logs #{n}] {:?} topic0={:?} block={:?}",
                    log.address,
                    log.topics.first(),
                    log.block_number
                );
            }
            Some(b) = async {
                match head_sub.as_mut() {
                    Some(s) => s.next().await,
                    None => std::future::pending().await,
                }
            } => {
                let n = heads.fetch_add(1, Ordering::SeqCst) + 1;
                if n <= 3 || n % 10 == 0 {
                    println!("  [head #{n}] block={:?}", b.number);
                }
            }
        }
    }

    let l = logs.load(Ordering::SeqCst);
    let h = heads.load(Ordering::SeqCst);
    println!("\n=== RESULT ({mode}) ===");
    println!("logs     : {l}");
    println!("newHeads : {h}");
    if l == 0 {
        println!("\nlogs delivered NOTHING — reproduces the production fault.");
    } else {
        println!("\nlogs delivered {l} events — this topology works.");
    }

    Ok(())
}
