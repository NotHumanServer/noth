//! `/noth <command>`。master だけが使え、結果は実行した本人にだけ見える(ephemeral)
use std::sync::Arc;

use noth_core::command::{CommandHost, Report};
use noth_core::exclusion::ChannelExclusions;
use serde_json::{Value, json};

mod api;
mod ignore;
mod render;

#[cfg(test)]
pub(crate) use api::ApiError;
pub(crate) use api::{InteractionApi, RestInteractionApi};
use render::{FLAG_EPHEMERAL, embed_message, parse_refresh, plain, text_message, v2_message};

/// Discord 側のコマンド名。サブコマンドが各 `Command::name()` になる
pub(crate) const ROOT: &str = "noth";
/// `/noth ignore add|remove|list`。引数を取って状態を変えるので `Command` ではなく Channel が直接受け持つ
pub(crate) const IGNORE_GROUP: &str = "ignore";
pub(crate) const CHANNEL_OPTION: &str = "channel";
const DENIED: &str = "権限がありません";
const UNKNOWN_COMMAND: &str = "未知のコマンドです";
/// 内部のエラー文は画面に出さず、ログにだけ残す
const FAILED: &str = "取得に失敗しました。ログを確認してください";
const SAVE_FAILED: &str = "保存に失敗しました。ログを確認してください(除外は変わっていません)";
const REFRESH_FAILED: &str = "更新に失敗しました。ログを確認してください";

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
    Slash {
        name: String,
        sub: Option<String>,
    },
    /// `/noth ignore <op> [channel]`。add / remove では channel が必須
    Ignore {
        op: IgnoreOp,
        channel: Option<u64>,
    },
    Button {
        custom_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IgnoreOp {
    Add,
    Remove,
    List,
}

impl IgnoreOp {
    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "add" => Some(Self::Add),
            "remove" => Some(Self::Remove),
            "list" => Some(Self::List),
            _ => None,
        }
    }
}

/// 同じ名前のサブコマンドを送ると Discord が登録全体を拒否し、`/noth` がすべて使えなくなる。先に出てきた方を残すので、
/// 呼び出し側は残したいものを先に並べる(`/noth ignore` は除外の仕組みそのものなので組み込みを先にする)
fn drop_duplicate_names(subs: Vec<Value>) -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    subs.into_iter()
        .filter(|s| {
            let name = s["name"].as_str().unwrap_or_default().to_string();
            let fresh = seen.insert(name.clone());
            if !fresh {
                tracing::warn!(
                    name,
                    "/noth のサブコマンド名が重複しているため、後のもの(組み込みの ignore があるときは command module の方)は登録しません"
                );
            }
            fresh
        })
        .collect()
}

pub(crate) struct Controller {
    api: Arc<dyn InteractionApi>,
    host: Arc<CommandHost>,
    master: Option<u64>,
    guild_id: u64,
    exclusions: ChannelExclusions,
}

impl Controller {
    pub(crate) fn new(
        api: Arc<dyn InteractionApi>,
        host: Arc<CommandHost>,
        master: Option<u64>,
        guild_id: u64,
        exclusions: ChannelExclusions,
    ) -> Self {
        Self {
            api,
            host,
            master,
            guild_id,
            exclusions,
        }
    }

    /// master が未設定なら誰にも使えないので、コマンド自体を出さない
    fn definitions(&self) -> Value {
        if self.master.is_none() {
            return json!([]);
        }
        let mut subs = vec![ignore::definition()];
        subs.extend(
            self.host
                .commands()
                .iter()
                .map(|c| json!({"type": 1, "name": c.name(), "description": c.description()})),
        );
        let subs = drop_duplicate_names(subs);
        json!([{
            "type": 1,
            "name": ROOT,
            "description": "Noth の管理コマンド",
            "dm_permission": false,
            "options": subs,
        }])
    }

    /// master がいれば `ignore` を常に登録し、いなければ空配列を送る。PUT はこの application の guild コマンドをすべて置き換えるので、
    /// master を消したときに、前回登録した `/noth` が残らない。
    /// 代わりに、Noth 以外が登録した guild コマンドも消える(今の Noth には他のコマンドが無い)
    pub(crate) async fn sync(&self) {
        if self.master.is_none() {
            tracing::info!(
                "channel.discord.master が未設定のため、/noth コマンドは登録せず、誰にも使えません"
            );
        }
        let definitions = self.definitions();
        if let Err(e) = self.api.sync_commands(&definitions).await {
            tracing::warn!(
                error = %e,
                "スラッシュコマンドを登録できませんでした。Bot の招待に applications.commands スコープが含まれているか確認してください(docs/discord-setup.md)"
            );
            return;
        }
        let names: Vec<&str> = definitions
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| d["name"].as_str())
            .collect();
        if names.is_empty() {
            tracing::info!(guild = self.guild_id, "スラッシュコマンドを消しました");
        } else {
            tracing::info!(
                guild = self.guild_id,
                count = names.len(),
                ?names,
                "スラッシュコマンドを登録しました"
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
                _ => self.reply_ephemeral(&ix, UNKNOWN_COMMAND).await,
            },
            Kind::Slash { .. } => self.reply_ephemeral(&ix, UNKNOWN_COMMAND).await,
            Kind::Ignore { op, channel } => self.ignore(&ix, *op, *channel).await,
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
        if !self.defer(ix).await {
            return;
        }
        let report = match self.host.run(command).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, command, "コマンドの実行に失敗しました");
                self.edit_or_log(ix, &plain(FAILED)).await;
                return;
            }
        };
        self.deliver_first(ix, &report, Some(command)).await;
    }

    async fn ignore(&self, ix: &Incoming, op: IgnoreOp, channel: Option<u64>) {
        match (op, channel) {
            (IgnoreOp::List, _) => self.ignore_list(ix).await,
            (_, None) => {
                self.reply_ephemeral(ix, "チャンネルを指定してください")
                    .await
            }
            (op, Some(id)) => self.ignore_change(ix, op, id).await,
        }
    }

    /// 保存は 3 秒の受付期限に収まる保証がないので、`list` と同じく先に defer する
    async fn ignore_change(&self, ix: &Incoming, op: IgnoreOp, id: u64) {
        if !self.defer(ix).await {
            return;
        }
        let text = if op == IgnoreOp::Add {
            match self.exclusions.add(id, ix.user_id).await {
                Ok(outcome) => ignore::added_message(id, &outcome),
                Err(e) => {
                    tracing::warn!(error = %e, channel = id, "除外の追加を保存できませんでした");
                    SAVE_FAILED.to_string()
                }
            }
        } else {
            match self.exclusions.remove(id).await {
                Ok(outcome) => ignore::removed_message(id, &outcome),
                Err(e) => {
                    tracing::warn!(error = %e, channel = id, "除外の解除を保存できませんでした");
                    SAVE_FAILED.to_string()
                }
            }
        };
        self.edit_or_log(ix, &plain(&text)).await;
    }

    async fn defer(&self, ix: &Incoming) -> bool {
        let defer = json!({"flags": FLAG_EPHEMERAL});
        self.callback(ix, CALLBACK_DEFER_MESSAGE, Some(defer)).await
    }

    async fn ignore_list(&self, ix: &Incoming) {
        if !self.defer(ix).await {
            return;
        }
        let report = ignore::list_report(&self.exclusions.list());
        self.deliver_first(ix, &report, None).await;
    }

    /// defer 済みの応答はまだ V2 ではないので、V2 が拒否されても embed、さらに文字へ落とせる
    async fn deliver_first(&self, ix: &Incoming, report: &Report, command: Option<&str>) {
        let attempts = [
            ("Components V2", v2_message(report, command)),
            ("embed", embed_message(report)),
            ("テキスト", text_message(report)),
        ];
        for (label, body) in attempts {
            match self.api.edit_original(&ix.token, &body).await {
                Ok(()) => return,
                Err(e) => tracing::warn!(error = %e, label, "コマンドの結果を送れませんでした"),
            }
        }
        tracing::error!(?command, "コマンドの結果をどの形式でも送れませんでした");
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
                self.followup(ix, plain(REFRESH_FAILED)).await;
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
