use std::{fs, path::PathBuf};
use webrtc_dtls::crypto::CryptoPrivateKey;

use crate::error::CollabResult;

#[derive(Debug, Clone)]
pub struct ConfigManager {
    config_location: Option<String>,
}

impl ConfigManager {
    pub fn new(config_location: Option<String>) -> ConfigManager {
        ConfigManager { config_location }
    }

    /// Either load or create keys and a certificate for `uuid` in
    /// standard (XDG) config location. The subject alt names of the
    /// certificate would be `uuid`.
    pub fn get_key_and_cert(
        &self,
        uuid: String,
    ) -> CollabResult<(rcgen::KeyPair, rcgen::Certificate)> {
        let xdg_dirs = xdg::BaseDirectories::with_prefix("collab-mode/secrets").unwrap();
        let key_file = xdg_dirs.place_config_file("key.pem")?;
        let ca_file = xdg_dirs.place_config_file(format!("{uuid}.cert"))?;

        let key_pair = if key_file.exists() {
            let key_string = std::fs::read_to_string(key_file)?;
            rcgen::KeyPair::from_pem(&key_string)?
        } else {
            let key_pair = rcgen::KeyPair::generate(&rcgen::PKCS_ECDSA_P256_SHA256)?;
            fs::write(key_file, key_pair.serialize_pem())?;
            key_pair
        };

        let key_copy = rcgen::KeyPair::from_der(&key_pair.serialize_der()).unwrap();

        let ca_cert = if ca_file.exists() {
            let ca_string = std::fs::read_to_string(ca_file)?;
            let params = rcgen::CertificateParams::from_ca_cert_pem(&ca_string, key_pair)?;
            rcgen::Certificate::from_params(params)?
        } else {
            let mut params = rcgen::CertificateParams::new(vec![uuid]);
            params.key_pair = Some(key_pair);
            let ca_cert = rcgen::Certificate::from_params(params)?;
            fs::write(ca_file, ca_cert.serialize_pem()?)?;
            ca_cert
        };

        Ok((key_copy, ca_cert))
    }
}
