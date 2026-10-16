//! sh -c でコマンドを実行する tool。実行の可否は ApprovalPolicy(Core)が判断する
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use noth_core::approval::SHELL_TOOL;
use noth_core::registry::{ModuleCtx, Registry};
use noth_core::traits::Tool;
use noth_core::types::{ToolContext, ToolOutput, ToolSpec};
use noth_core::{Error, Result};
use serde_json::{Value, json};
use tokio::process::Command;

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 600;

mod capture;

use capture::spawn_capture;

pub fn register(reg: &mut Registry) {
    reg.register_tool("tool-shell", factory);
}

fn factory(ctx: &ModuleCtx) -> Result<Arc<dyn Tool>> {
    Ok(Arc::new(ShellTool {
        // 作れなかったときは、存在しない場所を環境変数で案内しない
        workspace: ctx
            .config
            .paths
            .ensure_workspace()
            .ok()
            .map(|()| noth_core::config::absolute(&ctx.config.paths.workspace())),
    }))
}

struct ShellTool {
    workspace: Option<std::path::PathBuf>,
}

#[async_trait]
impl Tool for ShellTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: SHELL_TOOL.into(),
            description: format!(
                "作業ディレクトリでシェルコマンド(sh -c)を実行し、終了コード・標準エラー・標準出力を返す。\
                 ファイルの作成・編集、プログラムの実行、テストの実行に使う。\
                 標準入力は使えない。timeout_secs を超えるとプロセスは強制終了される(既定 {DEFAULT_TIMEOUT_SECS} 秒、最大 {MAX_TIMEOUT_SECS} 秒)。\
                 コマンドが終了すると、& で起動した背景プロセスも含めてすべて終了させられる(常駐はできない)。\
                 出力は各ストリームの先頭と末尾だけを残し、長い中間は省略される。"
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "実行するシェルコマンド"},
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_TIMEOUT_SECS,
                        "description": "タイムアウト秒数(既定 120)"
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let Some(command) = input.get("command").and_then(Value::as_str) else {
            return Ok(error_output("command(文字列)が指定されていません"));
        };
        let timeout_secs = input
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS);

        create_work_dir(&ctx.work_dir)?;

        // Noth の秘密情報(API キー等)は設定ファイルにあり、プロセスの環境変数には載せない設計なので、環境は消さず引き継ぐ
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(command)
            .current_dir(&ctx.work_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // cwd は run ごとの下書き用なので、残す成果物の置き場所は環境変数で渡す。
        // 無いときは、親プロセスから引き継いだ同名の変数を残さない
        match &self.workspace {
            Some(w) => cmd.env("NOTH_WORKSPACE", w),
            None => cmd.env_remove("NOTH_WORKSPACE"),
        };
        // 孫プロセスまでまとめて止めるため、専用のプロセスグループで動かす
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd
            .spawn()
            .map_err(|e| Error::Other(format!("shell: プロセスを起動できません: {e}")))?;
        let _group = child.id().map(GroupKiller);

        let stdout = spawn_capture(child.stdout.take());
        let stderr = spawn_capture(child.stderr.take());

        let waited = tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait()).await;
        // sh が終わっても背景プロセスがパイプを握り続けるため、先にグループごと止めて EOF を得る
        drop(_group);
        let status = match waited {
            Ok(Ok(status)) => Some(status),
            Ok(Err(e)) => {
                return Ok(error_output(&format!(
                    "プロセスの完了待ちに失敗しました: {e}"
                )));
            }
            Err(_) => {
                child.wait().await.ok();
                None
            }
        };
        let (out, err) = (stdout.finish().await, stderr.finish().await);

        let body = format!("[stderr]\n{err}\n[stdout]\n{out}");
        Ok(match status {
            None => ToolOutput {
                text: format!(
                    "タイムアウト({timeout_secs} 秒)したためプロセスを終了しました。ここまでの出力:\n{body}"
                ),
                is_error: true,
            },
            Some(status) => {
                let exit = status.code();
                let exit_text = exit.map_or("シグナルで終了".to_string(), |c| c.to_string());
                ToolOutput {
                    text: format!("終了コード: {exit_text}\n{body}"),
                    is_error: exit != Some(0),
                }
            }
        })
    }
}

/// drop でプロセスグループ全体に SIGKILL を送る。呼び出しが途中で捨てられた場合にも孫プロセスを残さない
struct GroupKiller(u32);

impl Drop for GroupKiller {
    fn drop(&mut self) {
        #[cfg(unix)]
        // SAFETY: kill(2) に負の pid を渡してグループを指定するだけで、メモリには触れない
        unsafe {
            libc::kill(-(self.0 as libc::pid_t), libc::SIGKILL);
        }
    }
}

fn error_output(text: &str) -> ToolOutput {
    ToolOutput {
        text: text.to_string(),
        is_error: true,
    }
}

fn create_work_dir(dir: &std::path::Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir).map_err(|e| {
        Error::Other(format!(
            "shell: 作業ディレクトリ {} を作れません: {e}",
            dir.display()
        ))
    })
}

#[cfg(test)]
mod tests;
