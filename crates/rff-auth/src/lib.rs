//! `rff-auth` — authentication abstraction for the API server.
//!
//! Per the project's access-management requirement, the primary mechanism is a
//! **MATA mID** (sovereign cryptographic identity) verified locally — no central
//! auth server, built for headless/fleet deployments. This crate defines the
//! [`Authenticator`] trait so the server stays decoupled from any specific
//! scheme, ships a [`MataMidVerifier`] (behind the `mata-mid` feature), and a
//! [`DevAllowAll`] verifier for local development.

/// A verified caller identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// Stable subject identifier (e.g. the mID public key / DID).
    pub subject: String,
    /// Optional human-friendly display name.
    pub display_name: Option<String>,
}

/// Why authentication failed.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("missing credential")]
    Missing,
    #[error("invalid or expired credential")]
    Invalid,
    #[error("authentication backend not configured: {0}")]
    NotConfigured(&'static str),
}

/// Verifies a presented credential (e.g. a bearer token) into an [`Identity`].
pub trait Authenticator: Send + Sync {
    /// Verify `credential`, returning the caller's identity on success.
    fn authenticate(&self, credential: &str) -> Result<Identity, AuthError>;
}

/// Development-only verifier that accepts any non-empty credential. **Never use
/// in production** — it performs no cryptographic verification.
pub struct DevAllowAll;

impl Authenticator for DevAllowAll {
    fn authenticate(&self, credential: &str) -> Result<Identity, AuthError> {
        if credential.is_empty() {
            return Err(AuthError::Missing);
        }
        Ok(Identity {
            subject: format!("dev:{credential}"),
            display_name: Some("development user".into()),
        })
    }
}

/// MATA mID verifier.
///
/// Verifies a sovereign identity credential locally. The cryptographic
/// verification will be delegated to the `sovereign-id-verify` crate from the
/// Remade-With-Rust org; until that dependency is wired this returns
/// [`AuthError::NotConfigured`] so it can never silently authorize anyone.
#[cfg(feature = "mata-mid")]
pub struct MataMidVerifier {
    _private: (),
}

#[cfg(feature = "mata-mid")]
impl MataMidVerifier {
    pub fn new() -> MataMidVerifier {
        MataMidVerifier { _private: () }
    }
}

#[cfg(feature = "mata-mid")]
impl Default for MataMidVerifier {
    fn default() -> Self {
        MataMidVerifier::new()
    }
}

#[cfg(feature = "mata-mid")]
impl Authenticator for MataMidVerifier {
    fn authenticate(&self, _credential: &str) -> Result<Identity, AuthError> {
        // TODO(auth): verify the mID signature/claims via sovereign-id-verify.
        Err(AuthError::NotConfigured(
            "MATA mID verification (sovereign-id-verify) not yet wired",
        ))
    }
}
