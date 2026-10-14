//! tool 実行前の承認。MVP は auto のみ(security.md §3)
use serde_json::Value;

use crate::types::{TaskCategory, ToolContext, ToolSpec};

pub const SHELL_TOOL: &str = "shell";

#[derive(Debug, Clone, Copy, Default)]
pub struct ApprovalPolicy;

impl ApprovalPolicy {
    /// 拒否理由は tool_result のエラーとして Worker に返す。tool 定義は全 run で同一のまま保つため、定義から外さずここで弾く
    pub fn check(
        &self,
        spec: &ToolSpec,
        _input: &Value,
        ctx: &ToolContext,
    ) -> std::result::Result<(), String> {
        // Web から取った文字列に書かれた指示でシェルを動かされる経路(間接プロンプトインジェクション → RCE)を断つ。
        // shell を許す run で Web を読めると同じ経路が残るので、1 つの run では Web か shell のどちらかしか使わせない
        let is_shell = spec.name == SHELL_TOOL;
        let is_code_run = ctx.task_category == TaskCategory::CodeOrTool;
        if is_shell && !is_code_run {
            return Err("shell はコード・ツール作業(code_or_tool)の run でだけ使えます".into());
        }
        if !is_shell && is_code_run {
            return Err(format!(
                "コード・ツール作業(code_or_tool)の run では {} を使えません。Web から取った文字列で shell を動かさないためです",
                spec.name
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(c: TaskCategory) -> ToolContext {
        ToolContext {
            run_id: "r".into(),
            task_category: c,
            work_dir: "/tmp".into(),
        }
    }

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: String::new(),
            input_schema: Value::Null,
        }
    }

    #[test]
    fn shell_only_for_code_or_tool() {
        let p = ApprovalPolicy;
        assert!(
            p.check(
                &spec("shell"),
                &Value::Null,
                &ctx(TaskCategory::WebResearch)
            )
            .is_err()
        );
        assert!(
            p.check(&spec("shell"), &Value::Null, &ctx(TaskCategory::CodeOrTool))
                .is_ok()
        );
        assert!(
            p.check(
                &spec("web_search"),
                &Value::Null,
                &ctx(TaskCategory::FactCheck)
            )
            .is_ok()
        );
        assert!(
            p.check(
                &spec("web_fetch"),
                &Value::Null,
                &ctx(TaskCategory::CodeOrTool)
            )
            .is_err()
        );
    }
}
