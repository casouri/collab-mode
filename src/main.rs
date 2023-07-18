use clap::{Parser, Subcommand};
use collab_mode::{collab_server, jsonrpc};

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the collab server
    Run {
        /// Use this flag to use socket instead of stdio between the
        /// editor and collab process.
        #[arg(long)]
        socket: bool,
        /// Use this flag to start a file server that allows the user
        /// to share files and allow other people to connect.
        #[arg(long)]
        server: bool,
        /// Port used by the socket.
        #[arg(long)]
        #[arg(default_value_t = 7701)]
        socket_port: u16,
        /// Port used by the file server.
        #[arg(long)]
        #[arg(default_value_t = 7702)]
        server_port: u16,
    },
}

fn main() -> anyhow::Result<()> {
    env_logger::init();

    let cli = Cli::parse();

    match &cli.command {
        Some(Commands::Run {
            socket,
            server,
            socket_port,
            server_port,
        }) => {
            let collab_server = collab_server::LocalServer::new();
            let (err_tx, err_rx) = tokio::sync::mpsc::channel(1);

            let runtime = tokio::runtime::Runtime::new().unwrap();

            let future = collab_server
                .clone()
                .start_grpc_server(*server_port, err_tx);
            if *server {
                runtime.spawn(async {
                    let res = future.await;
                    if let Err(err) = res {
                        log::error!("Error starting gRPC server: {:#}", err);
                    }
                });
            }
            if !socket {
                return jsonrpc::run_stdio(collab_server, err_rx, runtime);
            } else {
                return jsonrpc::run_socket(
                    &format!("localhost:{}", socket_port),
                    collab_server,
                    err_rx,
                    runtime,
                );
            }
        }
        _ => {
            todo!()
        }
    }
}
