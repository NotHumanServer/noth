//! 黙って起動した Worker(補足・訂正)の結論メモから、報告の要否を読む。context-layers.md §4.3。
//! 投稿する文面は Worker ではなく main が書く
use crate::types::Origin;

/// 結論メモの「補足の要否」欄。Worker の指示文と Core の読み取りが同じ文字列を使う
pub const NEED_LABEL: &str = "補足の要否";
/// 結論メモの「確信」欄。この結論を断定してよいかの Worker の自己評価
pub const CONFIDENCE_LABEL: &str = "確信";
/// 結論メモの「種類」欄。何を投稿しようとしているかで、投稿に必要な確信を変える
pub const KIND_LABEL: &str = "種類";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Need {
    No,
    Yes,
}

/// 小さいほど厳しい側。複数の行で値が食い違うときは最小を採る
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Confidence {
    Low,
    Mid,
    High,
}

/// 小さいほど厳しい側
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Rebuttal,
    Addition,
    Acknowledge,
}

/// 結論メモを読んだ結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Post,
    NotNeeded,
    /// 要だが投稿しない。理由はログに出す
    Hold(String),
}

/// 結論メモの「補足の要否」「確信」「種類」から、投稿に回すかを決める。
/// 人の発言への反論は断定できるときだけ出したいので、読めない欄は厳しい側に倒す
pub fn decide(memo: &str, origin: Origin) -> Verdict {
    if read_field(memo, &[NEED_LABEL], need_of) != Some(Need::Yes) {
        return Verdict::NotNeeded;
    }
    let confidence = read_field(memo, &[CONFIDENCE_LABEL], confidence_of);
    // 認めるは訂正のときだけの種類。補足に書かれても反論と同じ厳しさで扱う
    let kind = match read_field(memo, &[KIND_LABEL], kind_of) {
        Some(Kind::Acknowledge) if origin != Origin::Correction => None,
        // 訂正は新しい主張を足さない方針なので、追加も反論と同じ厳しさにする
        Some(Kind::Addition) if origin == Origin::Correction => Some(Kind::Rebuttal),
        k => k,
    };
    let enough = match kind.unwrap_or(Kind::Rebuttal) {
        Kind::Acknowledge => true,
        Kind::Addition => confidence >= Some(Confidence::Mid),
        Kind::Rebuttal => confidence == Some(Confidence::High),
    };
    if enough {
        return Verdict::Post;
    }
    let kind_note = match kind {
        Some(Kind::Addition) => "追加",
        Some(_) => "反論",
        None => "種類が読めないため反論",
    };
    let confidence_note = match confidence {
        Some(Confidence::High) => "高",
        Some(Confidence::Mid) => "中",
        Some(Confidence::Low) => "低",
        None => "読めない",
    };
    Verdict::Hold(format!(
        "{kind_note}として確信が足りない(確信: {confidence_note})"
    ))
}

/// `labels` のどれかで始まる行から値を読み、読めた値のうちいちばん厳しいものを返す。読めなければ None
fn read_field<T: Ord>(memo: &str, labels: &[&str], parse: impl Fn(&str) -> Option<T>) -> Option<T> {
    memo.lines()
        .filter_map(|line| field_rest(line, labels))
        .filter_map(parse)
        .min()
}

/// 行頭の装飾とラベル(と「度」「レベル」の揺れ)、区切りを外した残り
fn field_rest<'a>(line: &'a str, labels: &[&str]) -> Option<&'a str> {
    let mut rest = strip_line_decoration(line);
    rest = labels.iter().find_map(|l| rest.strip_prefix(l))?;
    for suffix in ["度", "レベル"] {
        if let Some(r) = rest.strip_prefix(suffix) {
            rest = r;
            break;
        }
    }
    let rest = rest.trim_start_matches(['*', '`', ' ', '\t', '　']);
    let rest = rest.strip_prefix([':', '：', '|', '｜'])?;
    Some(rest.trim_start_matches(['*', '`', '|', '｜', ' ', '\t', '　']))
}

fn strip_line_decoration(line: &str) -> &str {
    const DECORATION: &[char] = &['-', '*', '`', '・', '>', '#', '|', '｜', ' ', '\t', '　'];
    let mut s = line.trim_start_matches(DECORATION);
    // 「1. 」「2) 」のような番号
    while let Some(after_digits) = {
        let t = s.trim_start_matches(|c: char| c.is_ascii_digit());
        (t.len() != s.len()).then_some(t)
    } {
        match after_digits.strip_prefix(['.', '．', ')', '）']) {
            Some(r) => s = r.trim_start_matches(DECORATION),
            None => break,
        }
    }
    s
}

/// `value` で始まり、その直後が値の一部に見える文字(「高め」「要否」の め や 否)ではなく、
/// 「高 / 中 / 低」のような雛形の写しでもないときだけ true
fn is_value(rest: &str, value: &str, all_values: &[&str]) -> bool {
    let Some(after) = rest.strip_prefix(value) else {
        return false;
    };
    if after.chars().next().is_some_and(is_word_char) {
        return false;
    }
    let tail = after.trim_start();
    if let Some(next) = tail.strip_prefix(['/', '／', '・', '|', '｜']) {
        let next = next.trim_start();
        if all_values.iter().any(|v| next.starts_with(v)) {
            return false;
        }
    }
    true
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || ('\u{4e00}'..='\u{9fff}').contains(&c)
        || ('\u{3040}'..='\u{309f}').contains(&c)
        || (('\u{30a0}'..='\u{30ff}').contains(&c) && c != '\u{30fb}')
}

fn need_of(rest: &str) -> Option<Need> {
    const ALL: &[&str] = &["要", "不要"];
    if is_value(rest, "不要", ALL) {
        Some(Need::No)
    } else if is_value(rest, "要", ALL) {
        Some(Need::Yes)
    } else {
        None
    }
}

fn confidence_of(rest: &str) -> Option<Confidence> {
    const ALL: &[&str] = &["高", "中", "低"];
    [
        ("高", Confidence::High),
        ("中", Confidence::Mid),
        ("低", Confidence::Low),
    ]
    .into_iter()
    .find_map(|(v, c)| is_value(rest, v, ALL).then_some(c))
}

fn kind_of(rest: &str) -> Option<Kind> {
    const ALL: &[&str] = &["反論", "追加", "認める"];
    [
        ("反論", Kind::Rebuttal),
        ("追加", Kind::Addition),
        ("認める", Kind::Acknowledge),
    ]
    .into_iter()
    .find_map(|(v, k)| is_value(rest, v, ALL).then_some(k))
}

const VOLATILE_RULES: &str = "\
         - 料金、無料枠、提供モデル、版のように変わりやすい話では、発言者のほうが Noth の知識や検索結果より新しい情報を持っている前提で調べてください。\n\
         - 根拠は、最終更新日や告知日の分かる一次情報にしてください。ページに載っていないことを「無い」の根拠にしないでください。\n\
         - 発言に反論する補足になるなら、反証も探してください。料金や無料枠なら、プランのページだけでなく、無料枠、公式の告知、changelog のページも見てください。\n\
         - 「〜なら」という仮定の発言に対して、仮定そのものを否定する補足は不要です。\n\
         - 「〜とは限らない」「〜かもしれない」「〜みたい」の形でしか言えないことは不要です。";

/// Worker の指示文に足す節。起こした経緯ごとに、何を確かめて何を結論に含めるかを変える
pub fn instruction(origin: Origin) -> Option<String> {
    let focus = match origin {
        Origin::Request => return None,
        Origin::Supplement => {
            "この run は、人どうしの会話の中の発言を黙って確かめるために起動されました。Goal の発言の具体的な主張(数字・仕様・料金・日付・固有名詞)や、答えの出ていない事実の問いを調べてください。"
        }
        Origin::Correction => {
            "この run は、Noth が前に述べたことや調べた結果の誤りを指摘する発言を受けて、黙って調べ直すために起動されました。指摘が正しいかを一次情報で確かめてください。相手の指摘が正しければ、そのことだけを結論にし(`補足の要否: 要`、`種類: 認める` と書いてください)、新しい主張を足さないでください。相手が誤りだと言い返すのは、下の基準で断定できるときだけです。"
        }
    };
    Some(format!(
        "{focus}\n\
         調べたうえで、本当に指摘や補足が必要かを判断し、結論メモに独立した行 `{NEED_LABEL}: 要` または `{NEED_LABEL}: 不要` を必ず含めてください。\n\
         あわせて、独立した行を 2 つ含めてください。\n\
         - `{CONFIDENCE_LABEL}: 高`、`{CONFIDENCE_LABEL}: 中`、`{CONFIDENCE_LABEL}: 低` のどれか 1 つ。この結論を断定してよいかの自己評価です。\n\
         - `{KIND_LABEL}: 反論`、`{KIND_LABEL}: 追加`、`{KIND_LABEL}: 認める` のどれか 1 つ。反論は人の発言が誤りだと言うこと、追加は答えの出ていない問いに答えるか情報を足すこと、認めるは訂正のときに相手の指摘が正しかったと認めることです。\n\
         要のときは、何を補うか(または訂正するか)を、根拠の出典つきで結論メモに書いてください。あなたの文面がそのまま投稿されることはありません。main がこのメモを読んで、短い補足を自分の言葉で書きます。\n\
         基準:\n{VOLATILE_RULES}\n\
         会話の役に立たない、すでに会話で出ている、断定できない場合は「不要」にしてください。人どうしの会話に割り込むのは、間違いを放っておくより害が大きいことがあります。"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post(memo: &str) -> bool {
        decide(memo, Origin::Supplement) == Verdict::Post
    }

    const HIGH: &str = "\n確信: 高";

    #[test]
    fn reads_needed_only_when_the_word_ends_there() {
        for yes in [
            "補足の要否: 要",
            "補足の要否:要。理由は…",
            "- 補足の要否: 要",
            "* **補足の要否:** 要",
            "`補足の要否: 要`",
            "補足の要否：要",
            "  補足の要否 : 要、料金が違う",
            "調べた\n補足の要否: 要\n根拠: …",
        ] {
            assert!(post(&format!("{yes}{HIGH}")), "{yes}");
        }
        for no in [
            "補足の要否: 不要",
            "補足の要否: 要らない",
            "補足の要否: 要確認",
            "補足の要否: 要否は判断できない",
            "補足は要る",
            "何も書いていない",
            "補足の要否",
        ] {
            assert!(!post(&format!("{no}{HIGH}")), "{no}");
        }
    }

    #[test]
    fn rebuttal_needs_high_confidence() {
        for yes in [
            "補足の要否: 要\n確信: 高",
            "補足の要否: 要\n- **確信:** 高",
            "補足の要否: 要\n確信：高。一次情報で確認",
            "`確信: 高`\n補足の要否: 要",
            "補足の要否: 要\n種類: 反論\n確信: 高",
        ] {
            assert!(post(yes), "{yes}");
        }
        for no in [
            "補足の要否: 要\n確信: 中",
            "補足の要否: 要\n確信: 低",
            "補足の要否: 要\n確信: 高め",
            "補足の要否: 要\n確信",
            "補足の要否: 要\n確信度は高い",
            "補足の要否: 要",
            "確信: 高",
            "補足の要否: 不要\n確信: 高",
            "補足の要否: 要\n種類: 反論\n確信: 中",
        ] {
            assert!(!post(no), "{no}");
        }
    }

    #[test]
    fn confidence_line_survives_formatting_noise() {
        for line in [
            "確信: 高（一次情報あり）",
            "確信: 高(一次情報あり)",
            "確信度: 高",
            "* **確信度**: 高",
            "確信レベル: 高",
            "## 確信: 高",
            "1. 確信: 高",
            "| 確信 | 高 |",
            "確信: 高—",
        ] {
            assert!(post(&format!("補足の要否: 要\n{line}")), "{line}");
        }
        for line in ["確信: 高 / 中 / 低", "確信: 高め", "確信: 高い", "確信: 中"] {
            assert!(!post(&format!("補足の要否: 要\n{line}")), "{line}");
        }
    }

    #[test]
    fn disagreeing_lines_take_the_strictest_value() {
        assert!(!post("補足の要否: 要\n確信: 中\n確信: 高"));
        assert!(!post("補足の要否: 要\n確信: 高\n確信: 中"));
        assert!(!post("補足の要否: 要\n補足の要否: 不要\n確信: 高"));
        assert!(!post("補足の要否: 要\n確信: 中\n種類: 追加\n種類: 反論"));
    }

    #[test]
    fn addition_posts_at_mid_and_acknowledge_regardless_of_confidence() {
        let memo = |kind: &str, conf: &str| format!("補足の要否: 要\n種類: {kind}\n確信: {conf}");
        assert!(decide(&memo("追加", "高"), Origin::Supplement) == Verdict::Post);
        assert!(decide(&memo("追加", "中"), Origin::Supplement) == Verdict::Post);
        assert!(matches!(
            decide(&memo("追加", "低"), Origin::Supplement),
            Verdict::Hold(_)
        ));
        for conf in ["高", "中", "低"] {
            assert!(decide(&memo("認める", conf), Origin::Correction) == Verdict::Post);
        }
        assert!(decide("補足の要否: 要\n種類: 認める", Origin::Correction) == Verdict::Post);
        // 補足に認めるは無いので、反論と同じ厳しさにする
        assert!(matches!(
            decide(&memo("認める", "中"), Origin::Supplement),
            Verdict::Hold(_)
        ));
    }

    #[test]
    fn correction_treats_addition_like_rebuttal() {
        let memo = |conf: &str| format!("補足の要否: 要\n種類: 追加\n確信: {conf}");
        assert!(matches!(
            decide(&memo("中"), Origin::Correction),
            Verdict::Hold(_)
        ));
        assert!(decide(&memo("高"), Origin::Correction) == Verdict::Post);
    }

    #[test]
    fn unreadable_kind_is_treated_as_rebuttal() {
        for kind in [
            "種類: 反論 / 追加 / 認める",
            "種類: 追加の情報",
            "種類は追加",
        ] {
            let memo = format!("補足の要否: 要\n{kind}\n確信: 中");
            let Verdict::Hold(reason) = decide(&memo, Origin::Correction) else {
                panic!("{kind}");
            };
            assert!(reason.contains("反論"), "{reason}");
        }
        assert!(decide("補足の要否: 要\n確信: 高", Origin::Correction) == Verdict::Post);
        assert_eq!(
            decide("補足の要否: 不要", Origin::Correction),
            Verdict::NotNeeded
        );
    }

    #[test]
    fn only_silent_origins_get_instruction() {
        assert!(instruction(Origin::Request).is_none());
        let s = instruction(Origin::Supplement).unwrap();
        assert!(s.contains("補足の要否: 要"));
        assert!(!s.contains("[補足]"));
        assert!(instruction(Origin::Correction).unwrap().contains("誤り"));
    }

    #[test]
    fn instruction_carries_the_criteria_without_revealing_the_gate() {
        for origin in [Origin::Supplement, Origin::Correction] {
            let s = instruction(origin).unwrap();
            for key in [
                "確信: 高",
                "断定してよいかの自己評価",
                "種類: 反論",
                "種類: 追加",
                "種類: 認める",
                "発言者のほうが Noth の知識や検索結果より新しい情報",
                "一次情報",
                "ページに載っていないことを「無い」の根拠にしない",
                "反証も探して",
                "changelog",
                "仮定そのものを否定する補足は不要",
                "「〜とは限らない」",
            ] {
                assert!(s.contains(key), "{origin:?}: {key}");
            }
            assert!(!s.contains("確信が高でなければ"), "{origin:?}");
            assert!(!s.contains("投稿されます"), "{origin:?}");
        }
        let c = instruction(Origin::Correction).unwrap();
        assert!(c.contains("指摘が正しければ、そのことだけを結論にし"));
        assert!(c.contains("新しい主張を足さないでください"));
        assert!(c.contains("言い返すのは、下の基準で断定できるときだけ"));
        assert!(c.contains("`補足の要否: 要`、`種類: 認める` と書いて"));
        assert!(
            !instruction(Origin::Supplement)
                .unwrap()
                .contains("言い返す")
        );
    }
}
