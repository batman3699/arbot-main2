//! Immutable, validated, versioned APEX-MEV v4 configuration.
//!
//! One job: turn `ops/inputs.yaml` plus a small set of secrets into a single
//! `Arc<ApexConfig>` at boot, or refuse to start. Blueprint §2.4 names "late
//! configuration lookup" as an engineering failure that loses opportunities, and
//! this repository reads 84 `ARBOT_*` variables at call sites throughout the hot
//! path. The chokepoint is the fix.
//!
//! Fails closed by design. A half-resolved config is worse than no config: an
//! unset `${VAR}` that survives as a literal string is how a placeholder reaches
//! an address field and gets treated as real.

mod env;
mod schema;
mod secret;

pub use env::{Env, SECRET_PREFIX};
pub use schema::*;
pub use secret::Secret;

use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config section `{0}` is required but missing or unparseable: {1}")]
    MissingSection(&'static str, String),

    #[error("chain `{chain}`: required field `{field}` is empty; a chain with nowhere to talk to is a misconfiguration, not a default")]
    EmptyRequiredField { chain: String, field: &'static str },

    #[error("unresolved environment placeholder `${{{0}}}` in {1}; refusing to pass it through as a literal")]
    UnresolvedPlaceholder(String, String),

    #[error("duplicate chain {kind} `{value}`")]
    DuplicateChain { kind: &'static str, value: String },

    #[error("chain `{0}` has no entry in risk.per_chain; a chain that trades with no declared policy is safety theater")]
    UnpolicedChain(String),

    #[error("failed to read {0}: {1}")]
    Io(String, String),
}

/// The resolved, immutable configuration. Cloning is cheap enough at boot; it is
/// never mutated after construction, and in-flight tickets keep the snapshot
/// they were admitted under.
#[derive(Debug, Clone)]
pub struct ApexConfig {
    raw: RawConfig,
    config_version: String,
    consulted_env_keys: Vec<String>,
}

impl ApexConfig {
    pub fn load_from(inputs_path: &str, env: &Env) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(inputs_path)
            .map_err(|e| ConfigError::Io(inputs_path.to_string(), e.to_string()))?;
        Self::from_yaml_str(&text, env)
    }

    pub fn from_yaml_str(yaml: &str, env: &Env) -> Result<Self, ConfigError> {
        // Content-address BEFORE interpolation, so the version identifies the
        // declared configuration rather than the machine it resolved on.
        let config_version = format!("{:x}", Sha256::digest(yaml.as_bytes()));

        let resolved = interpolate(yaml, env)?;
        let raw: RawConfig = serde_yaml::from_str(&resolved).map_err(|e| {
            let msg = e.to_string();
            // serde_yaml names the field it could not find; surface it so the
            // operator sees which section is at fault rather than a parse trace.
            for section in ["chains", "universe", "risk"] {
                if msg.contains(section) {
                    return ConfigError::MissingSection(
                        match section {
                            "chains" => "chains",
                            "universe" => "universe",
                            _ => "risk",
                        },
                        msg.clone(),
                    );
                }
            }
            ConfigError::MissingSection("chains", msg)
        })?;

        validate(&raw)?;

        Ok(Self { raw, config_version, consulted_env_keys: env.consulted() })
    }

    pub fn config_version(&self) -> &str {
        &self.config_version
    }

    pub fn consulted_env_keys(&self) -> &[String] {
        &self.consulted_env_keys
    }

    pub fn chains(&self) -> &[RawChain] {
        &self.raw.chains
    }

    pub fn universe(&self) -> &RawUniverse {
        &self.raw.universe
    }

    pub fn risk(&self) -> &RawRisk {
        &self.raw.risk
    }

    pub fn chain(&self, name: &str) -> Option<&RawChain> {
        self.raw.chains.iter().find(|c| c.chain_name == name)
    }
}

/// Resolve `${VAR}` placeholders, refusing anything unset.
///
/// `ops/inputs.yaml` uses this form (e.g. `${ARB_EXECUTOR_ADDRESS}`). Leaving an
/// unset one in place is the failure mode that matters: the literal string
/// `"${ARB_EXECUTOR_ADDRESS}"` in an address field parses as garbage far away
/// from the cause, or worse, is silently skipped.
fn interpolate(yaml: &str, env: &Env) -> Result<String, ConfigError> {
    let mut out = String::with_capacity(yaml.len());
    let mut rest = yaml;

    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            // An unterminated `${` is literal text, not a placeholder.
            out.push_str(&rest[start..]);
            return Ok(out);
        };
        let key = &after[..end];
        match env.get(key) {
            Some(v) => out.push_str(&v),
            None => {
                let line = yaml[..start].lines().count();
                return Err(ConfigError::UnresolvedPlaceholder(
                    key.to_string(),
                    format!("line {line}"),
                ));
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn validate(raw: &RawConfig) -> Result<(), ConfigError> {
    let mut names = BTreeSet::new();
    let mut ids = BTreeSet::new();

    for c in &raw.chains {
        if !names.insert(c.chain_name.clone()) {
            return Err(ConfigError::DuplicateChain {
                kind: "chain_name",
                value: c.chain_name.clone(),
            });
        }
        if !ids.insert(c.chain_id) {
            return Err(ConfigError::DuplicateChain {
                kind: "chain_id",
                value: c.chain_id.to_string(),
            });
        }
        if c.rpc_http_urls.is_empty() {
            return Err(ConfigError::EmptyRequiredField {
                chain: c.chain_name.clone(),
                field: "rpc_http_urls",
            });
        }

        let policed = raw.risk.per_chain.iter().any(|r| r.chain_name == c.chain_name);
        if !policed {
            return Err(ConfigError::UnpolicedChain(c.chain_name.clone()));
        }
    }
    Ok(())
}

/// Test-support helpers. Not `#[cfg(test)]` because integration tests are
/// separate crates and need them.
pub mod testing {
    use super::Env;

    /// Interpolate, leaving unset `${VAR}` placeholders as-is.
    ///
    /// For tests that need to inspect the SHAPE of the production file without
    /// tripping the fail-closed placeholder check. Never use in production: a
    /// surviving placeholder in an address field is exactly what
    /// `ConfigError::UnresolvedPlaceholder` exists to prevent.
    pub fn interpolate_permissively(yaml: &str) -> String {
        let env = Env::permissive();
        let mut out = String::with_capacity(yaml.len());
        let mut rest = yaml;
        while let Some(start) = rest.find("${") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let Some(end) = after.find('}') else {
                out.push_str(&rest[start..]);
                return out;
            };
            let key = &after[..end];
            match env.get(key) {
                Some(v) => out.push_str(&v),
                None => out.push_str(&format!("UNSET_{key}")),
            }
            rest = &after[end + 1..];
        }
        out.push_str(rest);
        out
    }
}
