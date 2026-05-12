//! Application-layer auth for the signaling [`Bind`] message.
//!
//! [`sign_identity`] builds an [`Identity`] from a [`KeyCert`] and
//! signs it with the private key. [`verify_identity`] checks the
//! timestamp window, parses the cert to extract the EC public key,
//! and verifies the [`Signature`] with `ring`.

use crate::config_man::{hash_der, KeyCert};
use anyhow::{anyhow, Context};
use ring::rand::SystemRandom;
use ring::signature::{
    EcdsaKeyPair, UnparsedPublicKey, ECDSA_P256_SHA256_FIXED, ECDSA_P256_SHA256_FIXED_SIGNING,
};
use std::time::{SystemTime, UNIX_EPOCH};

use super::{CertDerHash, Identity, Signature};

/// Maximum allowed offset between server system time and the
/// identity’s timestamp that’s allowed when verifying the signature.
const MAX_TIMESTAMP_OFFSET: u64 = 30;

/// Build an [`Identity`] for `key_cert` with the current timestamp
/// and sign it with the matching private key. Return the identity and
/// the signied signature.
pub fn sign_identity(key_cert: &KeyCert) -> anyhow::Result<(Identity, Signature)> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock before unix epoch")?
        .as_secs();
    let identity = Identity::from_cert_der(now, &key_cert.cert_der);

    let rng = SystemRandom::new();
    let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &key_cert.key_der, &rng)
        .map_err(|err| anyhow!("Loading signing key: {err}"))?;
    let sig = key
        .sign(&rng, &identity.to_bytes())
        .map_err(|err| anyhow!("Signing identity: {err}"))?;

    Ok((identity, Signature::from_bytes(sig.as_ref())))
}

/// Verify `sig` is a valid signature of `id` made with the private
/// key corresponding to `id.cert`, and that `id.timestamp` is within
/// [`MAX_TIMESTAMP_OFFSET`] of now. Returns the cert hash on success.
pub fn verify_identity(id: &Identity, sig: &Signature) -> anyhow::Result<CertDerHash> {
    // 1. Check timestamp is within offset.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock before unix epoch")?
        .as_secs();
    if now.abs_diff(id.timestamp) > MAX_TIMESTAMP_OFFSET {
        return Err(anyhow!(
            "Identity timestamp {} outside max allowed {}s offset (now={})",
            id.timestamp,
            MAX_TIMESTAMP_OFFSET,
            now
        ));
    }

    // 2. Decode base64-encoded fields to raw bytes.
    let cert_der = id.cert.clone();
    let sig_bytes = sig
        .to_bytes()
        .map_err(|err| anyhow!("Signature base64: {err}"))?;

    // 3. Pull the EC public key out of the X.509 cert.
    let (_, parsed) = x509_parser::parse_x509_certificate(&cert_der)
        .map_err(|err| anyhow!("Parsing identity cert: {err}"))?;
    let pubkey_bytes = parsed.tbs_certificate.subject_pki.subject_public_key.data;

    // 4. Verify signature.
    let pubkey = UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, pubkey_bytes.as_ref());
    pubkey
        .verify(&id.to_bytes(), &sig_bytes)
        .map_err(|_| anyhow!("Identity signature verification failed"))?;

    Ok(hash_der(&cert_der))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_man::create_key_cert;

    #[test]
    fn round_trip() {
        let key_cert = create_key_cert("alice");
        let (id, sig) = sign_identity(&key_cert).unwrap();
        let hash = verify_identity(&id, &sig).unwrap();
        assert_eq!(hash, key_cert.cert_der_hash());
    }

    /// Sign `id` directly with `key_cert`'s private key
    fn resign(id: &Identity, key_cert: &KeyCert) -> Signature {
        let rng = SystemRandom::new();
        let key =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &key_cert.key_der, &rng)
                .unwrap();
        Signature::from_bytes(key.sign(&rng, &id.to_bytes()).unwrap().as_ref())
    }

    #[test]
    fn tampered_cert() {
        let key_cert = create_key_cert("alice");
        let (mut id, sig) = sign_identity(&key_cert).unwrap();
        // Flip the last byte of the cert; signature is over the
        // original bytes, so verification must fail.
        let last = id.cert.len() - 1;
        id.cert[last] ^= 0x01;
        assert!(verify_identity(&id, &sig).is_err());
    }

    #[test]
    fn tampered_timestamp() {
        let key_cert = create_key_cert("alice");
        let (mut id, sig) = sign_identity(&key_cert).unwrap();
        id.timestamp = id.timestamp.wrapping_add(1);
        assert!(verify_identity(&id, &sig).is_err());
    }

    #[test]
    fn stale_timestamp() {
        let key_cert = create_key_cert("alice");
        let (mut id, _sig) = sign_identity(&key_cert).unwrap();
        id.timestamp = id.timestamp.saturating_sub(MAX_TIMESTAMP_OFFSET + 1);
        let sig = resign(&id, &key_cert);
        assert!(verify_identity(&id, &sig).is_err());
    }

    #[test]
    fn wrong_key() {
        // Sign with alice's private key but present bob's cert in the
        // identity. Verification must fail because bob's pubkey
        // doesn't match alice's signature.
        let alice = create_key_cert("alice");
        let bob = create_key_cert("bob");
        let (mut id, sig) = sign_identity(&alice).unwrap();
        id.cert = bob.cert_der.clone();
        assert!(verify_identity(&id, &sig).is_err());
    }
}
