use std::sync::Arc;

use async_trait::async_trait;
use noth_core::registry::{ModuleCtx, Registry};
use noth_core::traits::{Compactor, LayeredPrompt, Provider};
use noth_core::types::*;
use noth_core::{Error, Result};

pub fn register(reg: &mut Registry) {
    reg.register_compactor(factory);
}

fn factory(ctx: &ModuleCtx, provider: Arc<dyn Provider>) -> Result<Arc<dyn Compactor>> {
    Ok(Arc::new(LlmCompactor::new(
        provider,
        ctx.config.context.max_tokens,
    )))
}

pub struct LlmCompactor {
    provider: Arc<dyn Provider>,
    max_tokens: u32,
}

impl LlmCompactor {
    pub fn new(provider: Arc<dyn Provider>, max_tokens: u32) -> Self {
        Self {
            provider,
            max_tokens,
        }
    }
}

fn render_turn(turn: &Turn) -> String {
    match (turn.role, turn.kind) {
        (_, TurnKind::Conclusion) => format!("[調査結果]\n{}", turn.text),
        (Role::Assistant, _) => format!("[Noth 自身の発言]\n{}", turn.text),
        (Role::User, _) => turn.text.clone(),
    }
}

fn instruction(turns: &[Turn]) -> Turn {
    let rendered = turns
        .iter()
        .map(render_turn)
        .collect::<Vec<_>>()
        .join("\n\n");
    let text = format!(
        "以下は Discord サーバーでの会話の記録です。\n\n{rendered}\n\n\
         この会話を、日本語の三人称・事実のみのエピソード要約 1 つにまとめてください。\n\
         - 「A が○○について質問し、B が△△と回答した」の形で書く\n\
         - キャラクターの口調、意見、推測は入れない\n\
         - 記録から日付の範囲が分かる場合は、先頭に日付範囲を示す見出し行を 1 行置く\n\
         - 要約本文だけを出力する"
    );
    Turn {
        id: None,
        role: Role::User,
        kind: TurnKind::Instruction,
        text,
        chunk_id: None,
    }
}

#[async_trait]
impl Compactor for LlmCompactor {
    async fn digest(&self, harness_rules: &str, turns: &[Turn]) -> Result<String> {
        // Profile を渡すとキャラクターの口調が要約に混ざる
        let prompt = LayeredPrompt {
            harness_rules: harness_rules.to_string(),
            ephemeral_tail: vec![instruction(turns)],
            ..Default::default()
        };
        let completion = self
            .provider
            .complete(
                &prompt,
                &CompletionParams {
                    max_tokens: self.max_tokens,
                },
            )
            .await?;
        let summary = completion.text.as_deref().unwrap_or("").trim();
        if summary.is_empty() {
            return Err(Error::Other("エピソード要約が空でした".into()));
        }
        Ok(summary.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use noth_core::traits::Completion;

    use super::*;

    struct MockProvider {
        reply: Option<String>,
        prompts: Mutex<Vec<LayeredPrompt>>,
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn complete(&self, p: &LayeredPrompt, _: &CompletionParams) -> Result<Completion> {
            self.prompts.lock().unwrap().push(p.clone());
            Ok(Completion {
                text: self.reply.clone(),
                ..Default::default()
            })
        }
        async fn health_check(&self) -> Result<String> {
            Ok("ok".into())
        }
    }

    fn turn(role: Role, kind: TurnKind, text: &str) -> Turn {
        Turn {
            id: None,
            role,
            kind,
            text: text.into(),
            chunk_id: None,
        }
    }

    fn compactor(reply: Option<&str>) -> (LlmCompactor, Arc<MockProvider>) {
        let p = Arc::new(MockProvider {
            reply: reply.map(Into::into),
            prompts: Mutex::new(Vec::new()),
        });
        (LlmCompactor::new(p.clone(), 512), p)
    }

    #[tokio::test]
    async fn builds_prompt_without_profile() {
        let (c, p) = compactor(Some(" 要約 "));
        let turns = vec![
            turn(Role::User, TurnKind::Buffer, "[発言群]\nA: Rust って?"),
            turn(Role::Assistant, TurnKind::Response, "安全な言語です"),
            turn(Role::User, TurnKind::Conclusion, "1.90 が最新"),
        ];
        let out = c.digest("rules", &turns).await.unwrap();
        assert_eq!(out, "要約");
        let prompts = p.prompts.lock().unwrap();
        let prompt = &prompts[0];
        assert_eq!(prompt.harness_rules, "rules");
        assert!(prompt.profile.is_empty());
        assert!(prompt.episodic_digest.is_empty());
        assert!(prompt.active_log.is_empty());
        let t = &prompt.ephemeral_tail;
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].kind, TurnKind::Instruction);
        assert!(t[0].text.contains("A: Rust って?"));
        assert!(t[0].text.contains("[Noth 自身の発言]\n安全な言語です"));
        assert!(t[0].text.contains("[調査結果]\n1.90 が最新"));
        assert!(t[0].text.contains("三人称"));
    }

    #[tokio::test]
    async fn empty_result_is_error() {
        let (c, _) = compactor(Some("  "));
        assert!(c.digest("rules", &[]).await.is_err());
        let (c, _) = compactor(None);
        assert!(c.digest("rules", &[]).await.is_err());
    }
}
