use clap::Parser;

use ccs::cli::{Cli, Commands};

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        None => {
            // TUI mode — no tracing output to avoid corrupting the display
            ccs::tui::run_tui()
        }
        Some(Commands::Serve { listen }) => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::from_default_env()
                        .add_directive("ccs=info".parse().unwrap()),
                )
                .with_target(false)
                .init();
            ccs::cli::serve::run(listen).await
        }
    };

    if let Err(e) = result {
        eprintln!("{}: {e}", colored::Colorize::red("Error"));
        std::process::exit(1);
    }
}
