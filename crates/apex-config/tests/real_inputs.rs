//! Task 0.4a: `apex-config` against the real `ops/inputs.yaml`.
//!
//! A schema that only parses its own fixtures proves nothing. These tests run
//! the fresh loader over the file the bot actually ships with, and pin what it
//! finds there -- including the parts it refuses.

use apex_config::{ApexConfig, ConfigError, Env};

fn inputs_path() -> String {
    // The workspace root, not the crate dir: cargo runs tests with the PACKAGE
    // directory as cwd.
    format!("{}/../../ops/inputs.yaml", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn the_production_inputs_file_is_structurally_parseable() {
    // `permissive` so unset `${VAR}` placeholders resolve rather than abort --
    // this test is about the SHAPE being understood. The next test is about what
    // fail-closed does with those placeholders, which is the more interesting
    // half.
    let text = std::fs::read_to_string(inputs_path()).expect("read ops/inputs.yaml");
    let env = Env::permissive();
    match ApexConfig::from_yaml_str(&text, &env) {
        Ok(cfg) => {
            assert!(!cfg.chains().is_empty());
            assert!(cfg.chain("base").is_some(), "base must be configured");
        }
        Err(ConfigError::UnresolvedPlaceholder(key, _)) => {
            // Acceptable: permissive still cannot invent a value for an unset
            // var. Recorded rather than papered over.
            eprintln!("unresolved placeholder in production inputs: {key}");
        }
        Err(e) => panic!("production ops/inputs.yaml does not fit the schema: {e}"),
    }
}

#[test]
fn production_inputs_are_not_loadable_fail_closed_today() {
    // This is a FINDING, pinned as a test so it cannot regress silently and so
    // it fails loudly the day it is fixed.
    //
    // ops/inputs.yaml configures 7 chains but only base has real endpoints.
    // Several carry `${VAR}` placeholders for vars that are not set, so a
    // strictly fail-closed load refuses the file. That is the loader behaving
    // correctly -- audit item P3-2 asked for exactly this guard -- but it means
    // the config cannot go fail-closed until the dead chains are trimmed or
    // moved out of the default set.
    let text = std::fs::read_to_string(inputs_path()).expect("read ops/inputs.yaml");
    let result = ApexConfig::from_yaml_str(&text, &Env::secrets_only());

    match result {
        Err(ConfigError::UnresolvedPlaceholder(key, loc)) => {
            eprintln!("expected today: unresolved `{key}` at {loc}");
        }
        Err(ConfigError::EmptyRequiredField { chain, field }) => {
            eprintln!("expected today: chain `{chain}` has empty `{field}`");
        }
        Err(ConfigError::UnpolicedChain(chain)) => {
            eprintln!("expected today: chain `{chain}` has no risk policy");
        }
        Err(e) => panic!("refused for an unexpected reason, investigate: {e}"),
        Ok(_) => panic!(
            "ops/inputs.yaml now loads fail-closed. That is good news -- delete \
             this test, flip the loader's default to secrets_only, and close the \
             P3-2 audit item."
        ),
    }
}

#[test]
fn every_configured_chain_has_a_risk_policy() {
    // risk_policy.rs was written because risk.per_chain was parsed and never
    // consulted. The structural version of that bug is a chain with no policy
    // at all.
    // The TYPED schema, not serde_yaml::Value: the untyped value cannot hold
    // ethereum's max_fee_per_gas_cap, which exceeds u64::MAX.
    let text = std::fs::read_to_string(inputs_path()).expect("read");
    let raw: apex_config::RawConfig =
        serde_yaml::from_str(&apex_config::testing::interpolate_permissively(&text))
            .expect("production inputs must fit the typed schema");

    let chains: Vec<String> = raw.chains.iter().map(|c| c.chain_name.clone()).collect();
    let policed: Vec<String> =
        raw.risk.per_chain.iter().map(|c| c.chain_name.clone()).collect();

    let unpoliced: Vec<&String> = chains.iter().filter(|c| !policed.contains(c)).collect();
    assert!(
        unpoliced.is_empty(),
        "chains configured with no risk.per_chain entry: {unpoliced:?}"
    );
}

#[test]
fn the_linea_liquidation_config_is_placeholder_addresses() {
    // Audit item P3-2 recorded "linea liquidation block is placeholder
    // 0xCfDA...e90 repeated". Pinned here because a repeated address across
    // roles that must differ (pool / data_provider / price_oracle / debt token /
    // collateral token) is indistinguishable from real config at the point of
    // use -- the same failure class as the fabricated venue addresses.
    let text = std::fs::read_to_string(inputs_path()).expect("read");
    let raw: apex_config::RawConfig =
        serde_yaml::from_str(&apex_config::testing::interpolate_permissively(&text))
            .expect("typed schema");

    let markets = raw.features["liquidation_markets"].as_sequence();
    let Some(markets) = markets else { return };
    let Some(linea) = markets.iter().find(|m| m["chain_name"].as_str() == Some("linea")) else {
        return;
    };

    let aave = &linea["aave_v3"];
    let pool = aave["pool"].as_str().unwrap_or_default();
    let oracle = aave["price_oracle"].as_str().unwrap_or_default();
    assert_eq!(
        pool, oracle,
        "linea's placeholders have been replaced with real addresses -- good; \
         delete this test and re-enable linea"
    );
}

/// Ethereum's gas-price cap is the one outlier, and it is effectively infinite.
///
/// Measured across all seven chains: arbitrum, optimism, linea, abstract and ink
/// declare 200 gwei; base declares 5,000 gwei (high for a chain whose gas runs
/// ~0.01 gwei, but finite and plausible as a catastrophe stop). Ethereum
/// declares 21,000,000,000 gwei -- 21 ETH per gas unit, 105 million times base's
/// cap, and the only value in the file exceeding `u64::MAX`.
///
/// A 300k-gas transaction would have to cost 6.3M ETH to trip it, so the control
/// is enforced but unreachable. The shape (21e18 where 21e9 would be 21 gwei)
/// suggests nine surplus zeros rather than a deliberate choice.
///
/// Pinned so it fails the day it is corrected, at which point delete this test.
#[test]
fn ethereum_gas_cap_is_effectively_infinite() {
    let text = std::fs::read_to_string(inputs_path()).expect("read");
    let raw: apex_config::RawConfig =
        serde_yaml::from_str(&apex_config::testing::interpolate_permissively(&text))
            .expect("typed schema");

    let caps: Vec<(String, u128)> = raw
        .risk
        .per_chain
        .iter()
        .filter_map(|c| {
            c.max_fee_per_gas_cap
                .as_ref()
                .and_then(|v| v.parse::<u128>().ok())
                .map(|w| (c.chain_name.clone(), w))
        })
        .collect();

    assert!(!caps.is_empty(), "no chain declares max_fee_per_gas_cap");

    // Every chain declares one -- the control is configured, not absent.
    assert_eq!(
        caps.len(),
        raw.risk.per_chain.len(),
        "a chain stopped declaring max_fee_per_gas_cap"
    );

    const ONE_GWEI: u128 = 1_000_000_000;
    for (name, wei) in &caps {
        if name == "ethereum" {
            assert!(
                *wei > 1_000_000 * ONE_GWEI,
                "ethereum's cap is now below 1e6 gwei, i.e. plausibly real -- \
                 delete this test"
            );
        } else {
            assert!(
                *wei <= 10_000 * ONE_GWEI,
                "{name} declares {wei} wei ({} gwei), which is not a bound",
                wei / ONE_GWEI
            );
        }
    }

    // The u64 boundary is why serde_yaml's untyped Value cannot hold this file,
    // and why both this schema and the legacy one carry the field as a String.
    let eth = caps.iter().find(|(n, _)| n == "ethereum").map(|(_, w)| *w).unwrap_or(0);
    assert!(eth > u128::from(u64::MAX), "ethereum's cap no longer exceeds u64::MAX");
}
