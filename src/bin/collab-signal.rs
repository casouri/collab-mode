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
