use std::{fs, path::Path};

use crate::bridge::BridgeRouteCfg;
use crate::chain::parse_token_list_contents;
use crate::ops_inputs::parse_ops_inputs;
use crate::registry::Registry;
use crate::venues::{parse_pool_configs, BalPoolCfg, CurvePoolCfg, UniV2PoolCfg};

fn load(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|err| {
        panic!("failed to read {}: {err}", path.display());
    })
}

/// Resolve a repo-relative path against the WORKSPACE ROOT so these tests are
/// immune to the process working directory being changed by a parallel test
/// (e.g. registry.rs's DirGuard calls set_current_dir).
///
/// Was `CARGO_MANIFEST_DIR`, which stopped being the repo root at the workspace
/// split. `config/` and `ops/` live at the root, not beside this crate.
fn manifest_path(rel: &str) -> std::path::PathBuf {
    crate::util::workspace_path(rel)
}

#[test]
fn token_lists_are_valid() {
    for entry in fs::read_dir(manifest_path("config")).expect("config dir") {
        let path = entry.expect("dir entry").path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();

        if !(name.starts_with("tokens.") && name.ends_with(".json5")) {
            continue;
        }

        let raw = load(&path);
        let tokens = parse_token_list_contents(&raw, &name)
            .unwrap_or_else(|err| panic!("failed to parse {name}: {err}"));
        assert!(
            !tokens.is_empty(),
            "token list {name} should not be empty to ensure routers have routing anchors"
        );
    }
}

#[test]
fn pool_configs_are_valid() {
    for entry in fs::read_dir(manifest_path("config")).expect("config dir") {
        let path = entry.expect("dir entry").path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();

        if !name.ends_with(".json5") {
            continue;
        }

        let raw = load(&path);
        if name.starts_with("univ2.") {
            parse_pool_configs::<UniV2PoolCfg>(&raw, &name)
                .unwrap_or_else(|err| panic!("failed to parse {name}: {err}"));
        } else if name.starts_with("curve.") {
            parse_pool_configs::<CurvePoolCfg>(&raw, &name)
                .unwrap_or_else(|err| panic!("failed to parse {name}: {err}"));
        } else if name.starts_with("balancer.") {
            parse_pool_configs::<BalPoolCfg>(&raw, &name)
                .unwrap_or_else(|err| panic!("failed to parse {name}: {err}"));
        }
    }
}

#[test]
fn registry_example_is_valid_json() {
    let raw = load(&manifest_path("config/registry.example.json"));
    let registry: Registry = serde_json::from_str(&raw).expect("parse registry");
    assert!(
        !registry.chains.is_empty(),
        "registry example should enumerate at least one chain"
    );
}

#[test]
fn bridge_routes_example_is_valid() {
    let raw = load(&manifest_path("config/bridge_routes.example.json5"));
    let routes: Vec<BridgeRouteCfg> =
        json5::from_str(&raw).expect("parse bridge routes example config");
    assert!(
        !routes.is_empty(),
        "bridge routes example should include at least one entry"
    );
}

#[test]
fn ops_inputs_example_is_valid_yaml() {
    let raw = load(&manifest_path("ops/inputs.example.yaml"));
    let cfg = parse_ops_inputs(&raw).expect("parse ops inputs example");
    assert!(
        !cfg.chains.is_empty(),
        "ops inputs example should enumerate at least one chain"
    );
}
