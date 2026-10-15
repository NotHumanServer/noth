//! action の一覧を 1 か所に置き、スキーマと description をここから組み立てる
use serde_json::{Value, json};

pub(crate) struct ActionDef {
    pub(crate) name: &'static str,
    pub(crate) params: &'static str,
    pub(crate) description: &'static str,
}

pub(crate) const ACTIONS: &[ActionDef] = &[
    ActionDef {
        name: "search_messages",
        params: "(channel_id?, scope?, query?, author?, since?, until?, limit?)",
        description: "Noth が記録済みの過去の発言を検索する。新しい順。チャンネルは名前でも指定できる。チャンネルを省略すると、依頼された場所のチャンネルだけを検索する(サーバー全体は scope=\"guild\")",
    },
    ActionDef {
        name: "fetch_messages",
        params: "(channel_id?, before?, after?, around?, limit?)",
        description: "チャンネルの発言を Discord から直接取得する(古い順に返す)。channel_id を省略すると、依頼された場所のチャンネルを取る",
    },
    ActionDef {
        name: "list_pins",
        params: "(channel_id)",
        description: "チャンネルにピン留めされた発言",
    },
    ActionDef {
        name: "list_channels",
        params: "()",
        description: "このサーバーのチャンネルをカテゴリ別に並べる",
    },
    ActionDef {
        name: "channel_info",
        params: "(channel_id)",
        description: "チャンネルの名前・種別・トピックなど",
    },
    ActionDef {
        name: "server_info",
        params: "()",
        description: "このサーバーの名前・説明・人数など",
    },
    ActionDef {
        name: "list_roles",
        params: "()",
        description: "このサーバーのロール一覧",
    },
    ActionDef {
        name: "member_info",
        params: "(user_id)",
        description: "メンバーの表示名・ロール・参加日時。allowed_users のユーザーだけ",
    },
];

pub(crate) fn is_known(name: &str) -> bool {
    ACTIONS.iter().any(|a| a.name == name)
}

pub(crate) fn known_names() -> Vec<&'static str> {
    ACTIONS.iter().map(|a| a.name).collect()
}

pub(crate) fn description(enabled: &[&str]) -> String {
    let list = ACTIONS
        .iter()
        .filter(|a| enabled.contains(&a.name))
        .map(|a| format!("  {}{} — {}", a.name, a.params, a.description))
        .collect::<Vec<_>>()
        .join("\n");
    let has = |n: &str| enabled.contains(&n);
    let mut guidance = String::new();
    if has("search_messages") {
        guidance.push_str(
            "過去の会話を探すときは、まず search_messages(Noth の記録。速くて絞り込める)を使う。",
        );
    }
    if has("fetch_messages") {
        guidance
            .push_str("記録にない最新の発言や、Noth が見ていない時期は fetch_messages で取る。");
    }
    format!(
        "このサーバーの Discord を読み取り専用で参照する。投稿・編集・削除はできない。\n\n\
         使える action:\n{list}\n\n\
         {guidance}ID は文字列で渡す。allowed_users 以外の発言は本文が伏せられ、author が <user> になる"
    )
}

pub(crate) fn input_schema(enabled: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {"type": "string", "enum": enabled},
            "channel_id": {
                "type": "string",
                "description": "チャンネル ID。search_messages ではチャンネル名(#noth開発計画 のように # 付きでも可)も指定できる"
            },
            "scope": {
                "type": "string",
                "enum": ["guild"],
                "description": "search_messages: \"guild\" でサーバー全体を検索する。省略時は channel_id、無ければ依頼された場所のチャンネルだけ"
            },
            "user_id": {"type": "string", "description": "member_info の対象ユーザー ID"},
            "before": {"type": "string", "description": "fetch_messages: この発言 ID より前を取る"},
            "after": {"type": "string", "description": "fetch_messages: この発言 ID より後を取る"},
            "around": {"type": "string", "description": "fetch_messages: この発言 ID の前後を取る"},
            "query": {"type": "string", "description": "search_messages: 本文に含まれる文字列(大文字小文字を区別しない)"},
            "author": {"type": "string", "description": "search_messages: 発言者の表示名に含まれる文字列"},
            "since": {
                "type": "string",
                "description": "search_messages: この日時以降。2026-10-01 または 2026-10-01T09:00:00(オフセット無しはサーバーのローカル時刻)"
            },
            "until": {
                "type": "string",
                "description": "search_messages: この日時より前。日付だけなら、その日の終わりまで含む"
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": 100,
                "description": "件数。fetch_messages は既定 50、search_messages は既定 30"
            }
        },
        "required": ["action"]
    })
}
