mod config_cmd;
mod doctor;
mod init;
mod logs;
mod module;
mod run;
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
    /// module の一覧・有効化
    #[command(subcommand)]
    Module(ModuleCmd),
    /// フォアグラウンドで常駐する
    Run,
    /// OS のサービスとして管理する(MVP では未実装)
    #[command(subcommand)]
    Service(ServiceCmd),
    /// 常駐プロセスの状態を表示する
    Status,
    /// 設定と外部疎通を検査する
    Doctor,
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
    /// 値を設定する(MVP では未実装)
    Set {
        /// 設定するキー
        key: String,
        /// 設定する値
        value: String,
    },
    /// 値を削除する(MVP では未実装)
    Unset {
        /// 削除するキー
        key: String,
    },
}

#[derive(Subcommand)]
pub enum ModuleCmd {
    /// module の組み込み状況と有効状態を一覧表示する
    List,
    /// module を有効にする(MVP では未実装)
    Enable {
        /// module 名
        name: String,
        /// 常駐中なら再起動する
        #[arg(long)]
        restart: bool,
    },
    /// module を無効にする(MVP では未実装)
    Disable {
        /// module 名
        name: String,
        /// 常駐中なら再起動する
        #[arg(long)]
        restart: bool,
    },
}

#[derive(Subcommand)]
pub enum ServiceCmd {
    /// サービスとして登録する(MVP では未実装)
    Install,
    /// サービス登録を解除する(MVP では未実装)
    Uninstall,
    /// サービスを開始する(MVP では未実装)
    Start,
    /// サービスを停止する(MVP では未実装)
    Stop,
    /// サービスを再起動する(MVP では未実装)
    Restart,
    /// サービスの状態を表示する(MVP では未実装)
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
        Command::Init { non_interactive } => init::run(cfg, non_interactive),
        Command::Config(cmd) => config_cmd::run(cfg, cmd),
        Command::Module(ModuleCmd::List) => module::list(cfg),
        Command::Module(_) | Command::Service(_) => Ok(unimplemented_exit()),
        Command::Run => run::run(cfg, cli.verbose).await,
        Command::Status => status::run(cfg).await,
        Command::Doctor => doctor::run(cfg).await,
        Command::Logs { follow, lines } => logs::run(cfg, follow, lines).await,
    }
}

pub(crate) fn unimplemented_exit() -> ExitCode {
    eprintln!("MVP では未実装です");
    ExitCode::from(2)
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
