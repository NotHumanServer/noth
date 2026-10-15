mod auth;
mod config_cmd;
mod cost;
mod doctor;
mod edit;
mod init;
mod lock;
mod logs;
mod module;
mod run;
mod service;
mod status;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{ArgAction, CommandFactory, Parser, Subcommand};
use noth_core::config::{Config, Paths};

#[derive(Parser)]
#[command(
    name = "noth",
    version,
    about = "Discord 常駐 AI エージェント Noth",
    disable_help_flag = true,
    disable_version_flag = true,
    disable_help_subcommand = true
)]
pub struct Cli {
    /// 設定ファイルを直接指定する(`.env` も同じディレクトリから読む)
    #[arg(short = 'c', long = "config", global = true, value_name = "パス")]
    pub config: Option<PathBuf>,
    /// 詳細ログを出す
    #[arg(short = 'v', long, global = true)]
    pub verbose: bool,
    /// ヘルプを表示する
    #[arg(short = 'h', long, global = true, action = ArgAction::Help)]
    help: Option<bool>,
    /// バージョンを表示する
    #[arg(short = 'V', long, action = ArgAction::Version)]
    version: Option<bool>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// $NOTH_HOME と設定ファイルの雛形を作る
    Init {
        /// 対話せず、値をプレースホルダにした雛形だけを作る
        #[arg(long)]
        non_interactive: bool,
    },
    /// 設定の表示・検査
    #[command(subcommand)]
    Config(ConfigCmd),
    /// module の一覧・有効化・無効化
    #[command(subcommand)]
    Module(ModuleCmd),
    /// フォアグラウンドで常駐する
    Run,
    /// OS のサービスとして管理する(macOS: launchd、Linux: systemd のユーザーレベル)
    #[command(subcommand)]
    Service(ServiceCmd),
    /// 外部サービスにログインする
    #[command(subcommand)]
    Auth(AuthCmd),
    /// 常駐プロセスの状態を表示する
    Status,
    /// 設定と外部疎通を検査する
    Doctor,
    /// LLM と外部 API の利用量・コストを集計して表示する
    Cost {
        /// 集計する期間(today / 24h / 7d / 30d / all)
        #[arg(long, default_value = "7d", value_name = "期間")]
        since: String,
        /// 集計の単位
        #[arg(long, value_enum, default_value_t = cost::By::Model)]
        by: cost::By,
        /// 課金先アカウントの状態を取得しない(外部 API を呼ばない)
        #[arg(long)]
        offline: bool,
    },
    /// ログを表示する
    Logs {
        /// 新しい行を追って表示し続ける
        #[arg(short = 'f', long)]
        follow: bool,
        /// 末尾から表示する行数
        #[arg(short = 'n', long, default_value_t = 100, value_name = "行数")]
        lines: usize,
    },
}

#[derive(Subcommand)]
pub enum ConfigCmd {
    /// 設定を表示する(.env 由来の値はマスク)
    Show,
    /// config.toml と .env のパスを表示する
    Path,
    /// 構文・必須キー・${VAR} の解決・module の構成を検証する
    Check,
    /// ドット区切りのキーで値を取得する(.env 由来の値はマスク)
    Get {
        /// 取得するキー(例: channel.discord.debounce_secs)
        key: String,
        /// JSON で出力する
        #[arg(long)]
        json: bool,
        /// マスクせずに実際の値を表示する
        #[arg(long)]
        reveal: bool,
    },
    /// 値を設定する(大文字スネークケースのキーは .env、それ以外は config.toml)
    Set {
        /// 設定するキー(例: channel.discord.debounce_secs、ANTHROPIC_API_KEY)
        key: String,
        /// 設定する値(bool・10 進の数値・"文字列"・配列は型付き、それ以外は文字列。ID のキーは常に文字列)
        value: String,
        /// 値を型の解釈なしに文字列として設定する
        #[arg(long)]
        string: bool,
    },
    /// 値を削除する
    Unset {
        /// 削除するキー
        key: String,
    },
}

#[derive(Subcommand)]
pub enum ModuleCmd {
    /// module の組み込み状況と有効状態を一覧表示する
    List,
    /// module を有効にする(config.toml の [modules] enabled に追加)
    Enable {
        /// module 名
        name: String,
        /// 変更後に `noth service restart` まで行う
        #[arg(long)]
        restart: bool,
    },
    /// module を無効にする(config.toml の [modules] enabled から外す)
    Disable {
        /// module 名
        name: String,
        /// 変更後に `noth service restart` まで行う
        #[arg(long)]
        restart: bool,
    },
}

#[derive(Subcommand)]
pub enum AuthCmd {
    /// ChatGPT のサブスクリプションで OpenAI Codex にログインする(provider-openai の preset = "codex" 用)
    Codex,
}

#[derive(Subcommand)]
pub enum ServiceCmd {
    /// サービスとして登録する(開始はしない)
    Install,
    /// サービス登録を解除する
    Uninstall,
    /// サービスを開始する
    Start,
    /// サービスを停止する
    Stop,
    /// サービスを再起動する
    Restart,
    /// サービスの状態を表示する
    Status,
}

pub fn command() -> clap::Command {
    localize(Cli::command())
}

// clap の組み込み見出し(Usage / Commands / Options)は英語固定なので、テンプレートで置き換える
fn localize(cmd: clap::Command) -> clap::Command {
    let has_subcommands = cmd.has_subcommands();
    let has_positionals = cmd.get_positionals().next().is_some();
    let mut template = String::from("{about}\n\n使い方: {usage}\n");
    if has_subcommands {
        template.push_str("\nコマンド:\n{subcommands}\n");
    }
    if has_positionals {
        template.push_str("\n引数:\n{positionals}\n");
    }
    template.push_str("\nオプション:\n{options}\n");
    cmd.help_template(template).mut_subcommands(localize)
}

pub async fn dispatch(cli: Cli) -> Result<ExitCode> {
    let cfg = cli.config.as_deref();
    match cli.command {
        Command::Init { non_interactive } => init::run(cfg, non_interactive).await,
        Command::Config(cmd) => config_cmd::run(cfg, cmd).await,
        Command::Module(ModuleCmd::List) => module::list(cfg),
        Command::Module(ModuleCmd::Enable { name, restart }) => {
            module::set_enabled(cfg, &name, true, restart).await
        }
        Command::Module(ModuleCmd::Disable { name, restart }) => {
            module::set_enabled(cfg, &name, false, restart).await
        }
        Command::Service(cmd) => service::run(cfg, cmd).await,
        Command::Run => run::run(cfg, cli.verbose).await,
        Command::Auth(AuthCmd::Codex) => auth::codex(cfg).await,
        Command::Status => status::run(cfg).await,
        Command::Doctor => doctor::run(cfg).await,
        Command::Cost { since, by, offline } => cost::run(cfg, &since, by, offline).await,
        Command::Logs { follow, lines } => logs::run(cfg, follow, lines).await,
    }
}

/// `-c` の指定が使えるファイルか確認する。`init` は作成するので対象外
pub(crate) fn paths(cfg: Option<&Path>) -> Result<Paths> {
    if let Some(path) = cfg {
        if path.is_dir() {
            bail!(
                "-c にはファイルを指定してください(ディレクトリです): {}",
                path.display()
            );
        }
        if !path.exists() {
            bail!("-c で指定した設定ファイルがありません: {}", path.display());
        }
    }
    Ok(Paths::resolve(cfg)?)
}

pub(crate) fn load(cfg: Option<&Path>) -> Result<Config> {
    Config::load(paths(cfg)?).map_err(|e| anyhow::anyhow!(message(&e)))
}

/// 設定エラーはキー名から始まる本文だけで原因が分かるので、`Error` の種別接頭辞を付けない
pub(crate) fn message(e: &noth_core::Error) -> String {
    match e {
        noth_core::Error::Config(m) => m.clone(),
        other => other.to_string(),
    }
}

/// シークレットや会話ログを置くので、作成するディレクトリは自分だけが入れるようにする
pub(crate) fn create_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("{} を作成できません", dir.display()))
}

/// 全角文字を2桁として数え、日本語の見出しでも列を揃える
pub(crate) fn pad(s: &str, width: usize) -> String {
    let w: usize = s.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum();
    format!("{s}{}", " ".repeat(width.saturating_sub(w)))
}

/// `pad` の右寄せ版。数値の桁を揃える
pub(crate) fn pad_left(s: &str, width: usize) -> String {
    let w: usize = s.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum();
    format!("{}{s}", " ".repeat(width.saturating_sub(w)))
}
