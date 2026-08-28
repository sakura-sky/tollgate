// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Andrew Stevens

//! API-key generation, hashing, and verification.
//!
//! Tollgate issues keys shaped `tgk_<pub>_<secret>`:
//! - `tgk_<pub>` is the **public prefix** - a non-secret lookup handle stored in
//!   `api_keys.prefix` and used to find the row before verifying.
//! - `<secret>` is high-entropy random (192-bit) and is **never stored**. Only a
//!   keyed hash of it is kept.
//!
//! ## Why HMAC-SHA256, not argon2id
//!
//! argon2id is the right choice for *passwords* - low-entropy, human-chosen
//! secrets that must resist offline cracking. API keys are the opposite: 192
//! bits of randomness, which no attacker can brute-force regardless of hash
//! speed. So the slow-hash tax buys nothing here and would blow the sub-5ms
//! per-request budget. The correct best practice for high-entropy tokens is a
//! *fast keyed* hash: HMAC-SHA256 under a server-side **pepper**, compared in
//! constant time. The pepper (config, ideally sealed in a secret manager) means
//! a database leak alone cannot verify keys. argon2id is reserved for the future
//! console's human-password login.

use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

/// Key tag / scheme prefix.
pub const KEY_TAG: &str = "tgk";

/// Errors parsing a presented key.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeyError {
    #[error("malformed API key")]
    Malformed,
}

/// A freshly generated key. `plaintext` is shown to the operator exactly once
/// and never persisted; `prefix` and `key_hash` are what get stored.
#[derive(Debug, Clone)]
pub struct GeneratedKey {
    /// `tgk_<pub>_<secret>` - display once, never store.
    pub plaintext: String,
    /// `tgk_<pub>` - public lookup handle (stored in `api_keys.prefix`).
    pub prefix: String,
    /// Hex HMAC-SHA256(pepper, secret) - stored in `api_keys.key_hash`.
    pub key_hash: String,
}

/// Hashes and verifies API-key secrets with HMAC-SHA256 under a server pepper.
#[derive(Clone)]
pub struct KeyHasher {
    pepper: Vec<u8>,
}

impl std::fmt::Debug for KeyHasher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the pepper.
        f.debug_struct("KeyHasher").finish_non_exhaustive()
    }
}

impl KeyHasher {
    /// Build a hasher from a configured pepper (any length; keep it secret).
    #[must_use]
    pub fn new(pepper: impl Into<Vec<u8>>) -> Self {
        Self {
            pepper: pepper.into(),
        }
    }

    /// A hasher with a random pepper - for the demo and tests only. A production
    /// deployment MUST supply a fixed, secret pepper so restarts keep verifying.
    #[must_use]
    pub fn random() -> Self {
        let mut pepper = vec![0u8; 32];
        rand::thread_rng().fill_bytes(&mut pepper);
        Self { pepper }
    }

    fn mac(&self, secret: &str) -> Vec<u8> {
        let mut mac =
            HmacSha256::new_from_slice(&self.pepper).expect("HMAC accepts a key of any length");
        mac.update(secret.as_bytes());
        mac.finalize().into_bytes().to_vec()
    }

    /// Hex HMAC of a secret, for storage.
    #[must_use]
    pub fn hash(&self, secret: &str) -> String {
        hex::encode(self.mac(secret))
    }

    /// Constant-time verification of a presented secret against a stored hash.
    #[must_use]
    pub fn verify(&self, secret: &str, stored_hex: &str) -> bool {
        let expected = self.mac(secret);
        match hex::decode(stored_hex) {
            Ok(stored) => stored.ct_eq(&expected).into(),
            Err(_) => false,
        }
    }

    /// Generate a new key (`tgk_<pub>_<secret>`), returning the plaintext to show
    /// once plus the prefix and hash to store.
    #[must_use]
    pub fn generate(&self) -> GeneratedKey {
        let mut rng = rand::thread_rng();
        let mut pub_bytes = [0u8; 8];
        let mut secret_bytes = [0u8; 24];
        rng.fill_bytes(&mut pub_bytes);
        rng.fill_bytes(&mut secret_bytes);
        let secret = hex::encode(secret_bytes);
        let prefix = format!("{KEY_TAG}_{}", hex::encode(pub_bytes));
        let plaintext = format!("{prefix}_{secret}");
        let key_hash = self.hash(&secret);
        GeneratedKey {
            plaintext,
            prefix,
            key_hash,
        }
    }
}

/// Split a presented `tgk_<pub>_<secret>` into `(prefix, secret)`.
///
/// # Errors
/// Returns [`KeyError::Malformed`] if the key does not have the expected shape.
pub fn parse(key: &str) -> Result<(String, String), KeyError> {
    let rest = key
        .strip_prefix(KEY_TAG)
        .and_then(|r| r.strip_prefix('_'))
        .ok_or(KeyError::Malformed)?;
    let (pub_hex, secret) = rest.split_once('_').ok_or(KeyError::Malformed)?;
    if pub_hex.is_empty() || secret.is_empty() {
        return Err(KeyError::Malformed);
    }
    Ok((format!("{KEY_TAG}_{pub_hex}"), secret.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_parse_verify_roundtrip() {
        let hasher = KeyHasher::random();
        let key = hasher.generate();
        // Shape.
        assert!(key.plaintext.starts_with("tgk_"));
        assert!(key.plaintext.starts_with(&key.prefix));
        // Parse the plaintext back and verify against the stored hash.
        let (prefix, secret) = parse(&key.plaintext).expect("well-formed");
        assert_eq!(prefix, key.prefix);
        assert!(hasher.verify(&secret, &key.key_hash));
    }

    #[test]
    fn wrong_secret_fails() {
        let hasher = KeyHasher::random();
        let key = hasher.generate();
        assert!(!hasher.verify("deadbeef", &key.key_hash));
    }

    #[test]
    fn different_pepper_cannot_verify() {
        let a = KeyHasher::random();
        let key = a.generate();
        let (_, secret) = parse(&key.plaintext).unwrap();
        let b = KeyHasher::random(); // different pepper
        assert!(!b.verify(&secret, &key.key_hash));
    }

    #[test]
    fn fixed_pepper_is_stable_across_instances() {
        let pepper = b"a-fixed-secret-pepper".to_vec();
        let h1 = KeyHasher::new(pepper.clone());
        let key = h1.generate();
        let (_, secret) = parse(&key.plaintext).unwrap();
        // A second hasher with the SAME pepper must verify - this is what makes
        // restarts work in production.
        let h2 = KeyHasher::new(pepper);
        assert!(h2.verify(&secret, &key.key_hash));
    }

    #[test]
    fn malformed_keys_are_rejected() {
        for bad in ["", "tgk_", "tgk_abc", "nope_a_b", "tgk__b", "tgk_a_"] {
            assert_eq!(
                parse(bad),
                Err(KeyError::Malformed),
                "should reject {bad:?}"
            );
        }
    }
}
