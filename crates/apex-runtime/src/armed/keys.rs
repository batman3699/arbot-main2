//! The armed senders' keys (R36): one secret per line, in a file only its
//! owner can read. Made on the host by `apex keys new`, separate from the
//! reactive trader's key; only addresses ever leave the file, and no error
//! repeats a line of it.

use crate::sign::LaneKey;
use alloy_primitives::Address;
use apex_config::Secret;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// `count` new keys, written to a new file at `path` with mode 0400: an
/// existing file is never touched. Their addresses, in the file's order.
pub fn generate(count: usize, path: &Path) -> Result<Vec<Address>, String> {
    let shown = path.display();
    if count == 0 {
        return Err("no keys asked for".into());
    }
    let mut text = Secret::new(String::new());
    let mut addresses = Vec::with_capacity(count);
    for _ in 0..count {
        let wallet = ethers_signers::LocalWallet::new(&mut ethers_core::rand::thread_rng());
        let hex = Secret::new(format!("0x{}", alloy_primitives::hex::encode(wallet.signer().to_bytes())));
        let key = LaneKey::from_hex(&hex).map_err(|_| "a key just made did not load".to_string())?;
        addresses.push(key.address());
        text = Secret::new(format!("{}{}\n", text.expose(), hex.expose()));
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o400)
        .open(path)
        .map_err(|e| format!("{shown}: {e}"))?;
    file.write_all(text.expose().as_bytes()).and_then(|()| file.sync_all()).map_err(|e| format!("{shown}: {e}"))?;
    Ok(addresses)
}

/// The keys at `path`, refused when the file is open to its group or others.
pub fn load(path: &Path) -> Result<Vec<LaneKey>, String> {
    let shown = path.display();
    let mode = std::fs::metadata(path).map_err(|e| format!("{shown}: {e}"))?.permissions().mode();
    if mode & 0o077 != 0 {
        return Err(format!("{shown} is readable or writable by others (mode {:o}): it must be 0400", mode & 0o777));
    }
    let text = Secret::new(std::fs::read_to_string(path).map_err(|e| format!("{shown}: {e}"))?);
    let mut keys = Vec::new();
    for (i, line) in text.expose().lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let key = LaneKey::from_hex(&Secret::new(line.to_string())).map_err(|_| format!("{shown}: line {} is not a key", i + 1))?;
        keys.push(key);
    }
    if keys.is_empty() {
        return Err(format!("{shown} holds no key"));
    }
    Ok(keys)
}
