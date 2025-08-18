use std::path::PathBuf;

use crate::{error::CollabError, signaling::CertDerHash};
use sha2::{Digest, Sha256};

use crate::error::CollabResult;

pub struct KeyCert {
    /// The private key. I don't know if the [rcgen::KeyPair] is the
    /// same story as [rcgen::Certificate] (see below), but let's just
    /// do the same thing and store the DER instead of
    /// [rcgen::KeyPair].
    pub key_der: Vec<u8>,
    /// The certificate DER. NOTE: DO NOT use a rcgen::Certificate and
    /// call [rcgen::Certificate::serialize_der] when you need a DER.
    /// Despite its name [rcgen::Certificate::serialize_der] actually
    /// _generates_ a new certificate every time you call it, which
    /// means it's return value are different every time it's invoked.
    /// See https://github.com/rustls/rcgen/issues/62
    pub cert_der: Vec<u8>,
}

impl KeyCert {
    /// Return the certificate in DER format, hashed with SHA-256, and
    /// printed out fingerprint format: each byte in uppercase hex,
    /// separated by colons.
    pub fn cert_der_hash(&self) -> CertDerHash {
        hash_der(&self.cert_der)
    }

    /// Create a DTLS certificate.
    pub fn create_dtls_cert(&self) -> webrtc_dtls::crypto::Certificate {
        let dtls_cert = webrtc_dtls::crypto::Certificate {
            certificate: vec![rustls::Certificate(self.cert_der.clone())],
            private_key: webrtc_dtls::crypto::CryptoPrivateKey::from_key_pair(
                &rcgen::KeyPair::from_der(&self.key_der).unwrap(),
            )
            .unwrap(),
        };
        dtls_cert
    }
}

pub type ArcKeyCert = std::sync::Arc<KeyCert>;

/// Hash the binary DER file and return the hash in fingerprint
/// format: each byte in uppercase hex, separated by colons.
/// (ref:rcgen-cert-searlize)
pub fn hash_der(der: &[u8]) -> String {
    let hash = Sha256::digest(der);
    // Separate each byte with colon like webrtc does.
    let bytes: Vec<String> = hash.iter().map(|x| format!("{x:02x}")).collect();
    bytes.join(":").to_uppercase()
}

/// Return a clone of `key`.
pub fn clone_key(key: &rcgen::KeyPair) -> rcgen::KeyPair {
    let key_der = key.serialized_der();
    rcgen::KeyPair::from_der(&key_der).unwrap()
}

/// Return a freshly generated key and certificate for `name`.
pub fn create_key_cert(name: &str) -> KeyCert {
    let cert = rcgen::generate_simple_self_signed(vec![name.to_string()]).unwrap();
    let cert_der = cert.serialize_der().unwrap();
    let key_der = cert.get_key_pair().serialize_der();
    KeyCert { key_der, cert_der }
}

#[derive(Debug, Clone)]
pub struct ConfigManager {
    config_dir: Option<PathBuf>,
    base_dirs: xdg::BaseDirectories,
}

impl ConfigManager {
    pub fn new(config_location: Option<PathBuf>, profile: Option<String>) -> ConfigManager {
        ConfigManager {
            config_dir: config_location,
            base_dirs: if let Some(profile) = profile {
                xdg::BaseDirectories::with_profile("collab-mode", profile)
            } else {
                xdg::BaseDirectories::with_prefix("collab-mode")
            },
        }
    }

    /// Either load or create keys and a certificate for `uuid` in
    /// standard (XDG) config location. The subject alt names of the
    /// certificate would be `uuid`.
    pub fn get_key_and_cert(&self, uuid: String) -> CollabResult<KeyCert> {
        let key_file = if let Some(config_dir) = &self.config_dir {
            config_dir.join(format!("{uuid}.key.pem"))
        } else {
            self.base_dirs
                .place_config_file(format!("{uuid}.key.pem"))
                .map_err(|err| {
                    CollabError::Fatal(format!("Failed to find/create private key: {:#?}", err))
                })?
        };
        let cert_file = if let Some(config_dir) = &self.config_dir {
            config_dir.join(format!("{uuid}.cert.pem"))
        } else {
            self.base_dirs
                .place_config_file(format!("{uuid}.cert.pem"))
                .map_err(|err| {
                    CollabError::Fatal(format!("Failed to find/create certificate: {:#?}", err))
                })?
        };

        let (key_pair, key_der) = if key_file.exists() {
            let key_pem = std::fs::read_to_string(key_file).map_err(|err| {
                CollabError::Fatal(format!("Failed to read private key from disk: {:#?}", err))
            })?;
            let key_der = pem::parse(&key_pem)?.into_contents();
            let key_pair = rcgen::KeyPair::from_pem(&key_pem)?;
            (key_pair, key_der)
        } else {
            let key_pair = rcgen::KeyPair::generate(&rcgen::PKCS_ECDSA_P256_SHA256)?;
            let key_pem = key_pair.serialize_pem();
            let key_der = pem::parse(&key_pem).unwrap().into_contents();
            std::fs::write(key_file, key_pem).map_err(|err| {
                CollabError::Fatal(format!(
                    "Failed to save the private key to disk: {:#?}",
                    err
                ))
            })?;
            (key_pair, key_der)
        };

        let cert_der = if cert_file.exists() {
            let pem = std::fs::read_to_string(cert_file).map_err(|err| {
                CollabError::Fatal(format!("Failed to read the certificate file: {:#?}", err))
            })?;
            pem::parse(pem)?.into_contents()
        } else {
            // The alt subject name doesn't really matter, but let's
            // use the uuid because why not.
            let mut params = rcgen::CertificateParams::new(vec![uuid]);
            params.key_pair = Some(key_pair);
            let cert = rcgen::Certificate::from_params(params).unwrap();
            let cert_pem = cert.serialize_pem().unwrap();
            let cert_der = pem::parse(&cert_pem)?.into_contents();
            std::fs::write(cert_file, &cert_pem).map_err(|err| {
                CollabError::Fatal(format!(
                    "Failed to save the certificate to disk: {:#?}",
                    err
                ))
            })?;
            cert_der
        };

        Ok(KeyCert { key_der, cert_der })
    }

    pub fn get_db(&self) -> anyhow::Result<rusqlite::Connection> {
        let db_file = if let Some(config_dir) = &self.config_dir {
            config_dir.join("backups.sqlite3")
        } else {
            self.base_dirs.place_data_file("backups.sqlite3")?
        };

        let conn = rusqlite::Connection::open(db_file)?;
        Ok(conn)
    }
}
