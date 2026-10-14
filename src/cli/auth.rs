use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;

#[cfg(feature = "provider-openai")]
pub async fn codex(cfg: Option<&Path>) -> Result<ExitCode> {
    let store = super::paths(cfg)?.codex_auth();
    if let Some(dir) = store.parent() {
        super::create_private_dir(dir)?;
    }
    noth_provider_openai::codex_auth::login(&store, |s| println!("{s}"))
        .await
        .map_err(|e| anyhow::anyhow!(super::message(&e)))?;
    println!(
        "ログインしました。トークンを {} に保存しました",
        store.display()
    );
    Ok(ExitCode::SUCCESS)
}

#[cfg(not(feature = "provider-openai"))]
pub async fn codex(_cfg: Option<&Path>) -> Result<ExitCode> {
    anyhow::bail!(
        "このバイナリには provider-openai が組み込まれていません。`cargo build --release --features provider-openai` でビルドしてください"
    )
}
