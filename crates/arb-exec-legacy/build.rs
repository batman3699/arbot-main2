//! Exports `WORKSPACE_ROOT` so compile-time and runtime path lookups survive the
//! crate's position in the tree.
//!
//! Before the workspace split this crate WAS the workspace, so `CARGO_MANIFEST_DIR`
//! and the repository root were the same directory and four sites relied on that
//! silently. After the split they differ by two levels, and each of those sites
//! breaks differently: `chain.rs`'s `include_str!` is a hard compile error, while
//! `main.rs`'s `.env` fallback fails SILENTLY -- masked when launched from the repo
//! root, because dotenvy searches the working directory first, and starting the bot
//! with no configuration from anywhere else.
//!
//! Resolved by walking up for the manifest that declares `[workspace]` rather than
//! by counting `../` levels, so moving a crate deeper does not reintroduce this.

// scripts/ci/no_runtime_panics.sh forbids unwrap/expect, and correctly: a panic
// in a running trading bot is an outage. A BUILD script is the opposite case --
// it runs at compile time, and aborting the build is the loudest, safest way to
// report that the workspace root cannot be resolved. The alternative is emitting
// a wrong WORKSPACE_ROOT, which is precisely the silent failure this file exists
// to prevent (main.rs's .env fallback resolving to the crate directory).
#![allow(clippy::expect_used)]

use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let manifest = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is always set by cargo"),
    );

    let root = find_workspace_root(&manifest).unwrap_or_else(|| {
        panic!(
            "no ancestor of {} contains a Cargo.toml declaring [workspace]; \
             WORKSPACE_ROOT cannot be resolved and path lookups would silently \
             fall back to the crate directory",
            manifest.display()
        )
    });

    println!("cargo:rustc-env=WORKSPACE_ROOT={}", root.display());
}

fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        let manifest = dir.join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        // Only the workspace root declares [workspace]; member manifests do not.
        match std::fs::read_to_string(&manifest) {
            Ok(text) if text.lines().any(|l| l.trim_start().starts_with("[workspace]")) => {
                return Some(dir.to_path_buf());
            }
            _ => {}
        }
    }
    None
}
