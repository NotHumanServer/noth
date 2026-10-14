use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use noth_backend_tinyfish::{AgentRun, RunStatus, TinyFish, require_backend};
use noth_core::registry::{ModuleCtx, Registry};
use noth_core::traits::Tool;
use noth_core::types::{ToolContext, ToolOutput, ToolSpec};
use noth_core::{Error, Result};
use serde_json::{Value, json};

const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;
/// worker の timeout より短くはならない想定だが、run が課金されたまま残らないよう上限を必ず付ける
const DEFAULT_MAX_DURATION_SECS: u64 = 300;
/// サーバ側の max_duration_seconds が効かなかった場合に備えた、ローカル deadline の猶予
const DEADLINE_GRACE: Duration = Duration::from_secs(60);
const MAX_STEPS_LIMIT: u64 = 150;
/// ステップ課金のため、モデルが指定しない場合の既定は小さく取る
const DEFAULT_MAX_STEPS: u32 = 30;
const DEFAULT_MAX_STEPS_PER_RUN: u32 = 60;
/// poll の一時的な失敗で run を cancel しないための連続許容回数
const POLL_ATTEMPTS: u32 = 3;

pub fn register(reg: &mut Registry) {
    reg.register_tool("tool-web-agent", factory);
}

fn factory(ctx: &ModuleCtx) -> Result<Arc<dyn Tool>> {
    require_backend(&ctx.section, "web-agent")?;
    let secs = |key: &str, default: u64| match ctx.section.get(key) {
        None => Ok(default),
        Some(v) => v
            .as_integer()
            .filter(|n| *n > 0)
            .map(|n| n as u64)
            .ok_or_else(|| {
                Error::Config(format!(
                    "[tool.web-agent] {key} は正の整数で指定してください"
                ))
            }),
    };
    let steps = |key: &str, default: u32, max: u64| -> Result<u32> {
        let n = secs(key, default as u64)?;
        if n > max {
            return Err(Error::Config(format!(
                "[tool.web-agent] {key} は {max} 以下で指定してください"
            )));
        }
        Ok(n as u32)
    };
    Ok(Arc::new(
        WebAgent::new(
            TinyFish::from_config(ctx.config)?,
            Duration::from_secs(secs("poll_interval_secs", DEFAULT_POLL_INTERVAL_SECS)?),
            secs("max_duration_secs", DEFAULT_MAX_DURATION_SECS)?,
        )
        .with_step_limits(
            steps("max_steps", DEFAULT_MAX_STEPS, MAX_STEPS_LIMIT)?,
            steps(
                "max_steps_per_run",
                DEFAULT_MAX_STEPS_PER_RUN,
                u32::MAX as u64,
            )?,
        ),
    ))
}

pub struct WebAgent {
    backend: TinyFish,
    poll_interval: Duration,
    max_duration_secs: u64,
    grace: Duration,
    max_steps: u32,
    max_steps_per_run: u32,
    /// エントリは run 1件あたり数十バイトなので、掃除はしない。
    /// Worker の run_id ごとの累計ステップ数。1回の調査で web_agent を繰り返し呼ばれても課金を頭打ちにする
    used_steps: Mutex<HashMap<String, u32>>,
}

impl WebAgent {
    pub fn new(backend: TinyFish, poll_interval: Duration, max_duration_secs: u64) -> Self {
        Self {
            backend,
            poll_interval,
            max_duration_secs,
            grace: DEADLINE_GRACE,
            max_steps: DEFAULT_MAX_STEPS,
            max_steps_per_run: DEFAULT_MAX_STEPS_PER_RUN,
            used_steps: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_step_limits(mut self, max_steps: u32, max_steps_per_run: u32) -> Self {
        self.max_steps = max_steps;
        self.max_steps_per_run = max_steps_per_run;
        self
    }

    fn used(&self, run_id: &str) -> u32 {
        let g = self.used_steps.lock().expect("lock");
        g.get(run_id).copied().unwrap_or(0)
    }

    fn add_used(&self, run_id: &str, steps: u32) {
        let mut g = self.used_steps.lock().expect("lock");
        *g.entry(run_id.to_string()).or_insert(0) += steps;
    }

    pub fn with_grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }

    /// 打ち切る場合は run を止めてから理由を返す。止められなかった場合もログに残す
    async fn abort(&self, id: &str, guard: &mut CancelOnDrop, reason: String) -> ToolOutput {
        guard.disarm();
        if let Err(e) = self.backend.agent_cancel(id).await {
            tracing::warn!(run_id = id, error = %e, "web_agent の run を cancel できませんでした");
        }
        ToolOutput {
            text: reason,
            is_error: true,
        }
    }

    /// `seen_steps` には中断で戻った場合でも、最後に観測したステップ数を残す
    async fn run(
        &self,
        url: &str,
        goal: &str,
        max_steps: u32,
        seen_steps: &mut u32,
    ) -> Result<ToolOutput> {
        let id = self
            .backend
            .agent_run_async(url, goal, Some(max_steps), self.max_duration_secs)
            .await?;
        tracing::info!(run_id = id, "web_agent の run を開始しました");
        let mut guard = CancelOnDrop::new(self.backend.clone(), id.clone());
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(self.max_duration_secs) + self.grace;
        let mut failures = 0;
        let run = loop {
            let run = match self.backend.agent_run(&id).await {
                Ok(run) => run,
                Err(e) => {
                    failures += 1;
                    tracing::warn!(run_id = id, failures, error = %e, "run の状態取得に失敗しました");
                    if failures >= POLL_ATTEMPTS || !is_transient(&e) {
                        let reason = format!("run の状態を取得できないため実行を中断しました: {e}");
                        return Ok(self.abort(&id, &mut guard, reason).await);
                    }
                    tokio::time::sleep(self.poll_interval).await;
                    continue;
                }
            };
            failures = 0;
            *seen_steps = run.num_of_steps.unwrap_or(*seen_steps);
            let status = run.run_status();
            if status.is_terminal() {
                break run;
            }
            if status == RunStatus::Unknown {
                let reason = format!("想定外の status `{}` のため実行を中断しました", run.status);
                return Ok(self.abort(&id, &mut guard, reason).await);
            }
            if tokio::time::Instant::now() >= deadline {
                let reason = format!(
                    "{} 秒以内に完了しなかったため実行を中断しました(status: {})",
                    self.max_duration_secs + self.grace.as_secs(),
                    run.status
                );
                return Ok(self.abort(&id, &mut guard, reason).await);
            }
            tokio::time::sleep(self.poll_interval).await;
        };
        guard.disarm();
        Ok(format_run(&run))
    }
}

/// 待てば直る失敗だけ。認証や存在しない run のような失敗は再試行しても同じ結果になる
fn is_transient(e: &Error) -> bool {
    match e {
        Error::Transport(_) => true,
        Error::Api { status, .. } => matches!(status, 408 | 429 | 500..=599),
        _ => false,
    }
}

/// worker の timeout で future が捨てられると run が走り続けて課金されるため、捨てられたら cancel を送る。
/// Drop は async にできないので、別 task に投げて結果は待たない
struct CancelOnDrop {
    backend: TinyFish,
    run_id: String,
    armed: bool,
}

impl CancelOnDrop {
    fn new(backend: TinyFish, run_id: String) -> Self {
        Self {
            backend,
            run_id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let backend = self.backend.clone();
        let run_id = std::mem::take(&mut self.run_id);
        handle.spawn(async move {
            match backend.agent_cancel(&run_id).await {
                Ok(()) => tracing::info!(run_id, "中断された web_agent の run を cancel しました"),
                Err(e) => tracing::warn!(run_id, error = %e, "web_agent の run を cancel できませんでした"),
            }
        });
    }
}

fn parse_input(input: &Value) -> std::result::Result<(String, String, Option<u32>), String> {
    let text = |key: &str| {
        input
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or(format!("{key} は必須です"))
    };
    let max_steps = match input.get("max_steps") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .filter(|n| (1..=MAX_STEPS_LIMIT).contains(n))
                .ok_or(format!(
                    "max_steps は 1〜{MAX_STEPS_LIMIT} で指定してください"
                ))? as u32,
        ),
    };
    Ok((text("url")?, text("goal")?, max_steps))
}

fn format_run(run: &AgentRun) -> ToolOutput {
    let steps = run.num_of_steps.unwrap_or(0);
    let status = run.status.as_str();
    let pretty = |v: &Value| serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string());
    let (headline, is_error) = match run.run_status() {
        RunStatus::Completed => {
            // COMPLETED は基盤の処理が終わっただけで、目標の達成は result 側にしか書かれない
            let failure = run
                .result
                .as_ref()
                .filter(|r| r.get("status").and_then(Value::as_str) == Some("failure"));
            match failure {
                Some(r) => {
                    let reason = r
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("理由不明");
                    (format!("目標を達成できませんでした: {reason}"), true)
                }
                None => ("目標を達成しました".to_string(), false),
            }
        }
        RunStatus::Failed => {
            let e = run.error.as_ref();
            let msg = e.and_then(|e| e.message.as_deref()).unwrap_or("理由不明");
            let code = e.and_then(|e| e.code.as_deref()).map(|c| format!("{c}: "));
            (
                format!("実行に失敗しました: {}{msg}", code.unwrap_or_default()),
                true,
            )
        }
        RunStatus::Cancelled => ("実行は中断されました".to_string(), true),
        _ => (format!("想定外の状態です: {status}"), true),
    };
    let mut text = format!("{headline}\nstatus: {status}\nnum_of_steps: {steps}");
    if let Some(r) = &run.result {
        text.push_str(&format!("\nresult:\n{}", pretty(r)));
    }
    ToolOutput { text, is_error }
}

#[async_trait]
impl Tool for WebAgent {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_agent".into(),
            description: "ブラウザを自動操作する AI エージェントに、自然言語の目標を実行させる。\
                ログインや入力操作を伴うページ、動的に描画されるページなど、web_search と web_fetch では取れない情報にだけ使う。\
                有料(1 ステップあたり約 $0.016)で、完了まで数十秒〜数分かかるため、検索と取得で足りるなら使わない。"
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "操作を始めるページの URL"},
                    "goal": {"type": "string", "description": "達成したいことを具体的に書いた自然言語の指示"},
                    "max_steps": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_STEPS_LIMIT,
                        "description": "ステップ数の上限。課金の上限にもなる"
                    }
                },
                "required": ["url", "goal"]
            }),
        }
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let (url, goal, requested) = match parse_input(&input) {
            Ok(v) => v,
            Err(text) => {
                return Ok(ToolOutput {
                    text,
                    is_error: true,
                });
            }
        };
        let remaining = self
            .max_steps_per_run
            .saturating_sub(self.used(&ctx.run_id));
        if remaining == 0 {
            return Ok(ToolOutput {
                text: format!(
                    "この調査での web_agent のステップ上限({})に達したため実行しません。取得済みの情報で結論を出してください",
                    self.max_steps_per_run
                ),
                is_error: true,
            });
        }
        let max_steps = requested
            .map_or(self.max_steps, |n| n.min(self.max_steps))
            .min(remaining);
        let mut seen = 0;
        let result = self.run(&url, &goal, max_steps, &mut seen).await;
        self.add_used(&ctx.run_id, seen);
        match result {
            Err(Error::Api { body, .. }) => Ok(ToolOutput {
                text: body,
                is_error: true,
            }),
            other => other,
        }
    }

    /// 実行すると課金されるので API は呼ばない。設定の検証は構築時に済んでいる
    async fn health_check(&self) -> Result<String> {
        Ok("設定を確認しました(課金を避けるため API は呼んでいません)".into())
    }
}

#[cfg(test)]
mod tests;
