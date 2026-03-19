pub mod serve;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "ccs",
    about = "Claude Code Switch - API proxy for routing Claude Code traffic"
)]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Start the proxy server
    Serve {
        /// Listen address (e.g., 127.0.0.1:8080)
        #[arg(long)]
        listen: Option<String>,
    },
}
