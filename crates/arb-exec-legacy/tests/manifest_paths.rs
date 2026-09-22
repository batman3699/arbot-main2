//! The workspace split moved this crate two levels below the repo root. Four
//! sites resolved repo-relative paths against `CARGO_MANIFEST_DIR` and silently
//! assumed those were the same directory. These tests pin the fix.
//!
//! Three of the four failed loudly (a missing `include_str!` is a compile error,
//! and a missing config file is an obvious panic). The fourth — the `.env`
//! fallback in `load_dotenv` — failed silently, and is the reason this file
//! exists rather than relying on the compiler.

use std::path::Path;

/// `WORKSPACE_ROOT` must be the repo root, not the crate directory.
#[test]
fn workspace_root_is_the_repo_root_not_the_crate_dir() {
    let root = arb_exec::util::workspace_root();

    assert!(
        root.join("Cargo.toml").is_file(),
        "{} has no Cargo.toml",
        root.display()
    );
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("read root manifest");
    assert!(
        manifest.contains("[workspace]"),
        "WORKSPACE_ROOT points at {}, whose Cargo.toml does not declare [workspace] \
         -- build.rs resolved the wrong directory",
        root.display()
    );

    assert_ne!(
        root,
        Path::new(env!("CARGO_MANIFEST_DIR")),
        "WORKSPACE_ROOT collapsed onto CARGO_MANIFEST_DIR; the split is the whole \
         point of this indirection"
    );
}

/// Every repo-root directory these four sites reach for must actually resolve.
#[test]
fn repo_relative_paths_resolve_from_the_workspace_root() {
    for rel in [
        "scripts/fork/run_integration_dry_run.sh", // chain.rs include_str!
        "ops/inputs.yaml",                         // config_validation.rs
        "config/registry.json",                    // config_validation.rs
        "contracts",                               // foundry sources
    ] {
        let p = arb_exec::util::workspace_path(rel);
        assert!(p.exists(), "{rel} does not resolve: {}", p.display());
    }
}

/// The silent one. `load_dotenv` falls back to this path when dotenvy finds
/// nothing by walking up from the working directory. Pointed at the crate dir it
/// resolves to `crates/arb-exec-legacy/.env`, which does not exist -- and because
/// dotenvy searches the cwd first, running from the repo root hides it entirely.
/// The bot would then start with no configuration, silently, but only when
/// launched from somewhere else.
#[test]
fn dotenv_fallback_resolves_to_the_workspace_root() {
    let fallback = arb_exec::util::dotenv_fallback_path();

    assert_eq!(fallback, arb_exec::util::workspace_root().join(".env"));
    assert!(
        !fallback.starts_with(Path::new(env!("CARGO_MANIFEST_DIR")).join("src")),
        "fallback must not resolve inside the crate: {}",
        fallback.display()
    );
}

/// `data/` is gitignored and lives at the repo root. A wrong base here does not
/// fail -- `shipped_base_univ3_inventory_ranks_in_full` takes an early return on
/// `records.is_empty()` and reports success while checking nothing.
#[test]
fn shipped_inventory_path_resolves_when_the_inventory_is_present() {
    let p = arb_exec::util::workspace_path("data/base/uniswap_v3/pools.jsonl");

    if !arb_exec::util::workspace_path("data").exists() {
        eprintln!("data/ absent in this checkout; nothing to assert");
        return;
    }
    assert!(
        p.exists(),
        "data/ exists but {} does not -- the inventory test would silently no-op",
        p.display()
    );
}
