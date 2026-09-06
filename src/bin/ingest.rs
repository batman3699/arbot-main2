use anyhow::{anyhow, Context, Result};
use ethers::prelude::*;
use std::env;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{sleep, timeout};

// ops_inputs is a public library module (src/lib.rs), so consume it from the
// crate rather than re-compiling it here — a `#[path]` copy would be a separate
// module without access to the lib's `util` (which ops_inputs now depends on).
#[path = "../pool_store.rs"]
mod pool_store;

use arb_exec::ops_inputs::{load_ops_inputs, VenueKind};
use pool_store::{merge_pool_records, pool_data_path, write_pool_records, PoolRecord};

mod events {
    use ethers::prelude::abigen;
    abigen!(
        UniV2FactoryEvents,
        r#"[event PairCreated(address indexed token0, address indexed token1, address pair, uint256)]"#,
    );
    abigen!(
        UniV3FactoryEvents,
        r#"[event PoolCreated(address indexed token0, address indexed token1, uint24 indexed fee, int24 tickSpacing, address pool)]"#,
    );
}

const DEFAULT_BLOCK_CHUNK_SIZE: u64 = 10_000;
const DEFAULT_QUERY_TIMEOUT_SECS: u64 = 45;
const INTER_CHUNK_DELAY_MS: u64 = 350;

fn rpc_error_is_rate_limited(err: &impl std::fmt::Display) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("429")
        || msg.contains("rate limit")
        || msg.contains("compute units per second")
        || msg.contains("too many requests")
}

fn parse_args() -> Result<(String, String, u64, u64, u64, Duration)> {
    let mut chain = None;
    let mut venue = None;
    let mut from_block = None;
    let mut to_block = None;
    let mut chunk_size = Some(DEFAULT_BLOCK_CHUNK_SIZE);
    let mut query_timeout_secs = Some(DEFAULT_QUERY_TIMEOUT_SECS);

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--chain" => chain = args.next(),
            "--venue" => venue = args.next(),
            "--from-block" => from_block = args.next(),
            "--to-block" => to_block = args.next(),
            "--chunk-size" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--chunk-size requires a value"))?;
                chunk_size = Some(value.parse::<u64>().context("parse --chunk-size")?);
            }
            "--query-timeout-secs" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--query-timeout-secs requires a value"))?;
                query_timeout_secs =
                    Some(value.parse::<u64>().context("parse --query-timeout-secs")?);
            }
            other => return Err(anyhow!("unknown argument: {other}")),
        }
    }

    let chain = chain.ok_or_else(|| anyhow!("--chain is required"))?;
    let venue = venue.ok_or_else(|| anyhow!("--venue is required"))?;
    let from_block = from_block
        .ok_or_else(|| anyhow!("--from-block is required"))?
        .parse::<u64>()
        .context("parse --from-block")?;
    let to_block = to_block
        .ok_or_else(|| anyhow!("--to-block is required"))?
        .parse::<u64>()
        .context("parse --to-block")?;
    if to_block < from_block {
        return Err(anyhow!("--to-block must be >= --from-block"));
    }
    let chunk_size = chunk_size.ok_or_else(|| anyhow!("--chunk-size is required"))?;
    if chunk_size == 0 {
        return Err(anyhow!("--chunk-size must be > 0"));
    }
    let timeout_secs =
        query_timeout_secs.ok_or_else(|| anyhow!("--query-timeout-secs is required"))?;
    if timeout_secs == 0 {
        return Err(anyhow!("--query-timeout-secs must be > 0"));
    }

    Ok((
        chain,
        venue,
        from_block,
        to_block,
        chunk_size,
        Duration::from_secs(timeout_secs),
    ))
}

fn block_windows(
    from_block: u64,
    to_block: u64,
    chunk_size: u64,
) -> impl Iterator<Item = (u64, u64)> {
    let mut start = from_block;
    std::iter::from_fn(move || {
        if start > to_block {
            return None;
        }
        let end = start
            .saturating_add(chunk_size.saturating_sub(1))
            .min(to_block);
        let window = (start, end);
        start = end.saturating_add(1);
        Some(window)
    })
}

async fn ingest_univ2(
    provider: Arc<Provider<Http>>,
    factory: Address,
    fee_bps: u32,
    from_block: u64,
    to_block: u64,
    chunk_size: u64,
    query_timeout: Duration,
) -> Result<Vec<PoolRecord>> {
    let contract = events::UniV2FactoryEvents::new(factory, provider);

    let mut records = Vec::new();
    for (window_start, window_end) in block_windows(from_block, to_block, chunk_size) {
        println!("Ingesting UniV2 pool logs for block window [{window_start}, {window_end}]...");
        let mut backoff = Duration::from_secs(2);
        let events = loop {
            match timeout(
                query_timeout,
                contract
                    .event::<events::PairCreatedFilter>()
                    .from_block(window_start)
                    .to_block(window_end)
                    .query_with_meta(),
            )
            .await
            {
                Ok(Ok(events)) => break events,
                Ok(Err(err)) => {
                    if rpc_error_is_rate_limited(&err) {
                        eprintln!(
                            "RPC rate limited on PairCreated [{window_start}, {window_end}]; backing off {:?}",
                            backoff
                        );
                        sleep(backoff).await;
                        backoff = backoff.saturating_mul(2).min(Duration::from_secs(30));
                        continue;
                    }
                    return Err(anyhow::Error::msg(err.to_string()));
                }
                Err(_) => {
                    return Err(anyhow!(
                        "timed out querying PairCreated logs for block window [{window_start}, {window_end}] after {:?}",
                        query_timeout
                    ));
                }
            }
        };

        for (event, meta) in events {
            let block = meta.block_number.as_u64();
            records.push(PoolRecord {
                pool: event.pair,
                token0: event.token_0,
                token1: event.token_1,
                fee: fee_bps,
                created_block: block,
                hub_usd_liquidity: None,
                hub_symbol: None,
            });
        }
        sleep(Duration::from_millis(INTER_CHUNK_DELAY_MS)).await;
    }
    Ok(records)
}

async fn ingest_univ3(
    provider: Arc<Provider<Http>>,
    factory: Address,
    from_block: u64,
    to_block: u64,
    chunk_size: u64,
    query_timeout: Duration,
) -> Result<Vec<PoolRecord>> {
    let contract = events::UniV3FactoryEvents::new(factory, provider);

    let mut records = Vec::new();
    for (window_start, window_end) in block_windows(from_block, to_block, chunk_size) {
        println!("Ingesting UniV3 pool logs for block window [{window_start}, {window_end}]...");
        let mut backoff = Duration::from_secs(2);
        let events = loop {
            match timeout(
                query_timeout,
                contract
                    .event::<events::PoolCreatedFilter>()
                    .from_block(window_start)
                    .to_block(window_end)
                    .query_with_meta(),
            )
            .await
            {
                Ok(Ok(events)) => break events,
                Ok(Err(err)) => {
                    if rpc_error_is_rate_limited(&err) {
                        eprintln!(
                            "RPC rate limited on PoolCreated [{window_start}, {window_end}]; backing off {:?}",
                            backoff
                        );
                        sleep(backoff).await;
                        backoff = backoff.saturating_mul(2).min(Duration::from_secs(30));
                        continue;
                    }
                    return Err(anyhow::Error::msg(err.to_string()));
                }
                Err(_) => {
                    return Err(anyhow!(
                        "timed out querying PoolCreated logs for block window [{window_start}, {window_end}] after {:?}",
                        query_timeout
                    ));
                }
            }
        };

        for (event, meta) in events {
            let block = meta.block_number.as_u64();
            records.push(PoolRecord {
                pool: event.pool,
                token0: event.token_0,
                token1: event.token_1,
                fee: event.fee,
                created_block: block,
                hub_usd_liquidity: None,
                hub_symbol: None,
            });
        }
        sleep(Duration::from_millis(INTER_CHUNK_DELAY_MS)).await;
    }
    Ok(records)
}

#[tokio::main]
async fn main() -> Result<()> {
    let (chain_name, venue_name, from_block, to_block, chunk_size, query_timeout) = parse_args()?;
    let ops = load_ops_inputs("ops/inputs.yaml").context("load ops inputs")?;
    let chain_inputs = ops
        .chain_inputs(&chain_name)
        .ok_or_else(|| anyhow!("chain `{chain_name}` not found in ops inputs"))?;
    let venue = ops
        .venue_inputs(&chain_name, &venue_name)
        .ok_or_else(|| anyhow!("venue `{venue_name}` not found for chain `{chain_name}`"))?;
    let kind = venue
        .kind
        .as_ref()
        .ok_or_else(|| anyhow!("venue `{venue_name}` missing kind"))?;

    let rpc_url = chain_inputs
        .rpc_http_urls
        .first()
        .ok_or_else(|| anyhow!("chain `{chain_name}` missing rpc_http_urls"))?;
    let provider =
        Arc::new(Provider::<Http>::try_from(rpc_url.as_str()).context("create http provider")?);

    let records = match kind {
        VenueKind::Univ2Like => {
            let factory = venue
                .factory
                .as_ref()
                .ok_or_else(|| anyhow!("venue `{venue_name}` missing factory"))?;
            let fee_bps = venue
                .fee_bps
                .ok_or_else(|| anyhow!("venue `{venue_name}` missing fee_bps"))?;
            ingest_univ2(
                provider,
                Address::from_str(factory).context("parse univ2 factory")?,
                fee_bps,
                from_block,
                to_block,
                chunk_size,
                query_timeout,
            )
            .await?
        }
        VenueKind::Univ3Like => {
            let factory = venue
                .factory
                .as_ref()
                .ok_or_else(|| anyhow!("venue `{venue_name}` missing factory"))?;
            ingest_univ3(
                provider,
                Address::from_str(factory).context("parse univ3 factory")?,
                from_block,
                to_block,
                chunk_size,
                query_timeout,
            )
            .await?
        }
        _ => {
            return Err(anyhow!(
                "venue `{venue_name}` kind {:?} not supported for ingestion",
                kind
            ));
        }
    };

    let path = pool_data_path(&chain_name, &venue_name);
    let existing = pool_store::load_pool_records(&path).unwrap_or_default();
    let merged = merge_pool_records(existing, records);
    write_pool_records(&path, &merged)?;

    println!("Wrote {} pools to {}", merged.len(), path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::block_windows;

    #[test]
    fn block_windows_partition_range() {
        let windows: Vec<(u64, u64)> = block_windows(100, 350, 100).collect();
        assert_eq!(windows, vec![(100, 199), (200, 299), (300, 350)]);
    }

    #[test]
    fn block_windows_single_block() {
        let windows: Vec<(u64, u64)> = block_windows(42, 42, 10_000).collect();
        assert_eq!(windows, vec![(42, 42)]);
    }
}
