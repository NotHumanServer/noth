use super::*;

fn names() -> HashMap<UserId, String> {
    HashMap::from([(1, "alice".to_string()), (3, "carol".to_string())])
}

fn replace(s: &str) -> String {
    replace_mentions(s, 99, "Noth", &[1, 2], &names())
}

#[test]
fn mention_allowed_user_uses_display_name() {
    assert_eq!(replace("hi <@1> and <@!1>"), "hi alice and alice");
}

#[test]
fn mention_allowed_user_without_name_is_masked() {
    assert_eq!(replace("<@2>"), "<user>");
}

#[test]
fn mention_unknown_user_is_masked_even_if_name_known() {
    assert_eq!(replace("<@3>"), "<user>");
}

#[test]
fn mention_self_becomes_at_name() {
    assert_eq!(replace("<@99> help"), "@Noth help");
}

#[test]
fn role_everyone_and_malformed_are_kept() {
    assert_eq!(
        replace("<@&5> @everyone <@abc> <@1 <@"),
        "<@&5> @everyone <@abc> <@1 <@"
    );
}

#[test]
fn wraps_url_inside_japanese_parens() {
    assert_eq!(
        wrap_urls("公式サイト(https://www.tinyfish.ai/)を見る"),
        "公式サイト(<https://www.tinyfish.ai/>)を見る"
    );
    assert_eq!(
        wrap_urls("https://a.example/pathを参照"),
        "<https://a.example/path>を参照"
    );
    assert_eq!(
        wrap_urls("（https://a.example）です"),
        "（<https://a.example>）です"
    );
}

#[test]
fn trims_trailing_punctuation() {
    assert_eq!(
        wrap_urls("見て https://a.example/x."),
        "見て <https://a.example/x>."
    );
    assert_eq!(
        wrap_urls("https://a.example/x?!"),
        "<https://a.example/x>?!"
    );
    assert_eq!(
        wrap_urls("\"https://a.example/x\","),
        "\"<https://a.example/x>\","
    );
}

#[test]
fn keeps_balanced_parens() {
    assert_eq!(
        wrap_urls("https://ja.wikipedia.org/wiki/A_(b)"),
        "<https://ja.wikipedia.org/wiki/A_(b)>"
    );
    assert_eq!(
        wrap_urls("(https://ja.wikipedia.org/wiki/A_(b))"),
        "(<https://ja.wikipedia.org/wiki/A_(b)>)"
    );
}

#[test]
fn leaves_wrapped_markdown_and_code_untouched() {
    for s in [
        "<https://a.example/x>",
        "[見出し](https://a.example/x)",
        "[https://a.example](https://a.example)",
        "```\nhttps://a.example\n```",
        "`https://a.example`",
    ] {
        assert_eq!(wrap_urls(s), s);
    }
    assert_eq!(
        wrap_urls("`x` https://a.example [t](https://b.example) https://c.example"),
        "`x` <https://a.example> [t](https://b.example) <https://c.example>"
    );
}

#[test]
fn wraps_multiple_and_ignores_bare_scheme() {
    assert_eq!(
        wrap_urls("A https://a.example と B http://b.example/ です"),
        "A <https://a.example> と B <http://b.example/> です"
    );
    assert_eq!(wrap_urls("https:// だけ"), "https:// だけ");
    assert_eq!(wrap_urls("日本語のみ"), "日本語のみ");
}

#[test]
fn prepare_edit_wraps_urls_and_cuts_at_limit() {
    assert_eq!(
        prepare_edit("進捗(https://a.example)です"),
        "進捗(<https://a.example>)です"
    );
    let long = "あ".repeat(MESSAGE_LIMIT + 50);
    assert_eq!(prepare_edit(&long).chars().count(), MESSAGE_LIMIT);
    assert_eq!(prepare_edit("短い"), "短い");
}

#[test]
fn split_short_is_single() {
    assert_eq!(split_message("abc", 2000), vec!["abc"]);
    assert!(split_message("", 2000).is_empty());
}

#[test]
fn split_prefers_newline() {
    let text = format!("{}\n{}", "a".repeat(10), "b".repeat(10));
    assert_eq!(
        split_message(&text, 15),
        vec!["a".repeat(10), "b".repeat(10)]
    );
}

#[test]
fn split_drops_whitespace_only_chunks() {
    let text = format!("{}\n{}", "a".repeat(10), " ".repeat(10));
    assert_eq!(split_message(&text, 15), vec!["a".repeat(10)]);
    assert!(split_message(" \n\t", 2000).is_empty());
}

#[test]
fn split_hard_cuts_multibyte_by_char() {
    let text = "あ".repeat(4500);
    let chunks = split_message(&text, MESSAGE_LIMIT);
    assert_eq!(
        chunks.iter().map(|c| c.chars().count()).collect::<Vec<_>>(),
        vec![2000, 2000, 500]
    );
    assert_eq!(chunks.concat(), text);
}
