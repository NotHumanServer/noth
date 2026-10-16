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
    #[cfg(feature = "provider-openai")]
    noth_provider_openai::register(&mut reg);
    #[cfg(feature = "provider-anthropic")]
    noth_provider_anthropic::register(&mut reg);
    #[cfg(feature = "provider-google")]
    noth_provider_google::register(&mut reg);
    #[cfg(feature = "worker")]
    noth_worker::register(&mut reg);
    #[cfg(feature = "compactor")]
    noth_compactor::register(&mut reg);
    #[cfg(feature = "tool-web-search")]
    noth_tool_web_search::register(&mut reg);
    #[cfg(feature = "tool-web-fetch")]
    noth_tool_web_fetch::register(&mut reg);
    #[cfg(feature = "tool-web-agent")]
    noth_tool_web_agent::register(&mut reg);
    #[cfg(feature = "tool-shell")]
    noth_tool_shell::register(&mut reg);
    #[cfg(feature = "tool-discord")]
    noth_tool_discord::register(&mut reg);
    #[cfg(feature = "command-context")]
    noth_command_context::register(&mut reg);
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
