//! Task 0.5: every legacy `ARBOT_*` variable is accounted for exactly once.
//!
//! Enforced in BOTH directions. A variable present in the source but missing
//! from the manifest fails, and a manifest entry no longer present in the source
//! fails too -- otherwise the table silently rots into fiction as modules
//! migrate, which is this repository's most-repeated failure mode (four
//! instances of something written in one phase and never wired in the next).

use apex_config::env_migration::{count_for, lookup, Destination, LEGACY_ENV_VARS};
use std::collections::BTreeSet;

/// Scan **every crate** for `ARBOT_*` identifiers.
///
/// Originally this walked `arb-exec-legacy/src` alone, which was right while
/// that was the only crate with any code in it. Phase 2 moved the venue
/// quoters to `apex-venues` and the scan promptly declared
/// `ARBOT_SLIPSTREAM_LIVE_QUOTE` retired — the variable had not moved, the
/// scanner had stopped looking. A manifest that covers the repository has to
/// be checked against the repository.
///
/// Two files are excluded, and the exclusion is load-bearing: the manifest
/// itself names all 84 variables, so scanning it would make
/// `the_manifest_has_not_rotted` find every entry and never fire again.
fn vars_in_source() -> BTreeSet<String> {
    let root = format!("{}/..", env!("CARGO_MANIFEST_DIR"));
    let mut found = BTreeSet::new();
    walk(std::path::Path::new(&root), &mut |text| {
        collect_env_names(text, &mut found);
        collect_prefixed(text, &mut found);
    });
    found
}

/// Any `ARBOT_`-prefixed token, wherever it appears.
///
/// Kept alongside the call-form scan because neither is sufficient alone. The
/// call forms cannot see a variable read through a local helper -- `base_fast`
/// has `f("ARBOT_COST_GAS_BPS", 5.0)` and `main` passes names to a wrapper
/// across several lines -- and enumerating every helper is a losing game. The
/// prefix scan cannot see the 132 variables outside the prefix. The union has
/// the blind spot of neither, at the cost of a scan that is slightly eager
/// about `ARBOT_` in prose, which is the safe direction: an extra entry in the
/// table is a line of documentation, a missing one is a variable nobody
/// retires.
fn collect_prefixed(text: &str, out: &mut BTreeSet<String>) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while let Some(rel) = text[i..].find("ARBOT_") {
        let start = i + rel;
        let mut end = start + "ARBOT_".len();
        while end < bytes.len()
            && (bytes[end].is_ascii_uppercase() || bytes[end].is_ascii_digit() || bytes[end] == b'_')
        {
            end += 1;
        }
        if end > start + "ARBOT_".len() {
            out.insert(text[start..end].to_string());
        }
        i = end.max(start + 1);
    }
}

/// Files whose contents would make the scan answer its own question.
const SELF_REFERENTIAL: [&str; 2] = ["env_migration.rs", "env_coverage.rs"];

/// Variables that belong to the build or the operating system, not to this
/// engine's configuration. Listing them in a MIGRATION manifest would be
/// noise: nothing here is going to be ported to a crate.
const NOT_OUR_CONFIGURATION: [&str; 7] = [
    "HOME",
    "PATH",
    "CARGO_MANIFEST_DIR",
    "CARGO_PKG_VERSION",
    "OUT_DIR",
    "RUST_BACKTRACE",
    "TMPDIR",
];

/// The call forms that read or write a process environment variable.
///
/// Scanning for the literal `ARBOT_` was the original approach and it made the
/// manifest read as complete while 132 variables outside that prefix went
/// unaccounted for. Matching the CALL instead of the name covers every
/// variable and still cannot be fooled by prose: a doc comment mentioning
/// `PRIVATE_KEY` has no `env::var(` in front of it.
const READERS: &[&str] = &[
    "env::var(",
    "env::var_os(",
    "env::set_var(",
    "env::remove_var(",
    "env_flag(",
    "env_parse_opt(",
    // `env_parse_opt::<T>("NAME")` -- the turbofish sits between the name and
    // the paren, so the marker has to stop before it.
    "env_parse_opt::",
];

fn collect_env_names(text: &str, out: &mut BTreeSet<String>) {
    for marker in READERS {
        let mut from = 0;
        while let Some(rel) = text[from..].find(marker) {
            let after = from + rel + marker.len();
            from = after;
            // Skip to the opening quote of the first argument, tolerating a
            // turbofish and a leading `&`.
            let Some(q) = text[after..].find('"') else { break };
            let between = &text[after..after + q];
            if between.len() > 40 || between.contains(';') || between.contains('\n') {
                continue;
            }
            let start = after + q + 1;
            let Some(end_rel) = text[start..].find('"') else { break };
            let name = &text[start..start + end_rel];
            // At least one letter: an all-digit literal such as a wei amount
            // is an argument, not a variable name.
            if name.len() >= 3
                && name.chars().any(|c| c.is_ascii_uppercase())
                && name
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                && !NOT_OUR_CONFIGURATION.contains(&name)
            {
                out.insert(name.to_string());
            }
        }
    }
}

fn walk(dir: &std::path::Path, f: &mut impl FnMut(&str)) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            // `target/` holds build output including vendored sources; walking
            // it is slow and finds variables no one wrote.
            if p.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            walk(&p, f);
        } else if p.extension().is_some_and(|x| x == "rs")
            && !p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| SELF_REFERENTIAL.contains(&n))
        {
            if let Ok(text) = std::fs::read_to_string(&p) {
                f(&text);
            }
        }
    }
}

#[test]
fn every_legacy_env_var_is_accounted_for() {
    let found = vars_in_source();
    assert!(!found.is_empty(), "scanner found nothing -- it is broken, not the source");

    let unaccounted: Vec<&String> = found.iter().filter(|v| lookup(v).is_none()).collect();
    assert!(
        unaccounted.is_empty(),
        "these ARBOT_* variables have no migration destination: {unaccounted:#?}\n\
         Add them to apex_config::env_migration::LEGACY_ENV_VARS with a destination \
         and a reason. Blueprint §2.4 forbids late configuration lookup; a variable \
         with no owning crate is one that never gets retired."
    );
}

#[test]
fn the_manifest_has_not_rotted() {
    // The other direction. An entry for a variable that no longer exists means
    // the table is describing a codebase that has moved on.
    let found = vars_in_source();
    let stale: Vec<&str> = LEGACY_ENV_VARS
        .iter()
        .map(|e| e.name)
        .filter(|n| !found.contains(*n))
        .collect();
    assert!(
        stale.is_empty(),
        "manifest entries with no remaining call site: {stale:#?}\n\
         The variable is gone -- delete the entry, or move it to the phase notes \
         if it was genuinely retired."
    );
}

#[test]
fn no_variable_is_listed_twice() {
    let mut seen = BTreeSet::new();
    for e in LEGACY_ENV_VARS {
        assert!(seen.insert(e.name), "{} appears twice in the manifest", e.name);
    }
    assert_eq!(seen.len(), LEGACY_ENV_VARS.len());
}

#[test]
fn every_entry_carries_a_reason() {
    for e in LEGACY_ENV_VARS {
        assert!(!e.note.trim().is_empty(), "{} has no note", e.name);
        assert!(
            e.note.len() > 12,
            "{}'s note is too short to be a reason: {:?}",
            e.name,
            e.note
        );
    }
}

#[test]
fn the_dispatch_path_destinations_are_populated() {
    // A sanity check on the classification itself: if everything landed in one
    // bucket the manifest would be shaped like a to-do list rather than a plan.
    for d in [
        Destination::State,
        Destination::Math,
        Destination::Search,
        Destination::Econ,
        Destination::Sim,
        Destination::Capture,
        Destination::Chain,
    ] {
        assert!(count_for(d) > 0, "no variable is destined for {d:?}");
    }
}

#[test]
fn test_only_variables_are_not_read_on_the_trading_path() {
    // A TestOnly classification is a claim, so check it rather than trust it.
    //
    // main.rs carries its own `#[cfg(test)] mod tests` -- 16,659 lines with the
    // tests at the bottom -- so a naive `contains` over the whole file reports
    // every fixture variable as production. Truncate at the test module first.
    // (That crude version did fire on ARBOT_FORK_RPC_URL, which turned out to
    // sit in a #[tokio::test]; the classification was right and the detector
    // was wrong.)
    let main = std::fs::read_to_string(format!(
        "{}/../arb-exec-legacy/src/main.rs",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("read main.rs");

    let production = match main.find("#[cfg(test)]") {
        Some(i) => &main[..i],
        None => &main[..],
    };
    assert!(
        production.len() < main.len(),
        "main.rs has no #[cfg(test)] module; this test's assumption no longer holds"
    );

    let leaked: Vec<&str> = LEGACY_ENV_VARS
        .iter()
        .filter(|e| e.destination == Destination::TestOnly)
        .map(|e| e.name)
        .filter(|n| production.contains(*n))
        .collect();

    assert!(
        leaked.is_empty(),
        "classified TestOnly but read by production code in main.rs: {leaked:#?}"
    );
}
