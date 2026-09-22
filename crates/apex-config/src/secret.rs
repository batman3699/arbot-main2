//! Credentials that cannot leak through the three channels that normally leak
//! them: `Debug`, `Display` and `Serialize` (Blueprint §43, INV-46).

use serde::{Serialize, Serializer};

/// A value that redacts itself everywhere except an explicit [`Secret::expose`].
///
/// Redaction is whole-value, not pattern-based. `.env` here carries provider
/// keys embedded directly in URL paths
/// (`https://base.blockpi.network/v1/rpc/<key>`), so anything that tried to
/// redact just a `key=` query parameter would miss the shape this repo actually
/// uses.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    /// Read the real value. Named `expose` so every read site is greppable and
    /// reads as a deliberate act.
    pub const fn expose(&self) -> &T {
        &self.0
    }

    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> std::fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(REDACTED)")
    }
}

impl<T> std::fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("REDACTED")
    }
}

/// Serializes as the literal string `"REDACTED"`.
///
/// The dangerous path this closes: a config struct derives `Serialize` for a
/// diagnostic dump and silently takes every credential with it.
impl<T> Serialize for Secret<T> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("REDACTED")
    }
}
