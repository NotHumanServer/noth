#[derive(PartialEq, Eq, Clone, Copy)]
enum Script {
    AsciiWord,
    Hiragana,
    Katakana,
    Kanji,
    Other,
}

fn script_of(c: char) -> Script {
    match c {
        'a'..='z' | 'A'..='Z' | '0'..='9' | '_' => Script::AsciiWord,
        '\u{3040}'..='\u{309F}' => Script::Hiragana,
        // 長音符 ー (U+30FC) は直前の語がカタカナなので同じ字種として扱う
        '\u{30A0}'..='\u{30FF}' | '\u{FF66}'..='\u{FF9F}' => Script::Katakana,
        '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}' => Script::Kanji,
        _ => Script::Other,
    }
}

/// 単語の一部(nothing の noth、ノスタルジーのノス)を拾わないよう、
/// 前後が同じ字種で続いている一致は除外する。ただし後ろに続くひらがなは「くん」「さん」や助詞なので切れ目とみなす
pub fn contains_word(content: &str, keyword: &str) -> bool {
    let (Some(first), Some(last)) = (keyword.chars().next(), keyword.chars().next_back()) else {
        return false;
    };
    let (head, tail) = (script_of(first), script_of(last));
    let joined = |neighbor: Option<char>, edge: Script| {
        edge != Script::Other && neighbor.is_some_and(|c| script_of(c) == edge)
    };
    content.match_indices(keyword).any(|(i, m)| {
        let before = content[..i].chars().next_back();
        let after = content[i + m.len()..].chars().next();
        !joined(before, head) && (tail == Script::Hiragana || !joined(after, tail))
    })
}

/// 呼び名は大文字小文字を区別しない
pub fn contains_any_name(content: &str, names: &[String]) -> bool {
    let content = content.to_lowercase();
    names
        .iter()
        .any(|n| contains_word(&content, &n.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hiragana_name_allows_honorific() {
        let names = vec!["のす".to_string(), "noth".to_string()];
        assert!(contains_any_name("のすく〜ん？", &names));
        assert!(contains_any_name("のすさん", &names));
        assert!(!contains_any_name("ものすごい", &names));
        assert!(!contains_any_name("nothing", &names));
    }
}
