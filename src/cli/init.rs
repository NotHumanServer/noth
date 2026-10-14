use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result};
use noth_core::config::{Paths, template_config, template_env, template_profile};

use super::create_private_dir;

pub fn run(cfg: Option<&Path>, non_interactive: bool) -> Result<ExitCode> {
    if !non_interactive {
        println!("対話モードは未実装です。--non-interactive として雛形を生成します");
    }
    let p = Paths::resolve(cfg)?;
    for dir in [
        p.home.clone(),
        p.home.join("profiles"),
        p.home.join("data"),
        p.home.join("work"),
        p.logs_dir(),
    ] {
        create_private_dir(&dir)?;
    }
    write_new(&p.config, template_config(), 0o644)?;
    // シークレットを置くので他ユーザーから読めないようにする
    write_new(&p.env, template_env(), 0o600)?;
    write_new(&p.profile("default"), template_profile(), 0o644)?;

    println!();
    println!("次の手順:");
    println!(
        "  1. {} に Bot トークンや API キーを記入する",
        p.env.display()
    );
    println!(
        "  2. {} の guild_id と allowed_users を設定する",
        p.config.display()
    );
    println!("  3. `noth doctor` で設定を検査する");
    Ok(ExitCode::SUCCESS)
}

fn write_new(path: &Path, content: &str, mode: u32) -> Result<()> {
    let opened = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path);
    match opened {
        Ok(mut f) => {
            f.write_all(content.as_bytes())?;
            println!("作成: {}", path.display());
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            println!("スキップ(既存): {}", path.display());
        }
        Err(e) => return Err(e).with_context(|| format!("{} を書けません", path.display())),
    }
    Ok(())
}
