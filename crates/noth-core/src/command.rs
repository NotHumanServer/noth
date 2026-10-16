//! 運営者が Channel から呼ぶ管理コマンド。Discord のスラッシュコマンドなどへの割り当て・権限の判定・描画は Channel が行い、
//! ここは何を調べて何を返すかだけを定める
use std::sync::Arc;

use async_trait::async_trait;

use crate::{Error, Result};

/// 画面に出す 1 画面分の報告。Channel が自分の表示形式(Discord なら Components V2 や embed)に当てはめる
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub title: String,
    pub sections: Vec<ReportSection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportSection {
    pub heading: String,
    /// 改行を含む Markdown。棒グラフなど桁をそろえたい行はコードブロックに入れて渡す
    pub body: String,
}

#[async_trait]
pub trait Command: Send + Sync {
    /// Channel 側のコマンド名(`/noth <name>`)になる。小文字の英数字とハイフンだけ
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    async fn run(&self, env: &CommandEnv) -> Result<Report>;
}

/// コマンドが Core の状態を読むための窓口。書き込みは持たない
#[derive(Clone)]
pub struct CommandEnv {
    context: Arc<dyn ContextSource>,
}

impl CommandEnv {
    pub fn new(context: Arc<dyn ContextSource>) -> Self {
        Self { context }
    }

    pub async fn context(&self) -> Result<ContextSnapshot> {
        self.context.context_snapshot().await
    }
}

/// actor が実装する。actor の状態は actor 自身だけが読めるので、仕事のキュー越しに問い合わせる
#[async_trait]
pub trait ContextSource: Send + Sync {
    async fn context_snapshot(&self) -> Result<ContextSnapshot>;
}

/// Channel に渡す、有効なコマンドの一覧と実行環境
#[derive(Clone)]
pub struct CommandHost {
    commands: Vec<Arc<dyn Command>>,
    env: CommandEnv,
}

impl CommandHost {
    pub fn new(commands: Vec<Arc<dyn Command>>, env: CommandEnv) -> Self {
        Self { commands, env }
    }

    pub fn commands(&self) -> &[Arc<dyn Command>] {
        &self.commands
    }

    pub async fn run(&self, name: &str) -> Result<Report> {
        let command = self
            .commands
            .iter()
            .find(|c| c.name() == name)
            .ok_or_else(|| Error::Other(format!("未知のコマンドです: `{name}`")))?;
        command.run(&self.env).await
    }
}

/// プロンプトの 1 つの層の大きさ
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerSize {
    /// `tools`、`Layer 0`、`Layer 1`、`Layer 1.5`、`Layer 2`、`Layer 3`
    pub name: &'static str,
    pub chars: usize,
    /// 文字数からの推定。tokenizer は provider ごとに違うので実測ではない
    pub est_tokens: u64,
}

/// 直近の main(または Join)のリクエストの実測。課金記録の 1 行から作るので、3 つの値は同じリクエストのもの
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestUsage {
    /// プロンプト全体(キャッシュの読み書きを含む)
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// リクエストから問い合わせまでの秒数
    pub age_secs: u64,
    /// このリクエストのあとに Layer 3 を圧縮した。いまの文脈の大きさとは合わない
    pub compacted_since: bool,
}

/// 今の main の文脈の大きさ。本文は運ばない
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextSnapshot {
    pub model: String,
    /// プロンプトに載る順。Layer 4 は発言が来たリクエストの間だけ存在するので含めない
    pub layers: Vec<LayerSize>,
    pub last_request: Option<RequestUsage>,
    /// `prompt_hard_limit_tokens`。直近のリクエストの `input_tokens` がこれを超えると緊急コンパクションが走る
    pub hard_limit_tokens: u64,
    pub turns: usize,
    pub turn_channels: usize,
    pub episodes: usize,
    pub workers_running: usize,
    pub workers_queued: usize,
}

/// ASCII は約 4 文字で 1 トークン、それ以外は 1 文字で約 1 トークンとみなす粗い推定
pub fn estimate_tokens(text: &str) -> u64 {
    let (ascii, other) = text.chars().fold((0u64, 0u64), |(a, o), c| {
        if c.is_ascii() { (a + 1, o) } else { (a, o + 1) }
    });
    ascii.div_ceil(4) + other
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_weighs_non_ascii_heavier() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcdefgh"), 2);
        assert_eq!(estimate_tokens("あいう"), 3);
        assert_eq!(estimate_tokens("abcdあ"), 2);
    }

    struct Echo;

    #[async_trait]
    impl Command for Echo {
        fn name(&self) -> &'static str {
            "echo"
        }
        fn description(&self) -> &'static str {
            "x"
        }
        async fn run(&self, _env: &CommandEnv) -> Result<Report> {
            Ok(Report {
                title: "t".into(),
                sections: Vec::new(),
            })
        }
    }

    struct NoContext;

    #[async_trait]
    impl ContextSource for NoContext {
        async fn context_snapshot(&self) -> Result<ContextSnapshot> {
            Err(Error::Other("unused".into()))
        }
    }

    #[tokio::test]
    async fn host_runs_by_name_and_rejects_unknown() {
        let host = CommandHost::new(vec![Arc::new(Echo)], CommandEnv::new(Arc::new(NoContext)));
        assert_eq!(host.run("echo").await.unwrap().title, "t");
        assert!(host.run("nope").await.is_err());
    }
}
