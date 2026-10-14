//! `config.toml` と `.env` を、コメントや並びを保ったまま書き換える
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result, bail};
use toml_edit::{Array, DocumentMut, Item, Table, Value};

/// 大文字スネークケースのキーは `.env` に書く(cli.md §2 `noth config`)
pub fn is_env_key(key: &str) -> bool {
    key.starts_with(|c: char| c.is_ascii_uppercase())
        && key
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn string_value(s: &str) -> Value {
    let mut v = Value::from(s);
    v.decor_mut().clear();
    v
}

/// 数値は 10 進のみ。`0x10` が 16 になる、日時や `inf` / `nan` になる、といった意図しない型を避ける
fn scalar_is_plain(v: &Value) -> bool {
    match v {
        Value::Boolean(_) | Value::String(_) => true,
        Value::Integer(_) => {
            let repr = v.to_string();
            let digits = repr.trim().trim_start_matches(['+', '-']);
            !(digits.starts_with("0x") || digits.starts_with("0o") || digits.starts_with("0b"))
        }
        Value::Float(f) => f.value().is_finite(),
        Value::Datetime(_) | Value::Array(_) | Value::InlineTable(_) => false,
    }
}

fn array_is_plain(a: &Array) -> bool {
    a.iter().all(|v| match v {
        Value::Array(inner) => array_is_plain(inner),
        other => scalar_is_plain(other),
    })
}

/// bool / 10 進の整数 / 有限の浮動小数 / 引用符付き文字列 / 配列だけを型付きで読み、残りは文字列にする。
/// `[` で始まる値は配列の意図なので、読めなければエラーにする
fn plain_value(raw: &str) -> Result<Value> {
    let raw = raw.trim();
    if raw.starts_with('[') {
        let mut v = raw
            .parse::<Value>()
            .map_err(|e| anyhow::anyhow!("配列として読めません: {e}"))?;
        match &v {
            Value::Array(a) if array_is_plain(a) => {}
            Value::Array(_) => bail!(
                "配列の要素に使えるのは bool・10 進の数値・文字列だけです(日時・16 進などは `\"…\"` で囲んでください)"
            ),
            _ => bail!("配列として読めません"),
        }
        v.decor_mut().clear();
        return Ok(v);
    }
    Ok(match raw.parse::<Value>() {
        Ok(mut v) if scalar_is_plain(&v) => {
            v.decor_mut().clear();
            v
        }
        _ => string_value(raw),
    })
}

fn unquote(s: &str) -> &str {
    let s = s.trim();
    for q in ['"', '\''] {
        if let Some(inner) = s.strip_prefix(q).and_then(|r| r.strip_suffix(q)) {
            return inner;
        }
    }
    s
}

/// Discord の ID は 18 桁前後で整数にすると桁や型を壊すので、`[..]` でも空白区切りでも文字列の配列にする
fn id_list(raw: &str) -> Result<Value> {
    let raw = raw.trim();
    let inner = match raw.strip_prefix('[') {
        Some(rest) => rest
            .strip_suffix(']')
            .context("配列として読めません(`]` がありません)")?,
        None => raw,
    };
    let mut arr = Array::new();
    for item in inner
        .split(|c: char| c == ',' || c.is_whitespace())
        .map(unquote)
        .filter(|s| !s.is_empty())
    {
        arr.push(item);
    }
    Ok(Value::Array(arr))
}

/// `key` に応じた型の値にする。`force_string` ならそのまま文字列にする
pub fn typed_value(key: &str, raw: &str, force_string: bool) -> Result<Value> {
    if force_string {
        return Ok(string_value(raw));
    }
    let last = key.rsplit('.').next().unwrap_or(key);
    if last == "allowed_users" {
        return id_list(raw);
    }
    let value = plain_value(raw)?;
    if last == "guild_id" || last.ends_with("_id") {
        return Ok(if value.is_str() {
            value
        } else {
            string_value(unquote(raw))
        });
    }
    Ok(value)
}

pub fn parse_doc(path: &Path) -> Result<DocumentMut> {
    let text =
        fs::read_to_string(path).with_context(|| format!("{} を読めません", path.display()))?;
    text.parse()
        .with_context(|| format!("{} の構文エラー", path.display()))
}

fn split_key(key: &str) -> Result<Vec<&str>> {
    let segs: Vec<&str> = key.split('.').collect();
    if segs.iter().any(|s| s.is_empty()) {
        bail!("キーが不正です: `{key}`(`a.b.c` の形式で指定してください)");
    }
    Ok(segs)
}

/// 親のテーブルが無ければ `[a.b]` 形式で作る。値が変わっても行末のコメントは残す
pub fn set_key(doc: &mut DocumentMut, key: &str, mut value: Value) -> Result<()> {
    let segs = split_key(key)?;
    let (last, parents) = segs.split_last().expect("split_key は空を返さない");
    let mut table: &mut dyn toml_edit::TableLike = doc.as_table_mut();
    for seg in parents {
        let item = table.entry(seg).or_insert_with(|| {
            let mut t = Table::new();
            // 子だけのテーブルに `[a]` の見出しを出さない
            t.set_implicit(true);
            Item::Table(t)
        });
        table = item
            .as_table_like_mut()
            .with_context(|| format!("`{seg}` はテーブルではないため、`{key}` を作れません"))?;
    }
    match table.get(last) {
        Some(Item::Value(old)) => *value.decor_mut() = old.decor().clone(),
        Some(_) => bail!("`{key}` はテーブルのため、値を設定できません"),
        None => {}
    }
    table.insert(last, Item::Value(value));
    Ok(())
}

/// 消したら true、元から無ければ false
pub fn unset_key(doc: &mut DocumentMut, key: &str) -> Result<bool> {
    let segs = split_key(key)?;
    let (last, parents) = segs.split_last().expect("split_key は空を返さない");
    let mut table: &mut dyn toml_edit::TableLike = doc.as_table_mut();
    for seg in parents {
        let Some(item) = table.get_mut(seg) else {
            return Ok(false);
        };
        let Some(next) = item.as_table_like_mut() else {
            return Ok(false);
        };
        table = next;
    }
    if table.get(last).is_some_and(Item::is_table_like) {
        bail!("`{key}` はテーブルのため削除できません(配下のキーを個別に削除してください)");
    }
    Ok(table.remove(last).is_some())
}

/// `api_key` / `token` / `*_secret` などに、`${VAR}` ではなく値そのものを書こうとしているか
pub fn is_literal_secret(key: &str, value: &Value) -> bool {
    let last = key.rsplit('.').next().unwrap_or(key);
    let secret_like = matches!(last, "api_key" | "token" | "password")
        || last.ends_with("_secret")
        || last.ends_with("_token")
        || last.ends_with("_api_key");
    let Some(s) = value.as_str() else {
        return false;
    };
    let is_reference = s
        .strip_prefix("${")
        .and_then(|r| r.strip_suffix('}'))
        .is_some_and(is_env_key);
    secret_like && !s.is_empty() && !is_reference
}

fn env_line_key(line: &str) -> Option<&str> {
    let line = line.trim();
    if line.starts_with('#') {
        return None;
    }
    let line = line.strip_prefix("export ").unwrap_or(line);
    line.split_once('=').map(|(k, _)| k.trim())
}

/// core の `.env` パーサはエスケープを解釈しないので、囲む引用符で空白や `#` を守る。
/// `"` と `'` の両方を含む値はどちらでも囲めず、書けない
pub fn env_value(value: &str) -> Result<String> {
    if value.contains('\n') || value.contains('\r') {
        bail!("改行を含む値は .env に書けません");
    }
    if !value
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '#' | '"' | '\''))
    {
        return Ok(value.to_string());
    }
    if !value.contains('"') {
        Ok(format!("\"{value}\""))
    } else if !value.contains('\'') {
        Ok(format!("'{value}'"))
    } else {
        bail!("`\"` と `'` の両方を含む値は .env に書けません")
    }
}

pub fn env_set(text: &str, key: &str, value: &str) -> Result<String> {
    let new_line = format!("{key}={}", env_value(value)?);
    let mut out = String::new();
    let mut replaced = false;
    for line in text.split_inclusive('\n') {
        if env_line_key(line) == Some(key) {
            if !replaced {
                out.push_str(&new_line);
                out.push('\n');
                replaced = true;
            }
        } else {
            out.push_str(line);
        }
    }
    if !replaced {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&new_line);
        out.push('\n');
    }
    Ok(out)
}

/// 消したら true、元から無ければ false
pub fn env_unset(text: &str, key: &str) -> (String, bool) {
    let mut found = false;
    let out = text
        .split_inclusive('\n')
        .filter(|line| {
            let hit = env_line_key(line) == Some(key);
            found |= hit;
            !hit
        })
        .collect();
    (out, found)
}

/// 同じディレクトリの一時ファイルに書いてから rename する。途中で落ちても元のファイルが壊れない。
/// `mode` が None なら既存ファイルの権限を引き継ぎ、無ければ 0644 にする(シークレットは Some(0o600))
pub fn write_atomic(path: &Path, text: &str, mode: Option<u32>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = mode
        .unwrap_or_else(|| fs::metadata(path).map_or(0o644, |m| m.permissions().mode() & 0o777));
    let name = path
        .file_name()
        .context("ファイル名がありません")?
        .to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let result = (|| -> Result<()> {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)
            .with_context(|| format!("{} を作れません", tmp.display()))?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, path).with_context(|| format!("{} を更新できません", path.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

pub fn enabled_list(doc: &DocumentMut) -> Vec<String> {
    doc.get("modules")
        .and_then(|m| m.get("enabled"))
        .and_then(Item::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn enabled_array(doc: &mut DocumentMut) -> Result<&mut Array> {
    doc.get_mut("modules")
        .and_then(|m| m.get_mut("enabled"))
        .and_then(Item::as_array_mut)
        .context("config.toml に [modules] enabled がありません")
}

/// `enabled` 配列の 1 要素。toml_edit は要素間の空白とコメントを前後の要素の decor に持つので、
/// 付け外しのたびに壊れないよう、いったん要素単位のコメントに分解して組み直す
struct ModuleEntry {
    /// 要素の直前にあるコメント行(`# ...`)。その要素に付いた注意書きとして一緒に動かす
    lead: Vec<String>,
    name: String,
    /// カンマの後ろの行末コメント。桁揃えの空白ごと保つ
    trail: Option<String>,
}

fn decor_text(s: Option<&toml_edit::RawString>) -> String {
    s.and_then(|r| r.as_str()).unwrap_or("").to_string()
}

fn comment_lines(text: &str) -> impl Iterator<Item = String> + '_ {
    text.lines()
        .map(str::trim)
        .filter(|l| l.starts_with('#'))
        .map(String::from)
}

/// 1 行に収まる配列(`["a", "b"]`)は None。コメントを持てないので toml_edit の整形に任せる
fn split_entries(arr: &Array) -> Option<(Vec<ModuleEntry>, Vec<String>)> {
    let multiline = arr
        .iter()
        .any(|v| decor_text(v.decor().prefix()).contains('\n'))
        || arr.trailing().as_str().is_some_and(|t| t.contains('\n'));
    if !multiline {
        return None;
    }
    let mut entries: Vec<ModuleEntry> = Vec::new();
    for v in arr.iter() {
        let prefix = decor_text(v.decor().prefix());
        // 先頭行は直前の要素の行末コメント、以降が自分の直前コメント
        let (first, rest) = match prefix.split_once('\n') {
            Some((f, r)) => (f.to_string(), r.to_string()),
            None => (prefix.clone(), String::new()),
        };
        match entries.last_mut() {
            Some(prev) if first.contains('#') => prev.trail = Some(first.trim_end().to_string()),
            None if first.contains('#') => {
                return None;
            }
            _ => {}
        }
        entries.push(ModuleEntry {
            lead: comment_lines(&rest).collect(),
            name: v.as_str()?.to_string(),
            trail: None,
        });
    }
    let tail = arr.trailing().as_str().unwrap_or("").to_string();
    let (first, rest) = tail.split_once('\n').unwrap_or((&tail, ""));
    if first.contains('#')
        && let Some(last) = entries.last_mut()
    {
        last.trail = Some(first.trim_end().to_string());
    }
    Some((entries, comment_lines(rest).collect()))
}

fn write_entries(arr: &mut Array, entries: &[ModuleEntry], dangling: &[String]) {
    let mut new = Array::new();
    let mut prev_trail = "";
    for e in entries {
        let mut v = Value::from(e.name.as_str());
        let leads: String = e.lead.iter().map(|l| format!("  {l}\n")).collect();
        v.decor_mut().set_prefix(format!("{prev_trail}\n{leads}  "));
        new.push_formatted(v);
        prev_trail = e.trail.as_deref().unwrap_or("");
    }
    let dangling: String = dangling.iter().map(|l| format!("  {l}\n")).collect();
    new.set_trailing(format!("{prev_trail}\n{dangling}"));
    new.set_trailing_comma(true);
    *new.decor_mut() = arr.decor().clone();
    *arr = new;
}

/// 追加したら true、既に有効なら false
pub fn enable_module(doc: &mut DocumentMut, name: &str) -> Result<bool> {
    if enabled_list(doc).iter().any(|m| m == name) {
        return Ok(false);
    }
    let arr = enabled_array(doc)?;
    match split_entries(arr) {
        Some((mut entries, dangling)) => {
            // 末尾の宙に浮いたコメントは、直前に外した要素の注意書きなので新しい末尾要素に戻す
            entries.push(ModuleEntry {
                lead: dangling,
                name: name.to_string(),
                trail: None,
            });
            write_entries(arr, &entries, &[]);
        }
        None => arr.push(name),
    }
    Ok(true)
}

/// 並びとコメントを保ったまま `old` を `new` に置き換える。`old` が無ければ false
pub fn replace_module(doc: &mut DocumentMut, old: &str, new: &str) -> Result<bool> {
    let arr = enabled_array(doc)?;
    let Some(idx) = arr.iter().position(|v| v.as_str() == Some(old)) else {
        return Ok(false);
    };
    arr.replace(idx, new);
    Ok(true)
}

/// 外したら true、元から無効なら false
pub fn disable_module(doc: &mut DocumentMut, name: &str) -> Result<bool> {
    let arr = enabled_array(doc)?;
    let Some(idx) = arr.iter().position(|v| v.as_str() == Some(name)) else {
        return Ok(false);
    };
    match split_entries(arr) {
        Some((mut entries, mut dangling)) => {
            let removed = entries.remove(idx);
            // 外した要素の直前コメント(注意書き)は捨てず、次の要素の前、無ければ末尾に残す。行末コメントは要素と一緒に消える
            match entries.get_mut(idx) {
                Some(next) => next.lead.splice(0..0, removed.lead).for_each(drop),
                None => dangling.splice(0..0, removed.lead).for_each(drop),
            }
            write_entries(arr, &entries, &dangling);
        }
        None => {
            arr.remove(idx);
            arr.fmt();
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
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
}
