use clap::{Parser, Subcommand};
use collab_mode::signaling;
use std::{fs, path::PathBuf};

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the signaling server
    Run {
        /// Port running the server runs on.
        #[arg(long)]
        #[arg(default_value_t = 8822)]
        port: u16,
    },
}

fn main() -> anyhow::Result<()> {
    env_logger::init();

    let cli = Cli::parse();

    let xdg_dirs = xdg::BaseDirectories::with_prefix("collab-signal").unwrap();
    let key_file = xdg_dirs.place_config_file("key.pem")?;
    let ca_file = xdg_dirs.place_config_file("ca.cert")?;

    let key_pair = if key_file.exists() {
        let key_string = std::fs::read_to_string(key_file)?;
        rcgen::KeyPair::from_pem(&key_string)?
    } else {
        let key_pair = rcgen::KeyPair::generate(&rcgen::PKCS_ECDSA_P256_SHA256)?;
        fs::write(key_file, key_pair.serialize_pem())?;
        key_pair
    };

    let ca_cert = if ca_file.exists() {
        let ca_string = std::fs::read_to_string(ca_file)?;
        let params = rcgen::CertificateParams::from_ca_cert_pem(&ca_string, key_pair)?;
        rcgen::Certificate::from_params(params)?
    } else {
        let mut params =
            rcgen::CertificateParams::new(vec!["collab-signal for live!!!".to_string()]);
        params.key_pair = Some(key_pair);
        let ca_cert = rcgen::Certificate::from_params(params)?;
        fs::write(ca_file, ca_cert.serialize_pem()?)?;
        ca_cert
    };

    match &cli.command {
        Some(Commands::Run { port }) => {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(signaling::server::run_signaling_server(&format!(
                "0.0.0.0:{port}"
            )))?;
            Ok(())
        }
        _ => {
            todo!()
        }
    }
}
