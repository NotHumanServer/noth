use std::collections::HashMap;

use noth_core::traits::{Answer, Question};

fn in_unit(p: f32) -> bool {
    (0.0..=1.0).contains(&p)
}

/// 要求していない形・範囲外の値は不正として理由を返す
fn validate(answer: &Answer, question: &Question) -> std::result::Result<(), String> {
    match (answer, question) {
        (Answer::Noul { p }, Question::Noul { .. }) if in_unit(*p) => Ok(()),
        (Answer::Noul { p }, Question::Noul { .. }) => Err(format!("noul {p} が範囲外")),
        (
            Answer::Choice {
                choice,
                probabilities,
                confidence,
            },
            Question::Choice { criteria, .. },
        ) => {
            if !criteria.iter().any(|(n, _)| n == choice) {
                return Err(format!("選択肢にない choice `{choice}`"));
            }
            if !in_unit(*confidence) || !probabilities.values().all(|p| in_unit(*p)) {
                return Err("確率または confidence が範囲外".into());
            }
            Ok(())
        }
        (
            Answer::Score {
                score,
                probabilities,
                confidence,
            },
            Question::Score { .. },
        ) => {
            // score の値域は仕様が不明(既存のサンプルは 1.6)なので、負値と非有限値だけ弾く
            if !score.is_finite() || *score < 0.0 {
                return Err(format!("score {score} が範囲外"));
            }
            if !in_unit(*confidence) || !probabilities.values().all(|p| in_unit(*p)) {
                return Err("確率または confidence が範囲外".into());
            }
            Ok(())
        }
        _ => Err("質問と回答の type が一致しない".into()),
    }
}

/// 欠落・不正な回答は map に入れない。core の二値化は欠けた回答を安全側に倒す
pub(crate) fn validated_answers(
    mut parsed: HashMap<String, Answer>,
    questions: &[(String, Question)],
) -> HashMap<String, Answer> {
    let mut out = HashMap::new();
    for (key, question) in questions {
        match parsed.remove(key) {
            None => tracing::warn!(key, "systemone の回答に要求したキーがありません"),
            Some(answer) => match validate(&answer, question) {
                Ok(()) => {
                    out.insert(key.clone(), answer);
                }
                Err(reason) => tracing::warn!(key, reason, "systemone の回答を破棄します"),
            },
        }
    }
    out
}
