//! `config.toml` と `.env` を、コメントや並びを保ったまま書き換える
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result, bail};
use toml_edit::{DocumentMut, Item, Table, Value};

mod env;
mod modules;
#[cfg(test)]
mod tests;
mod value;

pub use env::{env_set, env_unset, env_value, is_env_key};
pub use modules::{disable_module, enable_module, enabled_list, replace_module};
pub use value::{is_literal_secret, typed_value};

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
