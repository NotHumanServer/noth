use super::*;

const SAMPLE: &str = r##"[modules]
enabled = [
  "channel-discord",
  "router-systemone",          # router はちょうど1つ
  "provider-anthropic",        # provider
  # "tool-web-agent",
  "tool-shell",
]

# 説明
[channel.discord]
debounce_secs = 2.0   # 秒
"##;

fn doc() -> DocumentMut {
    SAMPLE.parse().unwrap()
}

#[test]
fn env_key_detection() {
    assert!(is_env_key("ANTHROPIC_API_KEY"));
    assert!(is_env_key("A1"));
    assert!(!is_env_key("channel.discord.token"));
    assert!(!is_env_key("Anthropic"));
    assert!(!is_env_key("1A"));
    assert!(!is_env_key(""));
}

fn typed(key: &str, raw: &str) -> Value {
    typed_value(key, raw, false).unwrap()
}

#[test]
fn typed_value_reads_plain_toml_only() {
    assert_eq!(typed("a.b", "3").as_integer(), Some(3));
    assert_eq!(typed("a.b", "2.5").as_float(), Some(2.5));
    assert_eq!(typed("a.b", "true").as_bool(), Some(true));
    assert_eq!(typed("a.b", "[\"a\", \"b\"]").as_array().unwrap().len(), 2);
    assert_eq!(typed("a.b", "\"x y\"").as_str(), Some("x y"));
    assert_eq!(
        typed("a.b", "claude-opus-5-5").as_str(),
        Some("claude-opus-5-5")
    );
    assert_eq!(typed("a.b", "${FOO}").as_str(), Some("${FOO}"));
    for raw in ["2026-10-15", "0x10", "0o7", "inf", "nan", "-inf", "{a = 1}"] {
        assert_eq!(typed("a.b", raw).as_str(), Some(raw), "{raw}");
    }
}

#[test]
fn broken_array_is_error() {
    assert!(typed_value("a.b", "[1, 2", false).is_err());
    assert!(typed_value("a.b", "[0x10]", false).is_err());
    assert!(typed_value("a.b", "[2026-10-15]", false).is_err());
}

#[test]
fn force_string_keeps_raw() {
    assert_eq!(
        typed_value("a.b", "123", true).unwrap().as_str(),
        Some("123")
    );
    assert_eq!(
        typed_value("a.b", "[1]", true).unwrap().as_str(),
        Some("[1]")
    );
}

#[test]
fn id_keys_are_strings() {
    let v = typed("channel.discord.guild_id", "123456789012345678");
    assert_eq!(v.as_str(), Some("123456789012345678"));
    assert_eq!(typed("x.channel_id", "\"42\"").as_str(), Some("42"));
    assert_eq!(
        typed("channel.discord.master", "123456789012345678").as_str(),
        Some("123456789012345678")
    );
    let users = typed("channel.discord.allowed_users", "111111111111111111");
    assert_eq!(users.to_string(), "[\"111111111111111111\"]");
    let users = typed("channel.discord.allowed_users", "[111, \"222\"]");
    let items: Vec<_> = users
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(items, ["111", "222"]);
    assert!(typed_value("channel.discord.allowed_users", "[111", false).is_err());
}

#[test]
fn write_atomic_sets_mode_and_replaces() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("noth-edit-test-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let f = dir.join(".env");
    fs::write(&f, "A=1\n").unwrap();
    fs::set_permissions(&f, fs::Permissions::from_mode(0o644)).unwrap();
    write_atomic(&f, "A=2\n", Some(0o600)).unwrap();
    assert_eq!(fs::read_to_string(&f).unwrap(), "A=2\n");
    assert_eq!(
        fs::metadata(&f).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // None は既存の権限を引き継ぐ
    write_atomic(&f, "A=3\n", None).unwrap();
    assert_eq!(
        fs::metadata(&f).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn set_keeps_comments_and_trailing_comment() {
    let mut d = doc();
    set_key(&mut d, "channel.discord.debounce_secs", typed("a.b", "3.5")).unwrap();
    let out = d.to_string();
    assert!(out.contains("debounce_secs = 3.5   # 秒"), "{out}");
    assert!(out.contains("# 説明"));
    assert!(out.contains("# router はちょうど1つ"));
}

#[test]
fn set_creates_nested_tables_in_header_form() {
    let mut d = doc();
    set_key(
        &mut d,
        "tool.web-agent.poll_interval_secs",
        typed("a.b", "5"),
    )
    .unwrap();
    set_key(&mut d, "worker.timeout_secs", typed("a.b", "900")).unwrap();
    let out = d.to_string();
    assert!(
        out.contains("[tool.web-agent]\npoll_interval_secs = 5"),
        "{out}"
    );
    assert!(out.contains("[worker]\ntimeout_secs = 900"), "{out}");
    assert!(!out.contains("[tool]"), "{out}");
    let back: toml::Table = out.parse().unwrap();
    assert_eq!(back["worker"]["timeout_secs"].as_integer(), Some(900));
}

#[test]
fn set_rejects_non_table_parent_and_bad_key() {
    let mut d = doc();
    assert!(set_key(&mut d, "channel.discord.debounce_secs.x", typed("a.b", "1")).is_err());
    assert!(set_key(&mut d, "a..b", typed("a.b", "1")).is_err());
    assert!(set_key(&mut d, "channel.discord", typed("a.b", "1")).is_err());
}

#[test]
fn unset_removes_only_target() {
    let mut d = doc();
    assert!(unset_key(&mut d, "channel.discord.debounce_secs").unwrap());
    assert!(!unset_key(&mut d, "channel.discord.debounce_secs").unwrap());
    assert!(!unset_key(&mut d, "nope.nothing").unwrap());
    let out = d.to_string();
    assert!(!out.contains("debounce_secs"));
    assert!(out.contains("[channel.discord]"));
}

const MODULES_HEAD: &str = "[modules]\nenabled = [\n";
const TAIL: &str = "]\n\n# 説明\n[channel.discord]\ndebounce_secs = 2.0   # 秒\n";

fn modules(body: &str) -> String {
    format!("{MODULES_HEAD}{body}{TAIL}")
}

#[test]
fn enable_appends_with_plain_indent() {
    let mut d = doc();
    assert!(enable_module(&mut d, "worker").unwrap());
    assert!(!enable_module(&mut d, "worker").unwrap());
    assert_eq!(
        d.to_string(),
        modules(concat!(
            "  \"channel-discord\",\n",
            "  \"router-systemone\",          # router はちょうど1つ\n",
            "  \"provider-anthropic\",        # provider\n",
            "  # \"tool-web-agent\",\n",
            "  \"tool-shell\",\n",
            "  \"worker\",\n",
        ))
    );
}

#[test]
fn disable_last_leaves_its_leading_comments_at_the_end() {
    let mut d = doc();
    assert!(disable_module(&mut d, "tool-shell").unwrap());
    assert!(!disable_module(&mut d, "tool-shell").unwrap());
    assert_eq!(
        d.to_string(),
        modules(concat!(
            "  \"channel-discord\",\n",
            "  \"router-systemone\",          # router はちょうど1つ\n",
            "  \"provider-anthropic\",        # provider\n",
            "  # \"tool-web-agent\",\n",
        ))
    );
}

#[test]
fn disable_then_enable_last_round_trips_exactly() {
    let mut d = doc();
    assert!(disable_module(&mut d, "tool-shell").unwrap());
    assert!(enable_module(&mut d, "tool-shell").unwrap());
    assert_eq!(d.to_string(), SAMPLE);
}

#[test]
fn disable_middle_drops_only_its_own_trailing_comment() {
    let mut d = doc();
    assert!(disable_module(&mut d, "router-systemone").unwrap());
    assert_eq!(
        d.to_string(),
        modules(concat!(
            "  \"channel-discord\",\n",
            "  \"provider-anthropic\",        # provider\n",
            "  # \"tool-web-agent\",\n",
            "  \"tool-shell\",\n",
        ))
    );
}

#[test]
fn disable_moves_leading_comments_to_the_next_element() {
    let mut d = doc();
    assert!(disable_module(&mut d, "provider-anthropic").unwrap());
    assert_eq!(
        d.to_string(),
        modules(concat!(
            "  \"channel-discord\",\n",
            "  \"router-systemone\",          # router はちょうど1つ\n",
            "  # \"tool-web-agent\",\n",
            "  \"tool-shell\",\n",
        ))
    );
}

#[test]
fn single_line_array_stays_single_line() {
    let mut d: DocumentMut = "[modules]\nenabled = [\"a\", \"b\"]\n".parse().unwrap();
    assert!(enable_module(&mut d, "c").unwrap());
    assert!(disable_module(&mut d, "a").unwrap());
    assert_eq!(d.to_string(), "[modules]\nenabled = [\"b\", \"c\"]\n");
}

#[test]
fn env_set_replaces_or_appends() {
    let text = "# c\nA=\nB=old\n# C=x\n";
    assert_eq!(
        env_set(text, "B", "new").unwrap(),
        "# c\nA=\nB=new\n# C=x\n"
    );
    assert_eq!(
        env_set(text, "C", "z").unwrap(),
        "# c\nA=\nB=old\n# C=x\nC=z\n"
    );
    assert_eq!(env_set("A=1", "B", "2").unwrap(), "A=1\nB=2\n");
    assert_eq!(env_set("", "B", "a b").unwrap(), "B=\"a b\"\n");
    assert!(env_set("", "B", "a\nb").is_err());
}

#[test]
fn unset_rejects_whole_table() {
    let mut d = doc();
    assert!(unset_key(&mut d, "channel.discord").is_err());
    assert!(unset_key(&mut d, "channel").is_err());
    assert!(d.to_string().contains("[channel.discord]"));
}

#[test]
fn literal_secret_detection() {
    let lit = Value::from("sk-abc");
    let var = Value::from("${ANTHROPIC_API_KEY}");
    assert!(is_literal_secret("provider.anthropic.api_key", &lit));
    assert!(is_literal_secret("channel.discord.token", &lit));
    assert!(is_literal_secret("x.client_secret", &lit));
    assert!(!is_literal_secret("provider.anthropic.api_key", &var));
    assert!(!is_literal_secret("provider.anthropic.model", &lit));
    assert!(!is_literal_secret(
        "provider.anthropic.api_key",
        &Value::from(3)
    ));
}

#[test]
fn env_value_rejects_both_quotes() {
    assert!(env_value("a\"b'c").is_err());
    assert_eq!(env_value("a'b c").unwrap(), "\"a'b c\"");
    assert_eq!(env_value("a\"b c").unwrap(), "'a\"b c'");
}

#[test]
fn env_unset_removes_line() {
    let (out, found) = env_unset("# c\nA=1\nB=2\n", "A");
    assert!(found);
    assert_eq!(out, "# c\nB=2\n");
    assert!(!env_unset("A=1\n", "Z").1);
}
