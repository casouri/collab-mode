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
    let db_path = xdg_dirs.place_config_file("collab-signal.sqlite3")?;

    match &cli.command {
        Some(Commands::Run { port }) => {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(signaling::server::run_signaling_server(
                &format!("0.0.0.0:{port}"),
                &db_path,
            ))?;
            Ok(())
        }
        _ => {
            todo!()
        }
    }
}
