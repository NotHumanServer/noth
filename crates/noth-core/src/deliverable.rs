//! 結論メモの「成果物」欄。依頼が文章やコードそのものだったとき、Worker が全文をここに入れる。
//! main に報告を書かせると要約して置き場所だけを伝えることがあった(fakecord の評価)ので、
//! Core が切り出して、報告のあとにそのまま投稿する
use crate::supplement::strip_line_decoration;

const LABEL: &str = "成果物";
/// 成果物の後ろに別の欄が続いたとき、そこで止める
const OTHER_FIELDS: &[&str] = &[
    "分かったこと",
    "作ったもの",
    "置き場所",
    "出典",
    "不確かな点",
    "補足の要否",
    "確信",
    "種類",
];

/// 「成果物」欄の本文。欄が無い、空、または「なし」なら None。
/// コードブロックの中の行は欄の見出しとみなさない
pub fn extract(memo: &str) -> Option<String> {
    let mut in_fence = false;
    let mut body: Option<Vec<&str>> = None;
    for line in memo.lines() {
        match &mut body {
            None => {
                if !in_fence && let Some(rest) = label_rest(line, LABEL) {
                    body = Some(if rest.is_empty() { vec![] } else { vec![rest] });
                }
            }
            Some(lines) => {
                if !in_fence && OTHER_FIELDS.iter().any(|f| label_rest(line, f).is_some()) {
                    break;
                }
                lines.push(line);
            }
        }
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
    }
    let text = body?.join("\n").trim().to_string();
    (!text.is_empty() && text != "なし").then_some(text)
}

/// `label`(後ろに括弧書きがあってもよい)と区切りのコロンを外した、同じ行の残り
fn label_rest<'a>(line: &'a str, label: &str) -> Option<&'a str> {
    let mut rest = strip_line_decoration(line).strip_prefix(label)?;
    for (open, close) in [('(', ')'), ('（', '）')] {
        if let Some(r) = rest.strip_prefix(open) {
            rest = &r[r.find(close)? + close.len_utf8()..];
            break;
        }
    }
    let rest = rest.trim_start_matches(['*', ' ', '\t', '　']);
    let rest = rest.strip_prefix([':', '：'])?;
    Some(rest.trim_matches(['*', ' ', '\t', '　']))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_the_body_after_the_label_up_to_the_next_field() {
        let memo = "分かったこと：まとめた\n\n成果物：\n\n## 春\n桜が咲く。\n\n## 夏\n暑い。\n\n不確かな点: なし";
        assert_eq!(extract(memo).unwrap(), "## 春\n桜が咲く。\n\n## 夏\n暑い。");
    }

    #[test]
    fn label_may_have_a_note_and_decoration() {
        let memo =
            "- 作ったもの: main.rs\n- **成果物（`src/main.rs`）**：\n```rust\nfn main() {}\n```";
        assert_eq!(extract(memo).unwrap(), "```rust\nfn main() {}\n```");
        assert_eq!(extract("成果物: 一行の訳文").unwrap(), "一行の訳文");
    }

    #[test]
    fn lines_inside_code_are_not_field_labels() {
        let memo = "成果物:\n```text\n出典: これはコードの中\n```\n出典: https://example.com";
        assert_eq!(
            extract(memo).unwrap(),
            "```text\n出典: これはコードの中\n```"
        );
        // 成果物の前のコードブロックの中の「成果物:」も見出しではない
        assert_eq!(extract("```\n成果物: x\n```\n分かったこと: y"), None);
    }

    #[test]
    fn missing_or_empty_is_none() {
        assert_eq!(extract("分かったこと: x\n作ったもの: なし"), None);
        assert_eq!(extract("成果物: なし"), None);
        assert_eq!(extract("成果物:\n\n出典: x"), None);
        // 本文中の「成果物」という語は欄ではない
        assert_eq!(extract("成果物の置き場所は workspace"), None);
    }
}
