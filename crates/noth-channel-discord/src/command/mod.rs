//! `/noth <command>`。master だけが使え、結果は実行した本人にだけ見える(ephemeral)
use std::sync::Arc;

use noth_core::command::{CommandHost, Report};
use serde_json::{Value, json};

mod api;
mod render;

pub(crate) use api::{InteractionApi, RestInteractionApi};
use render::{FLAG_EPHEMERAL, embed_message, parse_refresh, plain, text_message, v2_message};

/// Discord 側のコマンド名。サブコマンドが各 `Command::name()` になる
const ROOT: &str = "noth";
const DENIED: &str = "権限がありません";

const CALLBACK_MESSAGE: u8 = 4;
const CALLBACK_DEFER_MESSAGE: u8 = 5;
const CALLBACK_DEFER_UPDATE: u8 = 6;

pub(crate) struct Incoming {
    pub(crate) id: u64,
    pub(crate) token: String,
    pub(crate) user_id: u64,
    pub(crate) guild_id: Option<u64>,
    pub(crate) kind: Kind,
}

pub(crate) enum Kind {
    Slash { name: String, sub: Option<String> },
    Button { custom_id: String },
}

pub(crate) struct Controller {
    api: Arc<dyn InteractionApi>,
    host: Arc<CommandHost>,
    master: Option<u64>,
    guild_id: u64,
}

impl Controller {
    pub(crate) fn new(
        api: Arc<dyn InteractionApi>,
        host: Arc<CommandHost>,
        master: Option<u64>,
        guild_id: u64,
    ) -> Self {
        Self {
            api,
            host,
            master,
            guild_id,
        }
    }

    /// master が未設定なら誰にも使えないので、コマンド自体を出さない
    fn definitions(&self) -> Value {
        if self.master.is_none() || self.host.commands().is_empty() {
            return json!([]);
        }
        let subs: Vec<Value> = self
            .host
            .commands()
            .iter()
            .map(|c| json!({"type": 1, "name": c.name(), "description": c.description()}))
            .collect();
        json!([{
            "type": 1,
            "name": ROOT,
            "description": "Noth の管理コマンド",
            "dm_permission": false,
            "options": subs,
        }])
    }

    pub(crate) async fn sync(&self) {
        if self.host.commands().is_empty() {
            return;
        }
        if self.master.is_none() {
            tracing::info!(
                "channel.discord.master が未設定のため、/noth コマンドは登録せず、誰にも使えません"
            );
        }
        if let Err(e) = self.api.sync_commands(&self.definitions()).await {
            tracing::warn!(
                error = %e,
                "スラッシュコマンドを登録できませんでした。Bot の招待に applications.commands スコープが含まれているか確認してください(docs/discord-setup.md)"
            );
        }
    }

    pub(crate) async fn handle(&self, ix: Incoming) {
        if ix.guild_id != Some(self.guild_id) || self.master != Some(ix.user_id) {
            self.reply_ephemeral(&ix, DENIED).await;
            return;
        }
        match &ix.kind {
            Kind::Slash { name, sub } if name == ROOT => match sub {
                Some(sub) if self.has(sub) => self.slash(&ix, sub).await,
                _ => self.reply_ephemeral(&ix, "未知のコマンドです").await,
            },
            Kind::Slash { .. } => {}
            Kind::Button { custom_id } => match parse_refresh(custom_id) {
                Some(command) if self.has(command) => self.refresh(&ix, command).await,
                _ => self.reply_ephemeral(&ix, "未知の操作です").await,
            },
        }
    }

    fn has(&self, command: &str) -> bool {
        self.host.commands().iter().any(|c| c.name() == command)
    }

    async fn reply_ephemeral(&self, ix: &Incoming, text: &str) {
        let mut data = plain(text);
        data["flags"] = json!(FLAG_EPHEMERAL);
        self.callback(ix, CALLBACK_MESSAGE, Some(data)).await;
    }

    async fn callback(&self, ix: &Incoming, kind: u8, data: Option<Value>) -> bool {
        let mut body = json!({"type": kind});
        if let Some(data) = data {
            body["data"] = data;
        }
        match self.api.callback(ix.id, &ix.token, &body).await {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, kind, "interaction に応答できませんでした");
                false
            }
        }
    }

    /// 3 秒の受付期限に収めるため、先に defer してから中身を作って最初の応答を書き換える
    async fn slash(&self, ix: &Incoming, command: &str) {
        let defer = json!({"flags": FLAG_EPHEMERAL});
        if !self.callback(ix, CALLBACK_DEFER_MESSAGE, Some(defer)).await {
            return;
        }
        let report = match self.host.run(command).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, command, "コマンドの実行に失敗しました");
                self.edit_or_log(ix, &plain(&format!("取得に失敗しました: {e}")))
                    .await;
                return;
            }
        };
        self.deliver_first(ix, &report, command).await;
    }

    /// defer 済みの応答はまだ V2 ではないので、V2 が拒否されても embed、さらに文字へ落とせる
    async fn deliver_first(&self, ix: &Incoming, report: &Report, command: &str) {
        let attempts = [
            ("Components V2", v2_message(report, Some(command))),
            ("embed", embed_message(report)),
            ("テキスト", text_message(report)),
        ];
        for (label, body) in attempts {
            match self.api.edit_original(&ix.token, &body).await {
                Ok(()) => return,
                Err(e) => tracing::warn!(error = %e, label, "コマンドの結果を送れませんでした"),
            }
        }
        tracing::error!(command, "コマンドの結果をどの形式でも送れませんでした");
    }

    async fn edit_or_log(&self, ix: &Incoming, body: &Value) {
        if let Err(e) = self.api.edit_original(&ix.token, body).await {
            tracing::warn!(error = %e, "コマンドの応答を書き換えられませんでした");
        }
    }

    /// 結果を作る間に actor の待ち行列で 3 秒を超えうるので、type 7 では即答できない。
    /// 先に type 6 で受け付けて、押されたメッセージを後から書き換える
    async fn refresh(&self, ix: &Incoming, command: &str) {
        if !self.callback(ix, CALLBACK_DEFER_UPDATE, None).await {
            return;
        }
        let report = match self.host.run(command).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, command, "コマンドの再実行に失敗しました");
                self.followup(ix, plain(&format!("更新に失敗しました: {e}")))
                    .await;
                return;
            }
        };
        match self
            .api
            .edit_original(&ix.token, &v2_message(&report, Some(command)))
            .await
        {
            Ok(()) => {}
            Err(e) => {
                // V2 のメッセージは通常の形式へ戻せないので、新しい ephemeral のメッセージで見せる
                tracing::warn!(error = %e, "更新を Components V2 で送れませんでした。新しいメッセージで送ります");
                self.followup_report(ix, &report).await;
            }
        }
    }

    async fn followup_report(&self, ix: &Incoming, report: &Report) {
        for mut body in [embed_message(report), text_message(report)] {
            body["flags"] = json!(FLAG_EPHEMERAL);
            match self.api.followup(&ix.token, &body).await {
                Ok(()) => return,
                Err(e) => {
                    tracing::warn!(error = %e, "更新結果を新しいメッセージでも送れませんでした")
                }
            }
        }
    }

    async fn followup(&self, ix: &Incoming, mut body: Value) {
        body["flags"] = json!(FLAG_EPHEMERAL);
        if let Err(e) = self.api.followup(&ix.token, &body).await {
            tracing::warn!(error = %e, "followup を送れませんでした");
        }
    }
}

#[cfg(test)]
mod tests;
