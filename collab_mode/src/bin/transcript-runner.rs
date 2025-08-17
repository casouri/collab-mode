use clap::{Parser, Subcommand};
use collab_mode::transcript_runner::{list_transcripts, display_transcript, get_transcripts_dir, run_single_transcript};

#[derive(Parser)]
#[command(name = "transcript-runner")]
#[command(about = "View and manage transcript tests for collab-mode")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// List all available transcript tests
    List,
    /// Display a specific transcript test
    Show {
        /// Name of the transcript file (with or without .txt extension)
        transcript: String,
    },
    /// Run a specific transcript test
    Test {
        /// Name of the transcript file (with or without .txt extension)
        transcript: String,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    
    match cli.command {
        Commands::List => {
            let transcripts = list_transcripts()?;
            
            if transcripts.is_empty() {
                println!("No transcript tests found.");
                println!("Transcript files should be placed in: src/server/transcripts/");
            } else {
                println!("\nAvailable transcript tests:\n");
                for (i, name) in transcripts.iter().enumerate() {
                    // Try to read the test name from the file
                    let path = get_transcripts_dir().join(name);
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        if let Some(line) = content.lines().find(|l| l.starts_with("Name:")) {
                            let test_name = line.trim_start_matches("Name:").trim();
                            println!("  {}. {} - {}", i + 1, name, test_name);
                        } else {
                            println!("  {}. {}", i + 1, name);
                        }
                    } else {
                        println!("  {}. {}", i + 1, name);
                    }
                }
                println!("\nView a test with: cargo run --bin transcript-runner show <filename>");
                println!("Run a test with: cargo run --bin transcript-runner --features test-runner test <filename>");
                println!("Run all tests with: cargo test --lib server::transcript_tests");
            }
            Ok(())
        }
        Commands::Show { transcript } => {
            display_transcript(&transcript)
        }
        Commands::Test { transcript } => {
            // Initialize logging
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
                )
                .init();
                
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(async {
                match run_single_transcript(&transcript).await {
                    Ok(_) => {
                        println!("\n[PASS] Test completed successfully");
                        Ok(())
                    }
                    Err(e) => {
                        eprintln!("\n[FAIL] Test failed: {}", e);
                        std::process::exit(1);
                    }
                }
            })
        }
    }
}