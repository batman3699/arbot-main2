use anyhow::{ensure, Context, Result};
use ethers::types::Address;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    str::FromStr,
};
use tracing::{info, warn};

use crate::util::expand_env_vars;
use crate::venues::{BalPoolCfg, CurvePoolCfg, UniV2PoolCfg};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Registry {
    pub version: String,
    #[serde(default)]
    pub last_updated: Option<String>,
    #[serde(default)]
    pub chains: HashMap<String, RegistryChain>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct RegistryChain {
    #[serde(default)]
    pub env_prefix: Option<String>,
    #[serde(default)]
    pub chain_id: u64,
    #[serde(default)]
    pub rpc_http: Vec<String>,
    #[serde(default)]
    pub rpc_ws: Vec<String>,
    #[serde(default)]
    pub tokens: Vec<String>,
    #[serde(default)]
    pub univ3_quoter: Option<String>,
    #[serde(default)]
    pub univ3_factory: Option<String>,
    #[serde(default)]
    pub univ3_router: Option<String>,
    #[serde(default)]
    pub bal_vault: Option<String>,
    #[serde(default)]
    pub aave_pool: Option<String>,
    #[serde(default)]
    pub bal_flashloan_tokens: Vec<String>,
    #[serde(default)]
    pub pools: RegistryPools,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct RegistryPools {
    #[serde(default)]
    pub balancer: Vec<BalPoolCfg>,
    #[serde(default)]
    pub curve: Vec<CurvePoolCfg>,
    #[serde(default)]
    pub univ2: Vec<UniV2PoolCfg>,
}

#[derive(Clone, Debug)]
enum RegistrySource {
    File(PathBuf),
    Url(String),
    Ipfs { cid: String, gateway: String },
    Ipns { name: String, gateway: String },
}

fn cache_path() -> PathBuf {
    std::env::var("REGISTRY_CACHE_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("cache/registry.json"))
}

#[derive(Clone)]
struct RegistryBlob {
    registry: Registry,
    raw: Vec<u8>,
    hash: String,
}

fn hash_bytes(raw: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"arbot-registry-v1|");
    hasher.update(raw);
    format!("{:x}", hasher.finalize())
}

fn parse_version(raw: &str) -> Option<Version> {
    Version::parse(raw).ok()
}

fn should_replace_cache(candidate: &RegistryBlob, current: Option<&RegistryBlob>) -> bool {
    if let Some(current) = current {
        if let (Some(new), Some(old)) = (
            parse_version(&candidate.registry.version),
            parse_version(&current.registry.version),
        ) {
            if new != old {
                return new > old;
            }
        } else if candidate.registry.version != current.registry.version {
            return true;
        }

        candidate.hash != current.hash
    } else {
        true
    }
}

fn parse_registry_bytes(raw: &[u8], source: impl std::fmt::Display) -> Result<Registry> {
    let raw = std::str::from_utf8(raw)
        .with_context(|| format!("registry json from `{}` is not valid utf-8", source))?;
    let expanded = expand_env_vars(raw);
    serde_json::from_str(&expanded)
        .with_context(|| format!("failed to parse registry json from `{}`", source))
}

fn load_registry_from_file(path: &Path) -> Result<RegistryBlob> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read registry file `{}`", path.display()))?;
    let expanded = expand_env_vars(&raw);
    let expanded_bytes = expanded.into_bytes();
    let registry = parse_registry_bytes(&expanded_bytes, path.display())?;
    let hash = hash_bytes(&expanded_bytes);
    Ok(RegistryBlob {
        registry,
        raw: expanded_bytes,
        hash,
    })
}

async fn load_registry_from_url(url: &str) -> Result<RegistryBlob> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("build registry http client")?;
    let res = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetch registry from {url}"))?;
    let status = res.status();
    let bytes = res
        .bytes()
        .await
        .with_context(|| format!("read registry body from {url}"))?
        .to_vec();
    if !status.is_success() {
        anyhow::bail!("registry fetch failed with status {status}");
    }
    let registry = parse_registry_bytes(&bytes, url)?;
    let hash = hash_bytes(&bytes);
    Ok(RegistryBlob {
        registry,
        raw: bytes,
        hash,
    })
}

async fn load_registry_from_source(source: &RegistrySource) -> Result<RegistryBlob> {
    match source {
        RegistrySource::File(path) => load_registry_from_file(path),
        RegistrySource::Url(url) => load_registry_from_url(url).await,
        RegistrySource::Ipfs { cid, gateway } => {
            let url = format!("{}/ipfs/{}", gateway.trim_end_matches('/'), cid);
            load_registry_from_url(&url).await
        }
        RegistrySource::Ipns { name, gateway } => {
            let url = format!("{}/ipns/{}", gateway.trim_end_matches('/'), name);
            load_registry_from_url(&url).await
        }
    }
}

fn persist_cache(path: &Path, registry: &RegistryBlob) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create cache directory `{}`", parent.display()))?;
    }
    fs::write(path, &registry.raw)
        .with_context(|| format!("write registry cache `{}`", path.display()))
}

fn load_cache(path: &Path) -> Result<RegistryBlob> {
    load_registry_from_file(path).with_context(|| "read registry cache".to_string())
}

fn hashes_match(observed: &str, expected: &str) -> bool {
    let observed = observed.strip_prefix("0x").unwrap_or(observed);
    let expected = expected.strip_prefix("0x").unwrap_or(expected);
    observed.eq_ignore_ascii_case(expected)
}

fn registry_sources_from_env() -> Vec<RegistrySource> {
    fn non_empty_env(name: &str) -> Option<String> {
        match std::env::var(name) {
            Ok(raw) if !raw.trim().is_empty() => Some(raw),
            _ => None,
        }
    }

    fn is_ops_inputs_path(path: &Path) -> bool {
        let file = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| {
                name.eq_ignore_ascii_case("inputs.yaml") || name.eq_ignore_ascii_case("inputs.yml")
            })
            .unwrap_or(false);
        let parent = path
            .parent()
            .and_then(|parent| parent.file_name())
            .and_then(|name| name.to_str())
            .map(|name| name.eq_ignore_ascii_case("ops"))
            .unwrap_or(false);
        file && parent
    }

    let mut sources = Vec::new();

    if let Some(path) = non_empty_env("REGISTRY_FILE") {
        let path_buf = PathBuf::from(path);
        if is_ops_inputs_path(&path_buf) {
            warn!(
                path = %path_buf.display(),
                "REGISTRY_FILE points to ops inputs; ignoring invalid registry source",
            );
        } else {
            sources.push(RegistrySource::File(path_buf));
        }
    }

    if let Some(url) = non_empty_env("REGISTRY_URL") {
        sources.push(RegistrySource::Url(url));
    }

    if let Some(cid) = non_empty_env("REGISTRY_IPFS_CID") {
        let gateway =
            non_empty_env("REGISTRY_IPFS_GATEWAY").unwrap_or_else(|| "https://ipfs.io".to_string());
        sources.push(RegistrySource::Ipfs { cid, gateway });
    }

    if let Some(name) = non_empty_env("REGISTRY_IPNS") {
        let gateway =
            non_empty_env("REGISTRY_IPFS_GATEWAY").unwrap_or_else(|| "https://ipfs.io".to_string());
        sources.push(RegistrySource::Ipns { name, gateway });
    }

    if sources.is_empty() {
        for default_path in [
            PathBuf::from("config/registry.json"),
            PathBuf::from("config/registry.example.json"),
        ] {
            if default_path.exists() {
                sources.push(RegistrySource::File(default_path));
                break;
            }
        }
    }

    sources
}

fn describe_source(source: &RegistrySource) -> String {
    match source {
        RegistrySource::File(path) => format!("file:{}", path.display()),
        RegistrySource::Url(url) => url.clone(),
        RegistrySource::Ipfs { cid, gateway } => {
            format!("ipfs:{cid} via {}", gateway.trim_end_matches('/'))
        }
        RegistrySource::Ipns { name, gateway } => {
            format!("ipns:{name} via {}", gateway.trim_end_matches('/'))
        }
    }
}

pub async fn maybe_load_registry() -> Result<Option<Registry>> {
    let cache = cache_path();
    let expected_hash = std::env::var("REGISTRY_EXPECTED_HASH")
        .ok()
        .map(|hash| hash.trim().to_string())
        .filter(|hash| !hash.is_empty());
    let sources = registry_sources_from_env();

    let mut best: Option<(RegistryBlob, bool)> = None;
    let mut cached_blob: Option<RegistryBlob> = None;

    if cache.exists() {
        match load_cache(&cache) {
            Ok(blob) => {
                if let Some(expected) = expected_hash.as_deref() {
                    if !hashes_match(&blob.hash, expected) {
                        warn!(
                            expected = %expected,
                            observed = %blob.hash,
                            source = %cache.display(),
                            "Registry cache hash mismatch; refusing to load",
                        );
                    } else {
                        info!(
                            version = %blob.registry.version,
                            source = %cache.display(),
                            hash = %blob.hash,
                            "Loaded registry from cache",
                        );
                        cached_blob = Some(blob.clone());
                        best = Some((blob, true));
                    }
                } else {
                    info!(
                        version = %blob.registry.version,
                        source = %cache.display(),
                        hash = %blob.hash,
                        "Loaded registry from cache",
                    );
                    cached_blob = Some(blob.clone());
                    best = Some((blob, true));
                }
            }
            Err(err) => {
                warn!(error = %err, "Failed to load registry cache");
            }
        }
    }

    for source in &sources {
        match load_registry_from_source(source).await {
            Ok(blob) => {
                if let Some(expected) = expected_hash.as_deref() {
                    if !hashes_match(&blob.hash, expected) {
                        warn!(
                            expected = %expected,
                            observed = %blob.hash,
                            source = %describe_source(source),
                            "Registry hash mismatch; skipping source",
                        );
                        continue;
                    }
                }

                let replace = should_replace_cache(&blob, best.as_ref().map(|(b, _)| b));
                info!(
                    version = %blob.registry.version,
                    hash = %blob.hash,
                    source = %describe_source(source),
                    replace_cache = replace,
                    "Loaded registry from source",
                );

                if replace {
                    best = Some((blob, false));
                }
            }
            Err(err) => {
                warn!(source = %describe_source(source), error = %err, "Registry fetch failed");
            }
        }
    }

    if let Some((blob, is_cache)) = best {
        if !is_cache && should_replace_cache(&blob, cached_blob.as_ref()) {
            persist_cache(&cache, &blob)?;
        }
        return Ok(Some(blob.registry));
    }

    warn!("No registry could be loaded; check configuration and hashes");
    Ok(None)
}

pub fn apply_pool_env_overrides(prefix: &str, registry_chain: &RegistryChain) -> Result<()> {
    if !registry_chain.pools.balancer.is_empty() {
        let key = format!("{}_BAL_POOLS", prefix);
        if std::env::var(&key).is_ok() {
            warn!(prefix = prefix, key = %key, "pool config conflict; using registry value");
        }
        let value = serde_json::to_string(&registry_chain.pools.balancer)
            .context("serialize balancer pools for env override")?;
        std::env::set_var(key, value);
    }

    if !registry_chain.pools.curve.is_empty() {
        let key = format!("{}_CURVE_POOLS", prefix);
        if std::env::var(&key).is_ok() {
            warn!(prefix = prefix, key = %key, "pool config conflict; using registry value");
        }
        let value = serde_json::to_string(&registry_chain.pools.curve)
            .context("serialize curve pools for env override")?;
        std::env::set_var(key, value);
    }

    if !registry_chain.pools.univ2.is_empty() {
        let key = format!("{}_UNIV2_POOLS", prefix);
        if std::env::var(&key).is_ok() {
            warn!(prefix = prefix, key = %key, "pool config conflict; using registry value");
        }
        let value = serde_json::to_string(&registry_chain.pools.univ2)
            .context("serialize univ2 pools for env override")?;
        std::env::set_var(key, value);
    }

    Ok(())
}

pub fn parse_address(raw: &str, field: &str) -> Result<Address> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!("invalid address for {field}: value is empty");
    }

    match Address::from_str(trimmed) {
        Ok(addr) => Ok(addr),
        Err(_) => {
            let stripped = trimmed.strip_prefix("0x").unwrap_or(trimmed);
            let bytes = hex::decode(stripped)
                .with_context(|| format!("invalid address for {field}: {raw}"))?;
            ensure!(
                bytes.len() == 20,
                "invalid address for {field}: {raw} (expected 20 bytes of hex)"
            );
            Ok(Address::from_slice(&bytes))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::MockServer;
    use once_cell::sync::Lazy;
    use std::sync::Mutex;
    use tempfile::tempdir;

    static REGISTRY_TEST_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));
    const REGISTRY_ENV_KEYS: &[&str] = &[
        "REGISTRY_FILE",
        "REGISTRY_URL",
        "REGISTRY_IPFS_CID",
        "REGISTRY_IPNS",
        "REGISTRY_IPFS_GATEWAY",
        "REGISTRY_CACHE_PATH",
        "REGISTRY_EXPECTED_HASH",
        "TEST_RPC_KEY",
    ];

    struct EnvGuard {
        saved: Vec<(String, Option<String>)>,
    }

    impl EnvGuard {
        fn new(keys: &[&str]) -> Self {
            let saved = keys
                .iter()
                .map(|key| ((*key).to_string(), std::env::var(key).ok()))
                .collect();
            Self { saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in self.saved.drain(..) {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    struct CwdGuard {
        original: std::path::PathBuf,
    }

    impl CwdGuard {
        fn new() -> Result<Self> {
            Ok(Self {
                original: std::env::current_dir().context("capture current dir")?,
            })
        }

        fn set(&self, path: &std::path::Path) -> Result<()> {
            std::env::set_current_dir(path)
                .with_context(|| format!("set current dir to {}", path.display()))
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            if let Err(err) = std::env::set_current_dir(&self.original) {
                eprintln!("failed to restore current dir: {err}");
            }
        }
    }

    fn reset_registry_env() {
        for key in REGISTRY_ENV_KEYS {
            std::env::remove_var(key);
        }
    }

    fn sample_registry() -> Registry {
        Registry {
            version: "1.0.0".into(),
            last_updated: None,
            chains: HashMap::from([(
                "arbitrum".into(),
                RegistryChain {
                    env_prefix: Some("ARB".into()),
                    chain_id: 42_161,
                    rpc_http: vec!["https://arb-mainnet".into()],
                    rpc_ws: vec!["wss://arb-mainnet".into()],
                    tokens: vec!["0x0000000000000000000000000000000000000001".into()],
                    univ3_quoter: Some("0x0000000000000000000000000000000000000002".into()),
                    univ3_factory: Some("0x0000000000000000000000000000000000000003".into()),
                    univ3_router: Some("0x0000000000000000000000000000000000000004".into()),
                    bal_vault: Some("0x0000000000000000000000000000000000000005".into()),
                    aave_pool: Some("0x0000000000000000000000000000000000000006".into()),
                    bal_flashloan_tokens: vec![],
                    pools: RegistryPools::default(),
                },
            )]),
        }
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn loads_from_file_and_caches() {
        let _guard = REGISTRY_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::new(REGISTRY_ENV_KEYS);
        reset_registry_env();
        let dir = tempdir().unwrap();
        let registry_path = dir.path().join("registry.json");
        let cache_path = dir.path().join("cache.json");
        let registry = sample_registry();
        fs::write(&registry_path, serde_json::to_vec(&registry).unwrap()).unwrap();
        std::env::set_var("REGISTRY_FILE", &registry_path);
        std::env::set_var("REGISTRY_CACHE_PATH", &cache_path);

        let loaded = maybe_load_registry().await.unwrap().unwrap();
        assert_eq!(loaded.version, registry.version);
        assert!(cache_path.exists());
        assert_eq!(
            fs::read(&cache_path).unwrap(),
            serde_json::to_vec(&registry).unwrap()
        );

        std::env::set_var("REGISTRY_FILE", dir.path().join("missing.json"));
        let cached = maybe_load_registry().await.unwrap().unwrap();
        assert_eq!(cached.version, registry.version);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn expands_env_placeholders_in_registry_file() {
        let _guard = REGISTRY_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::new(REGISTRY_ENV_KEYS);
        reset_registry_env();
        let dir = tempdir().unwrap();
        let registry_path = dir.path().join("registry.json");
        let cache_path = dir.path().join("cache.json");

        let registry = serde_json::json!({
            "version": "1.0.0",
            "chains": {
                "arbitrum": {
                    "chain_id": 42161,
                    "rpc_http": ["https://eth-mainnet.g.alchemy.com/v2/${ALCHEMY_KEY}"]
                }
            }
        });

        fs::write(&registry_path, serde_json::to_vec(&registry).unwrap()).unwrap();
        std::env::set_var("REGISTRY_FILE", &registry_path);
        std::env::set_var("REGISTRY_CACHE_PATH", &cache_path);
        std::env::set_var("ALCHEMY_KEY", "test-key");

        let loaded = maybe_load_registry().await.unwrap().unwrap();
        assert_eq!(
            loaded
                .chains
                .get("arbitrum")
                .unwrap()
                .rpc_http
                .first()
                .unwrap(),
            "https://eth-mainnet.g.alchemy.com/v2/test-key"
        );
        assert!(String::from_utf8(fs::read(&cache_path).unwrap())
            .unwrap()
            .contains("test-key"));
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn falls_back_to_cache_on_failure() {
        let _guard = REGISTRY_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::new(REGISTRY_ENV_KEYS);
        reset_registry_env();
        let dir = tempdir().unwrap();
        let cache_path = dir.path().join("cache.json");
        let registry = sample_registry();
        fs::write(&cache_path, serde_json::to_vec(&registry).unwrap()).unwrap();
        std::env::set_var("REGISTRY_CACHE_PATH", &cache_path);
        std::env::set_var("REGISTRY_FILE", dir.path().join("missing.json"));

        let loaded = maybe_load_registry().await.unwrap().unwrap();
        assert_eq!(loaded.version, registry.version);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn prefers_newer_registry_over_cache() {
        let _guard = REGISTRY_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::new(REGISTRY_ENV_KEYS);
        reset_registry_env();
        let dir = tempdir().unwrap();
        let cache_path = dir.path().join("cache.json");
        let registry_path = dir.path().join("registry.json");

        let mut cached_registry = sample_registry();
        cached_registry.version = "0.9.0".into();
        fs::write(&cache_path, serde_json::to_vec(&cached_registry).unwrap()).unwrap();

        let mut fresh_registry = sample_registry();
        fresh_registry.version = "1.1.0".into();
        fs::write(&registry_path, serde_json::to_vec(&fresh_registry).unwrap()).unwrap();

        std::env::set_var("REGISTRY_CACHE_PATH", &cache_path);
        std::env::set_var("REGISTRY_FILE", &registry_path);
        std::env::set_var(
            "REGISTRY_EXPECTED_HASH",
            hash_bytes(&serde_json::to_vec(&fresh_registry).unwrap()),
        );

        let loaded = maybe_load_registry().await.unwrap().unwrap();
        assert_eq!(loaded.version, fresh_registry.version);
        assert_eq!(
            fs::read(&cache_path).unwrap(),
            serde_json::to_vec(&fresh_registry).unwrap()
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn loads_from_ipfs_gateway_and_caches() {
        let _guard = REGISTRY_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::new(REGISTRY_ENV_KEYS);
        reset_registry_env();
        let dir = tempdir().unwrap();
        let cache_path = dir.path().join("cache.json");

        let registry = sample_registry();
        let registry_bytes = serde_json::to_vec(&registry).unwrap();

        let server = MockServer::start();
        let cid = "bafyregtest";
        server.mock(|when, then| {
            when.path(format!("/ipfs/{cid}"));
            then.status(200)
                .header("content-type", "application/json")
                .body(registry_bytes.clone());
        });

        std::env::set_var("REGISTRY_IPFS_CID", cid);
        std::env::set_var("REGISTRY_IPFS_GATEWAY", server.base_url());
        std::env::set_var("REGISTRY_CACHE_PATH", &cache_path);
        std::env::set_var("REGISTRY_EXPECTED_HASH", hash_bytes(&registry_bytes));

        let loaded = maybe_load_registry().await.unwrap().unwrap();
        assert_eq!(loaded.version, registry.version);
        assert!(cache_path.exists());
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn rejects_bad_hash_and_uses_cached() {
        let _guard = REGISTRY_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::new(REGISTRY_ENV_KEYS);
        reset_registry_env();
        let dir = tempdir().unwrap();
        let cache_path = dir.path().join("cache.json");
        let registry_path = dir.path().join("registry.json");

        let registry = sample_registry();
        let cache_bytes = serde_json::to_vec(&registry).unwrap();
        fs::write(&cache_path, &cache_bytes).unwrap();
        std::env::set_var("REGISTRY_CACHE_PATH", &cache_path);

        let mut tampered = registry.clone();
        tampered.version = "2.0.0".into();
        fs::write(&registry_path, serde_json::to_vec(&tampered).unwrap()).unwrap();
        std::env::set_var("REGISTRY_FILE", &registry_path);
        std::env::set_var("REGISTRY_EXPECTED_HASH", hash_bytes(&cache_bytes));

        let loaded = maybe_load_registry().await.unwrap().unwrap();
        assert_eq!(loaded.version, registry.version);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn treats_blank_expected_hash_as_unset() {
        let _guard = REGISTRY_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::new(REGISTRY_ENV_KEYS);
        reset_registry_env();
        let dir = tempdir().unwrap();
        let registry_path = dir.path().join("registry.json");
        let cache_path = dir.path().join("cache.json");

        let mut registry = sample_registry();
        registry.version = "1.2.3".into();
        fs::write(&registry_path, serde_json::to_vec(&registry).unwrap()).unwrap();

        std::env::set_var("REGISTRY_FILE", &registry_path);
        std::env::set_var("REGISTRY_CACHE_PATH", &cache_path);
        std::env::set_var("REGISTRY_EXPECTED_HASH", "   ");

        let loaded = maybe_load_registry().await.unwrap().unwrap();
        assert_eq!(loaded.version, registry.version);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn refuses_unverified_registry_without_cache() {
        let _guard = REGISTRY_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::new(REGISTRY_ENV_KEYS);
        reset_registry_env();
        let dir = tempdir().unwrap();
        let registry_path = dir.path().join("registry.json");

        let registry = sample_registry();
        fs::write(&registry_path, serde_json::to_vec(&registry).unwrap()).unwrap();

        std::env::set_var("REGISTRY_FILE", &registry_path);
        std::env::set_var("REGISTRY_CACHE_PATH", dir.path().join("cache.json"));
        std::env::set_var("REGISTRY_EXPECTED_HASH", "deadbeef");

        let loaded = maybe_load_registry().await.unwrap();
        assert!(loaded.is_none());
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn falls_back_to_default_config_when_no_env_sources() {
        let _guard = REGISTRY_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::new(REGISTRY_ENV_KEYS);
        reset_registry_env();
        let dir = tempdir().unwrap();
        let cwd_guard = CwdGuard::new().unwrap();
        let cache_path = dir.path().join("cache.json");

        let config_dir = dir.path().join("config");
        fs::create_dir_all(&config_dir).unwrap();
        let registry_path = config_dir.join("registry.json");

        let registry = sample_registry();
        fs::write(&registry_path, serde_json::to_vec(&registry).unwrap()).unwrap();

        cwd_guard.set(dir.path()).unwrap();
        std::env::set_var("REGISTRY_CACHE_PATH", &cache_path);

        let loaded = maybe_load_registry().await.unwrap().unwrap();
        assert_eq!(loaded.version, registry.version);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn ignores_empty_env_vars() {
        let _guard = REGISTRY_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::new(REGISTRY_ENV_KEYS);
        reset_registry_env();
        let dir = tempdir().unwrap();
        let cwd_guard = CwdGuard::new().unwrap();

        cwd_guard.set(dir.path()).unwrap();
        std::env::set_var("REGISTRY_FILE", "");
        std::env::set_var("REGISTRY_URL", "   ");
        std::env::set_var("REGISTRY_IPFS_CID", "");
        std::env::set_var("REGISTRY_IPNS", "");
        std::env::set_var("REGISTRY_CACHE_PATH", dir.path().join("cache.json"));

        let loaded = maybe_load_registry().await.unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn ignores_ops_inputs_as_registry_source() {
        let _guard = REGISTRY_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::new(REGISTRY_ENV_KEYS);
        reset_registry_env();
        let dir = tempdir().unwrap();
        let cwd_guard = CwdGuard::new().unwrap();
        let ops_dir = dir.path().join("ops");
        fs::create_dir_all(&ops_dir).unwrap();
        let ops_inputs = ops_dir.join("inputs.yaml");

        cwd_guard.set(dir.path()).unwrap();
        std::env::set_var("REGISTRY_FILE", &ops_inputs);

        let sources = registry_sources_from_env();
        assert!(sources.is_empty());
    }

    #[test]
    fn parses_lowercase_addresses_without_checksum() {
        let raw = "0x1f98431c8ad98523631ae4a59f267346ea31f984".to_ascii_lowercase();
        let parsed = parse_address(&raw, "test").unwrap();
        assert_eq!(
            parsed,
            Address::from_slice(&hex_literal::hex!(
                "1f98431c8ad98523631ae4a59f267346ea31f984"
            ))
        );
    }

    #[test]
    fn expands_env_placeholders_in_registry_json() {
        let _guard = REGISTRY_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::new(REGISTRY_ENV_KEYS);
        reset_registry_env();
        std::env::set_var("TEST_RPC_KEY", "abc123");

        let raw = r#"{
          "version": "1.0.0",
          "chains": {
            "arbitrum": {
              "chain_id": 42161,
              "rpc_http": ["https://arb-mainnet.g.alchemy.com/v2/${TEST_RPC_KEY}"]
            }
          }
        }"#;

        let registry = parse_registry_bytes(raw.as_bytes(), "inline").unwrap();
        assert_eq!(
            registry.chains["arbitrum"].rpc_http,
            vec!["https://arb-mainnet.g.alchemy.com/v2/abc123"]
        );
    }
}
