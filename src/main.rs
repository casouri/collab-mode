use clap::{Parser, Subcommand};
use collab_mode::jsonrpc;

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
        // /// Port used by the file server.
        // #[arg(long)]
        // #[arg(default_value_t = 7702)]
        // server_port: u16,
    },
}

fn main() -> anyhow::Result<()> {
    env_logger::init();

    let cli = Cli::parse();

    match &cli.command {
        Some(Commands::Run {
            socket,
            socket_port,
        }) => {
            let runtime = tokio::runtime::Runtime::new().unwrap();

            if !socket {
                return jsonrpc::run_stdio(runtime);
            } else {
                return jsonrpc::run_socket(&format!("localhost:{}", socket_port), runtime);
            }
        }
        _ => {
            todo!()
        }
    }
}
