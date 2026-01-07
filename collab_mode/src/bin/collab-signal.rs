use clap::{Parser, Subcommand};
use collab_mode::signaling;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the signaling server
    Run {
        /// Port the server runs on.
        #[arg(long, default_value_t = 8822)]
        port: u16,

        /// Persist client certificates to database. If false, only
        /// check active connections for ID conflicts.
        #[arg(long, default_value_t = false)]
        persisted: bool,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match &cli.command {
        Some(Commands::Run { port, persisted }) => {
            let db_path = if *persisted {
                let xdg_dirs = xdg::BaseDirectories::with_prefix("collab-signal");
                Some(xdg_dirs.place_data_file("collab-signal.sqlite3")?)
            } else {
                None
            };
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(signaling::server::run_signaling_server(
                &format!("0.0.0.0:{port}"),
                db_path.as_deref(),
            ))?;
            Ok(())
        }
        _ => {
            todo!()
        }
    }
}
