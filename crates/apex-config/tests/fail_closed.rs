//! `ApexConfig` must refuse to build rather than hand back a half-resolved
//! configuration. Blueprint §2.4 names "late configuration lookup" as an
//! engineering failure that loses opportunities; the corollary is that anything
//! missing has to be caught at boot, loudly, with the field named.
//!
//! Every case here is drawn from a real defect in this repository's own config,
//! not invented.

use apex_config::{ApexConfig, ConfigError, Env};

fn load(yaml: &str) -> Result<ApexConfig, ConfigError> {
    ApexConfig::from_yaml_str(yaml, &Env::secrets_only())
}

const MINIMAL: &str = r#"
chains:
  - chain_name: base
    chain_id: 8453
    env_prefix: BASE
    rpc_http_urls: ["https://base.example/rpc"]
    rpc_ws_urls: ["wss://base.example/ws"]
    gas_model: op_stack
universe:
  min_pool_liquidity_usd: 100000
  max_hops: 4
risk:
  per_chain:
    - chain_name: base
      min_net_profit_usd: 5
      max_gas_units_per_tx: 600000
      max_slippage_bps: 50
      must_simulate_before_send: true
features: {}
"#;

#[test]
fn the_minimal_fixture_actually_loads() {
    // Guards the negative tests below: if this ever fails they would all pass
    // for the wrong reason.
    load(MINIMAL).expect("the minimal fixture must be valid");
}

#[test]
fn a_missing_required_section_names_itself() {
    for section in ["chains", "universe", "risk"] {
        let stripped = strip_section(MINIMAL, section);
        let err = load(&stripped).unwrap_err();
        assert!(
            err.to_string().contains(section),
            "removing `{section}` must produce an error naming it, got: {err}"
        );
    }
}

#[test]
fn a_chain_with_no_rpc_is_refused() {
    // Audit item P3-2: ops/inputs.yaml configures 7 chains but only base has
    // real endpoints. `ethereum` silently pointed at http://127.0.0.1:8565.
    // Launching a chain with nowhere to talk to is a misconfiguration, not a
    // default.
    let yaml = MINIMAL.replace(r#"rpc_http_urls: ["https://base.example/rpc"]"#, "rpc_http_urls: []");
    let err = load(&yaml).unwrap_err();
    assert!(err.to_string().contains("base"), "must name the chain: {err}");
    assert!(err.to_string().contains("rpc"), "must name the field: {err}");
}

#[test]
fn an_unresolved_env_placeholder_is_refused_not_passed_through() {
    // ops/inputs.yaml uses ${ARB_EXECUTOR_ADDRESS} interpolation. An unset var
    // must not survive into the resolved config as the literal string -- that is
    // how a placeholder reaches an address field and gets treated as real.
    let yaml = MINIMAL.replace(
        r#"rpc_http_urls: ["https://base.example/rpc"]"#,
        r#"rpc_http_urls: ["${DEFINITELY_UNSET_RPC_URL}"]"#,
    );
    let err = load(&yaml).unwrap_err();
    assert!(
        err.to_string().contains("DEFINITELY_UNSET_RPC_URL"),
        "must name the unresolved variable: {err}"
    );
}

#[test]
fn duplicate_chains_are_refused() {
    // Two entries in ONE list, not a second `chains:` key -- that would be a
    // duplicate YAML key and serde would reject it before validate() ran, so the
    // test would pass without exercising the check it names.
    let yaml = MINIMAL.replace(
        "  - chain_name: base\n    chain_id: 8453\n",
        "  - chain_name: base\n    chain_id: 8453\n    env_prefix: BASE\n\
         \x20   rpc_http_urls: [\"https://a.example/rpc\"]\n    rpc_ws_urls: []\n\
         \x20   gas_model: op_stack\n  - chain_name: base\n    chain_id: 9999\n",
    );
    let err = load(&yaml).unwrap_err();
    let msg = err.to_string();
    assert!(msg.to_lowercase().contains("duplicate"), "got: {msg}");
    assert!(msg.contains("chain_name"), "must say WHICH field duplicated: {msg}");
}

#[test]
fn duplicate_chain_ids_are_refused_too() {
    // Distinct names, same id. §29.6 makes wrong_chain_submission a
    // zero-tolerance counter; two names resolving to one chain id is how that
    // happens by accident.
    let yaml = MINIMAL.replace(
        "  - chain_name: base\n    chain_id: 8453\n",
        "  - chain_name: base\n    chain_id: 8453\n    env_prefix: BASE\n\
         \x20   rpc_http_urls: [\"https://a.example/rpc\"]\n    rpc_ws_urls: []\n\
         \x20   gas_model: op_stack\n  - chain_name: base_alias\n    chain_id: 8453\n",
    ).replace("    - chain_name: base\n", "    - chain_name: base\n    - chain_name: base_alias\n");
    let err = load(&yaml).unwrap_err();
    assert!(err.to_string().contains("chain_id"), "got: {err}");
}

#[test]
fn a_chain_missing_its_risk_policy_is_refused() {
    // risk_policy.rs exists because risk.per_chain was once parsed and never
    // consulted -- "safety theater". A chain that trades with no declared policy
    // is that failure in a different shape.
    let yaml = MINIMAL.replace("    - chain_name: base\n", "    - chain_name: ethereum\n");
    let err = load(&yaml).unwrap_err();
    assert!(err.to_string().contains("base"), "must name the unpoliced chain: {err}");
}

#[test]
fn boot_config_reads_no_environment_beyond_secrets() {
    // §2.4: no late configuration lookup. The loader records every key it
    // consulted so this is checkable rather than asserted.
    let cfg = load(MINIMAL).unwrap();
    let non_secret: Vec<_> = cfg
        .consulted_env_keys()
        .iter()
        .filter(|k| !k.starts_with("APEX_SECRET_"))
        .cloned()
        .collect();
    assert!(non_secret.is_empty(), "config read non-secret env: {non_secret:?}");
}

#[test]
fn config_version_is_content_addressed() {
    let a = load(MINIMAL).unwrap();
    let b = load(MINIMAL).unwrap();
    assert_eq!(a.config_version(), b.config_version(), "same input, same version");

    let changed = MINIMAL.replace("max_hops: 4", "max_hops: 5");
    let c = load(&changed).unwrap();
    assert_ne!(a.config_version(), c.config_version(), "changed input must change version");
}

// --------------------------------------------------------------------- helpers

fn strip_section(yaml: &str, section: &str) -> String {
    let mut out = String::new();
    let mut skipping = false;
    for line in yaml.lines() {
        if line.starts_with(&format!("{section}:")) {
            skipping = true;
            continue;
        }
        if skipping && !line.is_empty() && !line.starts_with(' ') && !line.starts_with('-') {
            skipping = false;
        }
        if !skipping {
            out.push('\n');
            out.push_str(line);
        }
    }
    out
}

