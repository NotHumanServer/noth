use clap::Parser;
use noth_fakecord::cli::{self, Cli, Command};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Serve { port, snapshot } => noth_fakecord::serve(port, snapshot).await,
        _ => cli::run(cli).await,
    }
}
