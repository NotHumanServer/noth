//! 黙って起動した Worker(補足・訂正)の結論メモから、報告の要否を読む。context-layers.md §4.3。
//! 投稿する文面は Worker ではなく main が書く
use crate::types::Origin;

/// 結論メモの「補足の要否」欄。Worker の指示文と Core の読み取りが同じ文字列を使う
pub const NEED_LABEL: &str = "補足の要否";

/// 結論メモに「補足の要否: 要」と書かれているか。読めなければ不要として扱う。
/// 黙って起動した Worker の誤読で、人どうしの会話に割り込まないため
pub fn needed(memo: &str) -> bool {
    memo.lines().any(line_says_needed)
}

fn line_says_needed(line: &str) -> bool {
    const DECORATION: &[char] = &['-', '*', '`', '・', '>', ' ', '\t', '　'];
    let Some(rest) = line.trim_start_matches(DECORATION).strip_prefix(NEED_LABEL) else {
        return false;
    };
    let rest = rest.trim_start_matches(['*', '`', ' ', '\t']);
    let Some(rest) = rest.strip_prefix([':', '：']) else {
        return false;
    };
    let Some(rest) = rest
        .trim_start_matches(['*', '`', ' ', '\t', '　'])
        .strip_prefix('要')
    else {
        return false;
    };
    // 「要らない」「要確認」のように、要の後ろに言葉が続くものは要の意味ではない
    rest.chars()
        .next()
        .is_none_or(|c| c.is_whitespace() || "。、，．.,;；!！)）*`」』".contains(c))
}

/// Worker の指示文に足す節。起こした経緯ごとに、何を確かめて何を結論に含めるかを変える
pub fn instruction(origin: Origin) -> Option<String> {
    let focus = match origin {
        Origin::Request => return None,
        Origin::Supplement => {
            "この run は、人どうしの会話の中の発言を黙って確かめるために起動されました。Goal の発言の具体的な主張(数字・仕様・料金・日付・固有名詞)や、答えの出ていない事実の問いを調べてください。"
        }
        Origin::Correction => {
            "この run は、Noth が前に述べたことや調べた結果の誤りを指摘する発言を受けて、黙って調べ直すために起動されました。指摘が正しいかを一次情報で確かめてください。誤りを認める場合も、根拠を添えて元の結論を保つ場合もあります。"
        }
    };
    Some(format!(
        "{focus}\n\
         調べたうえで、本当に指摘や補足が必要かを判断し、結論メモに独立した行 `{NEED_LABEL}: 要` または `{NEED_LABEL}: 不要` を必ず含めてください。\n\
         要のときは、何を補うか(または訂正するか)を、根拠の出典つきで結論メモに書いてください。あなたの文面がそのまま投稿されることはありません。main がこのメモを読んで、短い補足を自分の言葉で書きます。\n\
         会話の役に立たない、すでに会話で出ている、確信が持てない場合は「不要」にしてください。人どうしの会話に割り込むのは、間違いを放っておくより害が大きいことがあります。"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            assert!(needed(yes), "{yes}");
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
            assert!(!needed(no), "{no}");
        }
    }

    #[test]
    fn only_silent_origins_get_instruction() {
        assert!(instruction(Origin::Request).is_none());
        let s = instruction(Origin::Supplement).unwrap();
        assert!(s.contains("補足の要否: 要"));
        assert!(!s.contains("[補足]"));
        assert!(instruction(Origin::Correction).unwrap().contains("誤り"));
    }
}
