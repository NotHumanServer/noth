use std::fs;
use std::io::{IsTerminal, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow, bail};
use inquire::validator::Validation;
use inquire::{
    Confirm, CustomUserError, InquireError, Password, PasswordDisplayMode, Select, Text,
};
use noth_core::config::{Paths, template_config, template_env, template_profile};
use noth_core::registry::{KNOWN_MODULES, ModuleKind};
use toml_edit::{Array, Value};

use super::{create_private_dir, doctor, edit};

pub async fn run(cfg: Option<&Path>, non_interactive: bool) -> Result<ExitCode> {
    let p = Paths::resolve(cfg)?;
    if non_interactive {
        create_dirs(&p)?;
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
        return Ok(ExitCode::SUCCESS);
    }

    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        bail!(
            "対話入力できない環境です(標準入力が端末ではありません)。`noth init --non-interactive` で雛形を作り、config.toml と .env を編集してください"
        );
    }
    interactive(cfg, &p).await
}

fn create_dirs(p: &Paths) -> Result<()> {
    for dir in [
        p.home.clone(),
        p.home.join("profiles"),
        p.home.join("data"),
        p.home.join("work"),
        p.logs_dir(),
    ] {
        create_private_dir(&dir)?;
    }
    Ok(())
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

/// 確認のうえ上書きする。既存ファイルの権限は `mode` に揃える
fn write_overwrite(path: &Path, content: &str, mode: u32) -> Result<()> {
    let existed = path.exists();
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)
        .with_context(|| format!("{} を書けません", path.display()))?;
    f.write_all(content.as_bytes())?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    println!(
        "{}: {}",
        if existed { "上書き" } else { "作成" },
        path.display()
    );
    Ok(())
}

/// 対話で決めた内容。`render` が config.toml と .env の本文にする
struct Plan {
    router_module: &'static str,
    provider_module: &'static str,
    sets: Vec<(String, Value)>,
    env: Vec<(String, String)>,
}

/// 雛形を土台に、選択に応じた差分だけを toml_edit で当てる(雛形のコメントを残すため)
fn render(plan: &Plan, is_built_in: impl Fn(&str) -> bool) -> Result<(String, String)> {
    let mut doc: toml_edit::DocumentMut = template_config().parse()?;
    edit::replace_module(&mut doc, "router-systemone", plan.router_module)?;
    edit::replace_module(&mut doc, "provider-anthropic", plan.provider_module)?;
    for name in edit::enabled_list(&doc) {
        if !is_built_in(&name) {
            edit::disable_module(&mut doc, &name)?;
        }
    }
    for (key, value) in &plan.sets {
        edit::set_key(&mut doc, key, value.clone())?;
    }
    let mut env = template_env().to_string();
    for (key, value) in &plan.env {
        env = edit::env_set(&env, key, value)?;
    }
    Ok((doc.to_string(), env))
}

fn parse_ids(input: &str) -> Result<Vec<String>, String> {
    let ids: Vec<String> = input
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    match ids.iter().find(|id| !is_snowflake(id)) {
        Some(bad) => Err(format!("ユーザー ID は 17〜20 桁の数字です: `{bad}`")),
        None => Ok(ids),
    }
}

fn is_snowflake(s: &str) -> bool {
    (17..=20).contains(&s.len()) && s.chars().all(|c| c.is_ascii_digit())
}

fn validate_guild(s: &str) -> Result<Validation, CustomUserError> {
    Ok(if is_snowflake(s.trim()) {
        Validation::Valid
    } else {
        Validation::Invalid("17〜20 桁の数字で入力してください".into())
    })
}

fn validate_users(s: &str) -> Result<Validation, CustomUserError> {
    Ok(match parse_ids(s) {
        Ok(ids) if ids.is_empty() => Validation::Invalid("1人以上入力してください".into()),
        Ok(_) => Validation::Valid,
        Err(m) => Validation::Invalid(m.into()),
    })
}

fn validate_nonempty(s: &str) -> Result<Validation, CustomUserError> {
    Ok(if s.trim().is_empty() {
        Validation::Invalid("入力してください".into())
    } else {
        Validation::Valid
    })
}

fn ask<T>(r: Result<T, InquireError>) -> Result<T> {
    r.map_err(|e| match e {
        InquireError::OperationCanceled | InquireError::OperationInterrupted => {
            anyhow!("中止しました")
        }
        other => anyhow!(other),
    })
}

fn choose(msg: &str, options: &[&str]) -> Result<usize> {
    Ok(ask(Select::new(msg, options.to_vec()).raw_prompt())?.index)
}

fn validate_env_value(s: &str) -> Result<Validation, CustomUserError> {
    Ok(match edit::env_value(s) {
        Ok(_) => Validation::Valid,
        Err(e) => Validation::Invalid(e.to_string().into()),
    })
}

/// .env に書く値。書けない値(`"` と `'` の両方を含むなど)は最後の書き込みで失敗しないよう、入力時に弾く
fn env_text(msg: &str) -> Result<String> {
    Ok(ask(Text::new(msg)
        .with_validator(validate_nonempty)
        .with_validator(validate_env_value)
        .prompt())?
    .trim()
    .to_string())
}

fn secret(msg: &str) -> Result<String> {
    ask(Password::new(msg)
        .with_validator(validate_env_value)
        .with_display_mode(PasswordDisplayMode::Masked)
        .without_confirmation()
        .with_help_message(
            "空のままにすると .env の値を空にします(後から `noth config set` で設定できます)",
        )
        .prompt())
}

fn text(msg: &str, default: Option<&str>) -> Result<String> {
    let mut t = Text::new(msg).with_validator(validate_nonempty);
    if let Some(d) = default {
        t = t.with_default(d);
    }
    Ok(ask(t.prompt())?.trim().to_string())
}

fn str_value(s: &str) -> Value {
    Value::from(s)
}

fn pick_router(reg: &noth_core::registry::Registry, plan: &mut Plan) -> Result<()> {
    let mut options: Vec<(&str, &'static str, &str)> = Vec::new();
    if reg.is_built_in("router-systemone") {
        options.push(("jev(router-systemone、既定)", "router-systemone", "jev"));
        options.push((
            "clef(router-systemone、Cloudflare Workers AI)",
            "router-systemone",
            "clef",
        ));
    }
    if reg.is_built_in("router-mention") {
        options.push((
            "mention(router-mention、呼び名だけで判定)",
            "router-mention",
            "",
        ));
    }
    if options.is_empty() {
        bail!("このバイナリには router module が組み込まれていません");
    }
    let labels: Vec<&str> = options.iter().map(|o| o.0).collect();
    let (_, module, preset) = options[choose("router を選んでください", &labels)?];
    plan.router_module = module;
    match preset {
        "jev" => plan
            .env
            .push(("TYPESAFE_API_KEY".into(), secret("TYPESAFE_API_KEY(Jev)")?)),
        "clef" => {
            plan.sets.extend([
                ("router.systemone.preset".into(), str_value("clef")),
                ("router.systemone.model".into(), str_value("clef-flash")),
                (
                    "router.systemone.api_key".into(),
                    str_value("${CF_API_TOKEN}"),
                ),
                (
                    "router.systemone.account_id".into(),
                    str_value("${CF_ACCOUNT_ID}"),
                ),
            ]);
            plan.env
                .push(("CF_API_TOKEN".into(), secret("CF_API_TOKEN")?));
            plan.env
                .push(("CF_ACCOUNT_ID".into(), env_text("CF_ACCOUNT_ID")?));
        }
        _ => {}
    }
    Ok(())
}

fn pick_provider(reg: &noth_core::registry::Registry, plan: &mut Plan) -> Result<()> {
    let modules: Vec<&'static str> = KNOWN_MODULES
        .iter()
        .filter(|(n, k)| *k == ModuleKind::Provider && reg.is_built_in(n))
        .map(|(n, _)| *n)
        .collect();
    if modules.is_empty() {
        bail!("このバイナリには provider module が組み込まれていません");
    }
    let module = if modules.len() == 1 {
        modules[0]
    } else {
        modules[choose("provider を選んでください", &modules)?]
    };
    plan.provider_module = module;
    let section = format!("provider.{}", module.trim_start_matches("provider-"));
    let key = |k: &str| format!("{section}.{k}");

    match module {
        "provider-anthropic" => {
            let model = text("モデル名", Some("claude-opus-5-5"))?;
            plan.sets.push((key("model"), str_value(&model)));
            api_key_or_opencode(plan, &key("api_key"), "ANTHROPIC_API_KEY", &section)?;
        }
        "provider-openai" => {
            let model = text("モデル名", None)?;
            let format = ["chat", "responses"][choose("API 形式", &["chat", "responses"])?];
            plan.sets.push((key("model"), str_value(&model)));
            plan.sets.push((key("format"), str_value(format)));
            api_key_or_opencode(plan, &key("api_key"), "OPENAI_API_KEY", &section)?;
        }
        "provider-google" => {
            let model = text("モデル名", None)?;
            plan.sets.push((key("model"), str_value(&model)));
            let preset = choose(
                "接続先",
                &[
                    "Google AI Studio(API キー)",
                    "agent-platform(サービスアカウント)",
                ],
            )?;
            if preset == 0 {
                plan.sets
                    .push((key("api_key"), str_value("${GOOGLE_AI_STUDIO_API_KEY}")));
                plan.env.push((
                    "GOOGLE_AI_STUDIO_API_KEY".into(),
                    secret("GOOGLE_AI_STUDIO_API_KEY")?,
                ));
            } else {
                let project = text("GCP プロジェクト ID", None)?;
                let location = text("ロケーション", Some("us-central1"))?;
                let creds = env_text("サービスアカウント JSON のパス")?;
                plan.sets.extend([
                    (key("preset"), str_value("agent-platform")),
                    (key("project"), str_value(&project)),
                    (key("location"), str_value(&location)),
                    (
                        key("credentials_file"),
                        str_value("${GOOGLE_APPLICATION_CREDENTIALS}"),
                    ),
                ]);
                plan.env
                    .push(("GOOGLE_APPLICATION_CREDENTIALS".into(), creds));
            }
        }
        other => bail!("未対応の provider です: {other}"),
    }
    Ok(())
}

fn api_key_or_opencode(
    plan: &mut Plan,
    api_key_path: &str,
    var: &str,
    section: &str,
) -> Result<()> {
    let preset = choose(
        "接続先",
        &[&format!("標準({var})"), "opencode-go(OPENCODE_GO_API_KEY)"],
    )?;
    if preset == 0 {
        plan.env.push((var.into(), secret(var)?));
    } else {
        plan.sets
            .push((format!("{section}.preset"), str_value("opencode-go")));
        plan.sets
            .push((api_key_path.into(), str_value("${OPENCODE_GO_API_KEY}")));
        plan.env
            .push(("OPENCODE_GO_API_KEY".into(), secret("OPENCODE_GO_API_KEY")?));
    }
    Ok(())
}

async fn interactive(cfg: Option<&Path>, p: &Paths) -> Result<ExitCode> {
    let reg = crate::registry();
    println!("noth の初期設定を始めます(中止は Ctrl-C)");

    let overwrite = |path: &Path| -> Result<bool> {
        if !path.exists() {
            return Ok(true);
        }
        ask(Confirm::new(&format!(
            "{} は既にあります。上書きしますか?",
            path.display()
        ))
        .with_default(false)
        .prompt())
    };
    if !overwrite(&p.config)? || !overwrite(&p.env)? {
        bail!(
            "既存の設定は上書きしません。個別の変更は `noth config set` / `noth module enable` を使ってください"
        );
    }
    let write_profile = overwrite(&p.profile("default"))?;

    let mut plan = Plan {
        router_module: "router-systemone",
        provider_module: "provider-anthropic",
        sets: Vec::new(),
        env: Vec::new(),
    };

    println!();
    println!("1. Discord Bot");
    println!(
        "   Developer Portal の Bot ページで MESSAGE_CONTENT 特権インテントを有効にしてください。"
    );
    println!("   メンバー一覧の取得に GUILD_MEMBERS 特権インテントも必要になる場合があります。");
    plan.env
        .push(("DISCORD_BOT_TOKEN".into(), secret("Bot トークン")?));

    // Bot の参加 guild 一覧は Discord REST で引けるが、binary は HTTP クライアントを持たず、
    // channel module も一覧取得を公開していない。init のためだけに公開面を増やさず手入力にする
    println!(
        "   Discord の開発者モードを有効にし、サーバー名とユーザーを右クリックして ID をコピーできます。"
    );
    let guild = ask(Text::new("guild ID(Bot が参加しているサーバー)")
        .with_validator(validate_guild)
        .prompt())?;
    plan.sets
        .push(("channel.discord.guild_id".into(), str_value(guild.trim())));
    let users = ask(
        Text::new("allowed_users(応答対象のユーザー ID、カンマか空白区切り)")
            .with_validator(validate_users)
            .prompt(),
    )?;
    let users = parse_ids(&users).map_err(|m| anyhow!(m))?;
    plan.sets.push((
        "channel.discord.allowed_users".into(),
        Value::Array(users.iter().map(|u| u.as_str()).collect::<Array>()),
    ));

    println!();
    println!("2. router");
    pick_router(&reg, &mut plan)?;

    println!();
    println!("3. provider");
    pick_provider(&reg, &mut plan)?;

    if ["tool-web-search", "tool-web-fetch", "tool-web-agent"]
        .iter()
        .any(|m| reg.is_built_in(m))
    {
        println!();
        println!("4. Web ツール(TinyFish)");
        plan.env
            .push(("TINYFISH_API_KEY".into(), secret("TINYFISH_API_KEY")?));
    }

    let (config_text, env_text) = render(&plan, |m| reg.is_built_in(m))?;

    println!();
    create_dirs(p)?;
    write_overwrite(&p.config, &config_text, 0o644)?;
    write_overwrite(&p.env, &env_text, 0o600)?;
    if write_profile {
        write_overwrite(&p.profile("default"), template_profile(), 0o644)?;
    }
    println!(
        "profile は {} を編集してキャラクターを調整できます",
        p.profile("default").display()
    );

    println!();
    println!("noth doctor を実行します");
    doctor::run(cfg).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use noth_core::config::Config;

    #[test]
    fn parse_ids_accepts_commas_and_spaces() {
        let ids = parse_ids("111111111111111111, 222222222222222222 333333333333333333").unwrap();
        assert_eq!(ids.len(), 3);
        assert!(parse_ids("").unwrap().is_empty());
        assert!(parse_ids("abc").is_err());
        assert!(parse_ids("123").is_err());
    }

    fn load(config: &str, env: &str) -> noth_core::Result<Config> {
        let dir = std::env::temp_dir().join(format!("noth-init-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.toml"), config).unwrap();
        fs::write(dir.join(".env"), env).unwrap();
        let c = Config::load(Paths::resolve(Some(&dir.join("config.toml"))).unwrap());
        fs::remove_dir_all(&dir).ok();
        c
    }

    #[test]
    fn render_applies_choices_and_keeps_template_comments() {
        let plan = Plan {
            router_module: "router-mention",
            provider_module: "provider-openai",
            sets: vec![
                (
                    "channel.discord.guild_id".into(),
                    str_value("123456789012345678"),
                ),
                (
                    "provider.openai.api_key".into(),
                    str_value("${OPENAI_API_KEY}"),
                ),
                ("provider.openai.model".into(), str_value("gpt-x")),
                ("provider.openai.format".into(), str_value("chat")),
            ],
            env: vec![
                ("DISCORD_BOT_TOKEN".into(), "tok".into()),
                ("OPENAI_API_KEY".into(), "sk".into()),
                ("TINYFISH_API_KEY".into(), "tf".into()),
            ],
        };
        let (config, env) = render(&plan, |_| true).unwrap();
        assert!(config.contains("サンドボックス"));
        assert!(config.contains("[provider.openai]"), "{config}");
        let c = load(&config, &env).unwrap();
        assert!(c.is_enabled("router-mention") && !c.is_enabled("router-systemone"));
        assert!(c.is_enabled("provider-openai") && !c.is_enabled("provider-anthropic"));
        assert!(env.contains("OPENAI_API_KEY=sk\n") && env.contains("TINYFISH_API_KEY=tf\n"));
    }

    #[test]
    fn render_drops_modules_that_are_not_built_in() {
        let plan = Plan {
            router_module: "router-systemone",
            provider_module: "provider-anthropic",
            sets: Vec::new(),
            env: Vec::new(),
        };
        let (config, _) = render(&plan, |m| !m.starts_with("tool-")).unwrap();
        let doc: toml_edit::DocumentMut = config.parse().unwrap();
        let list = edit::enabled_list(&doc);
        assert!(list.iter().all(|m| !m.starts_with("tool-")), "{list:?}");
        assert!(list.contains(&"worker".to_string()));
    }
}
