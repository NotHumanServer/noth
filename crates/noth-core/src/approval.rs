//! tool 実行前の承認。MVP は auto のみ(security.md §3)
use serde_json::Value;

use crate::types::{ToolContext, ToolSpec};

pub const SHELL_TOOL: &str = "shell";
pub const DISCORD_TOOL: &str = "discord";

#[derive(Debug, Clone, Copy, Default)]
pub struct ApprovalPolicy;

impl ApprovalPolicy {
    /// auto は有効な tool をすべて通す。対話承認を入れるときの差し込み口として、Worker は全 tool 呼び出しをここ経由にする。
    /// 拒否理由は tool_result のエラーとして Worker に返る
    pub fn check(
        &self,
        _spec: &ToolSpec,
        _input: &Value,
        _ctx: &ToolContext,
    ) -> std::result::Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TaskCategory;

    #[test]
    fn auto_approves_every_tool_in_every_category() {
        let ctx = |c| ToolContext {
            run_id: "r".into(),
            task_category: c,
            work_dir: "/tmp".into(),
            meter: None,
            history: None,
            audience: Default::default(),
        };
        for c in [
            TaskCategory::None,
            TaskCategory::WebResearch,
            TaskCategory::FactCheck,
            TaskCategory::CodeOrTool,
        ] {
            for name in [
                SHELL_TOOL,
                DISCORD_TOOL,
                "web_search",
                "web_fetch",
                "web_agent",
            ] {
                let spec = ToolSpec {
                    name: name.into(),
                    description: String::new(),
                    input_schema: Value::Null,
                };
                assert!(
                    ApprovalPolicy.check(&spec, &Value::Null, &ctx(c)).is_ok(),
                    "{name} {c:?}"
                );
            }
        }
    }
}
