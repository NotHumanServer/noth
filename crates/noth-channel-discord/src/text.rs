use std::collections::HashMap;

use noth_core::types::UserId;
use serenity::all as sn;

use crate::MESSAGE_LIMIT;

/// `!` で始まる発言は Noth に向けない独り言・メモとして扱い、判定にも記録にも回さない。
/// 日本語入力のまま打つと全角になるので `！` も同じに扱う
pub(crate) fn is_aside(content: &str) -> bool {
    content.trim_start().starts_with(['!', '！'])
}

pub(crate) fn replace_mentions(
    content: &str,
    self_id: UserId,
    self_name: &str,
    allowed: &[UserId],
    names: &HashMap<UserId, String>,
) -> String {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(start) = rest.find("<@") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let digits_from = after.strip_prefix('!').unwrap_or(after);
        let digits_len = digits_from.bytes().take_while(u8::is_ascii_digit).count();
        let id = digits_from[..digits_len].parse::<u64>().ok();
        match (id, digits_from[digits_len..].strip_prefix('>')) {
            (Some(id), Some(tail)) => {
                if id == self_id {
                    out.push('@');
                    out.push_str(self_name);
                } else if allowed.contains(&id) {
                    out.push_str(names.get(&id).map_or("<user>", String::as_str));
                } else {
                    out.push_str("<user>");
                }
                rest = tail;
            }
            _ => {
                out.push_str("<@");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Discord は URL を次の空白まで自動リンクにするため、日本語の括弧や句読点が続くとリンクに巻き込まれる。
/// `<URL>` で囲むと終端が確定し、プレビューも出ない。コード・Markdown リンク・囲み済みの URL は触らない
pub(crate) fn wrap_urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        let skip = if let Some(body) = rest.strip_prefix("```") {
            Some(body.find("```").map_or(rest.len(), |p| p + 6))
        } else if let Some(body) = rest.strip_prefix('`') {
            body.find('`').map(|p| p + 2)
        } else if rest.starts_with("](") {
            Some(markdown_link_end(rest))
        } else if rest.starts_with("<http") {
            rest.find('>').map(|p| p + 1)
        } else {
            None
        };
        if let Some(n) = skip {
            out.push_str(&rest[..n]);
            i += n;
            continue;
        }
        let after_word = out
            .chars()
            .last()
            .is_some_and(|c| c.is_ascii_alphanumeric());
        if !after_word && (rest.starts_with("http://") || rest.starts_with("https://")) {
            let url = trim_url(url_candidate(rest));
            let scheme_only = url.ends_with("://");
            // `[https://x](https://x)` の表示側は Markdown リンクの一部なので囲まない
            let link_label = rest[url.len()..].starts_with("](");
            if !scheme_only && !link_label {
                out.push('<');
                out.push_str(url);
                out.push('>');
                i += url.len();
                continue;
            }
        }
        let c = rest.chars().next().expect("non-empty");
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// 編集は分割できないため、上限を超えたら切り捨てる。Core が事前に収めるので通常は到達しない安全網
pub(crate) fn prepare_edit(text: &str) -> String {
    let wrapped = wrap_urls(text);
    match wrapped.char_indices().nth(MESSAGE_LIMIT) {
        Some((i, _)) => wrapped[..i].to_string(),
        None => wrapped,
    }
}

/// `](` で始まる文字列から、対応する `)` の直後までの長さ。閉じていなければ `](` だけ
fn markdown_link_end(rest: &str) -> usize {
    let mut depth = 1;
    for (i, b) in rest.bytes().enumerate().skip(2) {
        match b {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
    }
    2
}

/// 空白・非 ASCII(日本語や全角括弧)・`<>` と backtick の手前までを URL 候補とする
fn url_candidate(rest: &str) -> &str {
    let end = rest
        .bytes()
        .position(|b| !b.is_ascii() || b.is_ascii_whitespace() || matches!(b, b'<' | b'>' | b'`'))
        .unwrap_or(rest.len());
    let candidate = &rest[..end];
    // Markdown リンクの表示側に URL を書いた場合、`](` 以降は別のリンクの URL
    &candidate[..candidate.find("](").unwrap_or(candidate.len())]
}

/// 文末の句読点と、開き括弧と対応しない閉じ括弧を URL から外す
fn trim_url(mut url: &str) -> &str {
    loop {
        let unbalanced =
            |open: char, close: char| url.matches(close).count() > url.matches(open).count();
        let trim = match url.chars().last() {
            Some('.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"') => true,
            Some(')') => unbalanced('(', ')'),
            Some(']') => unbalanced('[', ']'),
            _ => false,
        };
        if !trim {
            return url;
        }
        url = &url[..url.len() - 1];
    }
}

pub(crate) fn split_message(text: &str, limit: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        if rest.chars().count() <= limit {
            chunks.push(rest.to_string());
            break;
        }
        let window_end = rest
            .char_indices()
            .nth(limit)
            .map_or(rest.len(), |(i, _)| i);
        let window = &rest[..window_end];
        match window.rfind('\n').filter(|&i| i > 0) {
            Some(i) => {
                chunks.push(rest[..i].to_string());
                rest = &rest[i + 1..];
            }
            None => {
                chunks.push(window.to_string());
                rest = &rest[window_end..];
            }
        }
    }
    chunks.retain(|c| !c.trim().is_empty());
    chunks
}

const BOT_NAME_LIMIT: usize = 32;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AuthorKind {
    /// Noth 自身
    Own,
    Member,
    /// Noth 以外の Bot アカウント。本文は持ち込まず目印だけ流す
    Bot,
    Drop,
}

/// webhook は Bot アカウントと違い発言者を特定できないため、Bot の目印にも含めず捨てる
pub(crate) fn classify_author(
    is_self: bool,
    is_bot: bool,
    is_webhook: bool,
    is_allowed: bool,
    is_aside_msg: bool,
) -> AuthorKind {
    if is_self {
        AuthorKind::Own
    } else if is_webhook {
        AuthorKind::Drop
    } else if is_bot {
        AuthorKind::Bot
    } else if is_allowed && !is_aside_msg {
        AuthorKind::Member
    } else {
        AuthorKind::Drop
    }
}

pub(crate) fn bot_display_name(name: &str) -> String {
    // `[Bot …]` の目印の行を名前で装えないよう、括弧は全角にする
    let flat = name
        .replace(['\r', '\n'], " ")
        .replace('[', "［")
        .replace(']', "］");
    flat.trim().chars().take(BOT_NAME_LIMIT).collect()
}

pub(crate) fn message_chars(content: &str, embeds: &[sn::Embed]) -> usize {
    let embed_chars: usize = embeds
        .iter()
        .map(|e| {
            let fields: usize = e
                .fields
                .iter()
                .map(|f| f.name.chars().count() + f.value.chars().count())
                .sum();
            e.title.as_deref().map_or(0, |t| t.chars().count())
                + e.description.as_deref().map_or(0, |d| d.chars().count())
                + fields
        })
        .sum();
    content.chars().count() + embed_chars
}

#[cfg(test)]
mod tests;
