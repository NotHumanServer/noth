//! `/noth ignore add|remove|list` の定義と応答文
use noth_core::command::{Report, ReportSection};
use noth_core::exclusion::{AddOutcome, ExclusionList, RemoveOutcome};
use serde_json::{Value, json};

use super::{CHANNEL_OPTION, IGNORE_GROUP};

/// 選べるチャンネルの種類。Discord の Channel Types の値で、
/// テキスト 0・ボイス 2・カテゴリ 4・アナウンス 5・スレッド 10〜12・ステージ 13・フォーラム 15・メディア 16
const CHANNEL_TYPES: [u8; 10] = [0, 2, 4, 5, 10, 11, 12, 13, 15, 16];

pub(super) fn definition() -> Value {
    let channel = |description: &str| {
        json!([{
            "type": 7,
            "name": CHANNEL_OPTION,
            "description": description,
            "required": true,
            "channel_types": CHANNEL_TYPES,
        }])
    };
    json!({
        "type": 2,
        "name": IGNORE_GROUP,
        "description": "Noth が無視するチャンネルを管理する",
        "options": [
            {
                "type": 1,
                "name": "add",
                "description": "チャンネルまたはカテゴリを除外に足す",
                "options": channel("除外するチャンネルまたはカテゴリ"),
            },
            {
                "type": 1,
                "name": "remove",
                "description": "コマンドで足した除外を外す",
                "options": channel("除外から外すチャンネルまたはカテゴリ"),
            },
            {
                "type": 1,
                "name": "list",
                "description": "いまの除外の一覧を見る",
            },
        ],
    })
}

fn mention(id: u64) -> String {
    format!("<#{id}>")
}

pub(super) fn added_message(id: u64, outcome: &AddOutcome) -> String {
    let ch = mention(id);
    match outcome {
        AddOutcome::Added => format!(
            "{ch} を除外に追加しました。カテゴリなら配下のチャンネル、チャンネルならそのスレッドも対象です"
        ),
        AddOutcome::AlreadyInConfig => {
            format!("{ch} は config の ignored_channels で、すでに除外されています")
        }
        AddOutcome::AlreadyCommanded => format!("{ch} は、すでに除外されています"),
    }
}

pub(super) fn removed_message(id: u64, outcome: &RemoveOutcome) -> String {
    let ch = mention(id);
    match outcome {
        RemoveOutcome::Removed => format!("{ch} を除外から外しました"),
        RemoveOutcome::RemovedStillInConfig => format!(
            "コマンドで足した {ch} を外しましたが、config の ignored_channels にも書かれているため、除外は続きます"
        ),
        RemoveOutcome::ConfigOnly => format!(
            "{ch} は config の ignored_channels で除外されているため、コマンドでは外せません。config から消して再起動してください"
        ),
        RemoveOutcome::NotIgnored => format!("{ch} は、除外されていません"),
    }
}

pub(super) fn list_report(list: &ExclusionList) -> Report {
    let section = |heading: &str, ids: &[u64]| ReportSection {
        heading: format!("{heading}({}件)", ids.len()),
        body: if ids.is_empty() {
            "なし".into()
        } else {
            ids.iter()
                .map(|id| format!("- {} `{id}`", mention(*id)))
                .collect::<Vec<_>>()
                .join("\n")
        },
    };
    Report {
        title: "チャンネルの除外".into(),
        sections: vec![
            section("config 由来(コマンドでは外せません)", &list.config),
            section("コマンド由来", &list.commanded),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definition_offers_text_and_category_channels() {
        let d = definition();
        let add = &d["options"][0];
        assert_eq!(add["options"][0]["type"], 7);
        let types = add["options"][0]["channel_types"].as_array().unwrap();
        assert!(types.contains(&json!(0)) && types.contains(&json!(4)));
        assert_eq!(add["options"][0]["required"], true);
        assert!(d["options"][2].get("options").is_none());
    }

    #[test]
    fn list_labels_origin_and_marks_empty_sections() {
        let r = list_report(&ExclusionList {
            config: vec![5],
            commanded: vec![],
        });
        assert!(r.sections[0].heading.starts_with("config 由来"));
        assert!(r.sections[0].body.contains("<#5>"));
        assert_eq!(r.sections[1].body, "なし");
    }
}
