mod cli;

use std::process::ExitCode;

use clap::FromArgMatches;
use noth_core::registry::Registry;

fn registry() -> Registry {
    #[allow(unused_mut)]
    let mut reg = Registry::default();
    #[cfg(feature = "channel-discord")]
    noth_channel_discord::register(&mut reg);
    #[cfg(feature = "router-systemone")]
    noth_router_systemone::register(&mut reg);
    #[cfg(feature = "router-mention")]
    noth_router_mention::register(&mut reg);
    #[cfg(feature = "provider-anthropic")]
    noth_provider_anthropic::register(&mut reg);
    reg
}

#[tokio::main]
async fn main() -> ExitCode {
    let matches = cli::command().get_matches();
    let parsed = match cli::Cli::from_arg_matches(&matches) {
        Ok(c) => c,
        Err(e) => e.exit(),
    };
    match cli::dispatch(parsed).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("エラー: {e:#}");
            ExitCode::from(1)
        }
    }
}
