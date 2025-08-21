
use anyhow::Context;
use clap::{Parser, Subcommand};
use collab_mode::{config_man, jsonrpc};

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the collab process
    Run {
        /// Use this flag to use socket instead of stdio between the
        /// editor and collab process.
        #[arg(long)]
        socket: bool,
        /// Port used by the socket.
        #[arg(long)]
        #[arg(default_value_t = 7701)]
        socket_port: u16,
        #[arg(long)]
        config: Option<String>,
        #[arg(long)]
        profile: Option<String>,
        // /// Port used by the file server.
        // #[arg(long)]
        // #[arg(default_value_t = 7702)]
        // server_port: u16,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match &cli.command {
        Some(Commands::Run {
            socket,
            socket_port,
            config,
            profile,
        }) => {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let config_location = if let Some(config_path) = config {
                Some(
                    expanduser::expanduser(config_path)
                        .with_context(|| "Can't expand filename to absolute path".to_string())?,
                )
            } else {
                None
            };
            let config_man = config_man::ConfigManager::new(config_location, profile.to_owned())?;

            if !socket {
                jsonrpc::run_stdio(runtime, config_man)
            } else {
                jsonrpc::run_socket(
                    &format!("localhost:{}", socket_port),
                    runtime,
                    config_man,
                )
            }
        }
        _ => {
            panic!("Unsupported command");
        }
    }
}
