use crate::error::CollabResult;
use crate::types::*;
use crate::{error::CollabError, signaling::CertDerHash};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;

pub struct KeyCert {
    /// The private key. I don’t know if the [rcgen::KeyPair] is the
    /// same story as [rcgen::Certificate] (see below), but let’s just
    /// do the same thing and store the DER instead of
    /// [rcgen::KeyPair].
    pub key_der: Vec<u8>,
    /// The certificate DER. NOTE: DO NOT use a rcgen::Certificate and
    /// call [rcgen::Certificate::serialize_der] when you need a DER.
    /// Despite its name [rcgen::Certificate::serialize_der] actually
    /// _generates_ a new certificate every time you call it, which
    /// means it’s return value are different every time it’s invoked.
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

#[derive(Debug, Clone, Eq, PartialEq, Deserialize, Serialize)]
pub struct ConfigProject {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub enum AcceptMode {
    /// Accept all hosts, even those not in the trusted hosts list.
    All,
    /// Accept only hosts in the trusted hosts list.
    TrustedOnly,
}

impl Default for AcceptMode {
    fn default() -> Self {
        AcceptMode::All
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[non_exhaustive]
pub struct Permission {
    pub write: bool,
    pub create: bool,
    pub delete: bool,
}

impl Default for Permission {
    fn default() -> Self {
        Permission {
            write: true,
            create: true,
            delete: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// Projects that are shared by default.
    #[serde(default = "Vec::new")]
    pub projects: Vec<ConfigProject>,
    // We're really only checking the hash. the ServerId is for user
    // to know which hash belongs to which peer.
    #[serde(default = "HashMap::new")]
    pub trusted_hosts: HashMap<ServerId, String>,
    #[serde(default = "AcceptMode::default")]
    pub accept_mode: AcceptMode,
    // The host id of this server. User can put it in the config if
    // they run the server in headless mode and not connect an editor
    // to it.
    pub host_id: Option<ServerId>,
    #[serde(default = "HashMap::new")]
    pub permission: HashMap<ServerId, Permission>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            projects: Vec::new(),
            trusted_hosts: HashMap::new(),
            accept_mode: AcceptMode::default(),
            host_id: None,
            permission: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConfigManager {
    config_dir: Option<PathBuf>,
    base_dirs: xdg::BaseDirectories,
    config: Config,
}

impl ConfigManager {
    pub fn new(
        config_location: Option<PathBuf>,
        profile: Option<String>,
    ) -> anyhow::Result<ConfigManager> {
        let base_dirs = if let Some(profile) = profile {
            xdg::BaseDirectories::with_profile("collab-mode", profile)
        } else {
            xdg::BaseDirectories::with_prefix("collab-mode")
        };

        let config = Self::load_config(&config_location, &base_dirs)?;

        Ok(ConfigManager {
            config_dir: config_location,
            base_dirs,
            config,
        })
    }

    /// Get the path where the config file should be located
    pub fn get_config_file_path(&self) -> anyhow::Result<PathBuf> {
        if let Some(config_dir) = &self.config_dir {
            // If config_location is provided, use config.json there
            Ok(config_dir.join("config.json"))
        } else {
            // Otherwise, use XDG base directories
            self.base_dirs
                .place_config_file("config.json")
                .map_err(|err| anyhow::anyhow!("Failed to determine config file location: {}", err))
        }
    }

    fn load_config(
        config_location: &Option<PathBuf>,
        base_dirs: &xdg::BaseDirectories,
    ) -> anyhow::Result<Config> {
        let config_file = if let Some(config_dir) = config_location {
            // If config_location is provided, look for config.json there
            config_dir.join("config.json")
        } else {
            // Otherwise, try to find config.json in XDG base directories
            match base_dirs.find_config_file("config.json") {
                Some(path) => path,
                None => {
                    // No config file found, return default config
                    return Ok(Config::default());
                }
            }
        };

        // Check if the config file exists
        if !config_file.exists() {
            // Return default config if file doesn't exist
            return Ok(Config::default());
        }

        // Read and parse the config file
        let config_content = std::fs::read_to_string(&config_file).map_err(|err| {
            anyhow::anyhow!("Failed to read config file {:?}: {}", config_file, err)
        })?;

        let config: Config = serde_json::from_str(&config_content).map_err(|err| {
            anyhow::anyhow!("Failed to parse config file {:?}: {}", config_file, err)
        })?;

        tracing::info!(
            "Loaded config from {} : {:?}",
            &config_file.to_string_lossy(),
            &config
        );

        Ok(config)
    }

    /// Either load or create keys and a certificate for `uuid` in
    /// standard (XDG) config location. The subject alt names of the
    /// certificate would be `uuid`.
    pub fn get_key_and_cert(&self, uuid: String) -> CollabResult<KeyCert> {
        if let Some(config_dir) = &self.config_dir {
            let _ = std::fs::create_dir_all(config_dir.join("secrets"));
        }
        let key_file = if let Some(config_dir) = &self.config_dir {
            config_dir.join(format!("secrets/{uuid}.key.pem"))
        } else {
            self.base_dirs
                .place_config_file(format!("{uuid}.key.pem"))
                .map_err(|err| {
                    CollabError::Fatal(format!("Failed to find/create private key: {:#?}", err))
                })?
        };
        let cert_file = if let Some(config_dir) = &self.config_dir {
            config_dir.join(format!("secrets/{uuid}.cert.pem"))
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
            let _ = std::fs::create_dir_all(config_dir.join("var"));
            config_dir.join("var/backups.sqlite3")
        } else {
            self.base_dirs.place_data_file("backups.sqlite3")?
        };

        let conn = rusqlite::Connection::open(db_file)?;
        Ok(conn)
    }

    /// Get a copy of the loaded configuration
    pub fn config(&self) -> Config {
        self.config.clone()
    }

    /// Check if a host has write permission
    /// Always returns true for ourselves.
    pub fn write_allowed(&self, host_id: &ServerId) -> bool {
        if Some(host_id) == self.config.host_id.as_ref() {
            return true;
        }

        if let Some(permission) = self.config.permission.get(host_id) {
            return permission.write;
        }

        Permission::default().write
    }

    /// Check if a host has create permission
    /// Always returns true for ourselves.
    pub fn create_allowed(&self, host_id: &ServerId) -> bool {
        // Always allow ourselves.
        if Some(host_id) == self.config.host_id.as_ref() {
            return true;
        }

        if let Some(permission) = self.config.permission.get(host_id) {
            return permission.create;
        }

        Permission::default().create
    }

    /// Check if a host has delete permission
    /// Always returns true for ourselves
    pub fn delete_allowed(&self, host_id: &ServerId) -> bool {
        if Some(host_id) == self.config.host_id.as_ref() {
            return true;
        }

        if let Some(permission) = self.config.permission.get(host_id) {
            return permission.delete;
        }

        Permission::default().delete
    }

    /// Add a trusted host to the configuration
    pub fn add_trusted_host(&mut self, server_id: ServerId, cert_hash: String) {
        self.config.trusted_hosts.insert(server_id, cert_hash);
    }

    /// Override with a new config and save to disk
    pub fn replace_and_save(&mut self, new_config: Config) -> anyhow::Result<()> {
        // Override the internal config with the new one
        self.config = new_config;
        // Save to disk
        self.save_config()
    }

    /// Write the current configuration to disk
    pub fn save_config(&self) -> anyhow::Result<()> {
        let config_file = self.get_config_file_path()?;

        // Ensure the parent directory exists
        if let Some(parent) = config_file.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| anyhow::anyhow!("Failed to create config directory: {}", err))?;
        }

        // Serialize the config to pretty JSON
        let config_json = serde_json::to_string_pretty(&self.config)
            .map_err(|err| anyhow::anyhow!("Failed to serialize config: {}", err))?;

        // Write to file
        std::fs::write(&config_file, config_json).map_err(|err| {
            anyhow::anyhow!("Failed to write config file {:?}: {}", config_file, err)
        })?;

        Ok(())
    }
}
