use anyhow::Context;
use clap::{Parser, Subcommand};
use collab_mode::{config_man, editor_receptor, jsonrpc, server::Server};
use uuid::Uuid;

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
        #[arg(long, short = 'p')]
        #[arg(default_value_t = 7701)]
        socket_port: u16,
        #[arg(long)]
        config: Option<String>,
        #[arg(long)]
        profile: Option<String>,
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

            // Determine host_id with priority: config file > generate UUID v4
            let host_id = if let Some(config_id) = config_man.config().host_id.clone() {
                config_id
            } else {
                Uuid::new_v4().to_string()
            };

            let mut server = Server::new(host_id, config_man)?;
            let (server_in_tx, server_in_rx) = tokio::sync::mpsc::channel(32);
            let (server_out_tx, server_out_rx) = tokio::sync::mpsc::channel(32);
            let port = socket_port.clone();

            let _ = runtime.spawn(async move {
                let res = server.run(server_out_tx, server_in_rx).await;
                if let Err(err) = res {
                    panic!("Server exited with error: {:#?}", err);
                }
            });

            if !socket {
                editor_receptor::run_stdio(server_in_tx, server_out_rx);
            } else {
                let res = editor_receptor::run_socket(
                    &format!("localhost:{}", port),
                    server_in_tx,
                    server_out_rx,
                );
                if let Err(err) = res {
                    panic!("Failed to listen on local port: {:#?}", err);
                }
            }
            Ok(())
        }
        _ => {
            panic!("Unsupported command");
        }
    }
}
