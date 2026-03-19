use colored::Colorize;

use crate::config::load_config;
use crate::error::Result;
use crate::proxy;

pub async fn run(listen: Option<String>) -> Result<()> {
    let mut config = load_config()?;

    if let Some(addr) = listen {
        config.listen = addr;
    }

    if config.providers.is_empty() {
        eprintln!(
            "{} No providers configured. Run {} to add one.",
            "Warning:".yellow(),
            "ccs provider add".cyan()
        );
    } else if let Ok((name, provider)) = config.current_provider() {
        eprintln!(
            "{} {} [{}]",
            "Provider:".green(),
            name.cyan(),
            provider.api_format.to_string().dimmed()
        );
    }

    eprintln!("{} {}", "Listening:".green(), config.listen.cyan());
    eprintln!(
        "{} Set {} to use this proxy",
        "Tip:".blue(),
        format!("ANTHROPIC_BASE_URL=http://{}", config.listen).cyan()
    );

    proxy::start_server(config).await
}
