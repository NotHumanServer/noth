use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use super::jwt::{now_secs, rfc3339_utc};
use super::oauth::{form_body, tokens_from_response};
use super::store::FileLock;
use super::*;

fn b64url_encode(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |a, (i, b)| a | u32::from(*b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

pub fn jwt(claims: &Value) -> String {
    format!(
        "{}.{}.sig",
        b64url_encode(b"{\"alg\":\"none\"}"),
        b64url_encode(claims.to_string().as_bytes())
    )
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "noth-codex-{tag}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn base64url_roundtrip_and_padding() {
    assert_eq!(base64url_decode("aGVsbG8").unwrap(), b"hello");
    assert_eq!(base64url_decode("aGVsbG8=").unwrap(), b"hello");
    assert_eq!(base64url_decode("_-8").unwrap(), vec![0xff, 0xef]);
    assert!(base64url_decode("a b").is_none());
    let data: Vec<u8> = (0..=255).collect();
    assert_eq!(base64url_decode(&b64url_encode(&data)).unwrap(), data);
}

#[test]
fn claims_and_headers() {
    let token = jwt(&json!({
        "exp": 2000000000u64,
        AUTH_CLAIM: {"chatgpt_account_id": "acct_12345", "chatgpt_data_residency": " us "},
    }));
    assert_eq!(jwt_exp(&token), Some(2000000000));
    assert_eq!(account_id(&token).as_deref(), Some("acct_12345"));
    let h = request_headers(&token);
    let get = |k: &str| h.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str());
    assert_eq!(get("ChatGPT-Account-ID"), Some("acct_12345"));
    assert_eq!(get("x-openai-internal-codex-residency"), Some("us"));
    assert_eq!(get("originator"), Some("noth"));
    assert!(get("User-Agent").unwrap().starts_with("Noth/"));
}

#[test]
fn malformed_token_drops_account_headers() {
    let h = request_headers("not-a-jwt");
    assert!(
        h.iter()
            .all(|(n, _)| *n == "originator" || *n == "User-Agent")
    );
    assert_eq!(jwt_exp("a.b.c"), None);
}

#[test]
fn refresh_decision() {
    assert!(needs_refresh(Some(1000), 1000));
    assert!(needs_refresh(Some(1119), 1000));
    assert!(!needs_refresh(Some(1121), 1000));
    assert!(!needs_refresh(None, 1000));
}

#[test]
fn masking_and_time_format() {
    assert_eq!(mask_account_id("acct_12345"), "acct****");
    assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
    assert_eq!(rfc3339_utc(1_700_000_000), "2023-11-14T22:13:20Z");
}

#[test]
fn form_encoding() {
    assert_eq!(
        form_body(&[("a", "x y&z"), ("redirect", "https://h/p")]),
        "a=x%20y%26z&redirect=https%3A%2F%2Fh%2Fp"
    );
}

#[test]
fn tokens_keep_old_refresh_unless_rotated() {
    let t = tokens_from_response(&json!({"access_token": "a"}), "old").unwrap();
    assert_eq!(t.refresh_token, "old");
    let t =
        tokens_from_response(&json!({"access_token": "a", "refresh_token": "new"}), "old").unwrap();
    assert_eq!(t.refresh_token, "new");
    assert!(tokens_from_response(&json!({}), "old").is_err());
    // ログイン直後は引き継ぐ refresh_token が無いので、応答に無ければエラー
    assert!(tokens_from_response(&json!({"access_token": "a"}), "").is_err());
    assert!(tokens_from_response(&json!({"access_token": "a", "refresh_token": " "}), "").is_err());
}

#[test]
fn store_is_atomic_and_private() {
    let dir = temp_dir("store");
    let path = dir.join("data").join("codex-auth.json");
    let t = StoredTokens {
        access_token: "a".into(),
        refresh_token: "r".into(),
        last_refresh: "2026-01-01T00:00:00Z".into(),
    };
    save(&path, &t).unwrap();
    let t2 = StoredTokens {
        access_token: "a2".into(),
        ..t.clone()
    };
    save(&path, &t2).unwrap();
    assert_eq!(load(&path).unwrap(), t2);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap()).unwrap().collect();
    assert_eq!(leftovers.len(), 1);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn missing_store_mentions_login_command() {
    let err = load(Path::new("/nonexistent/noth/codex-auth.json")).unwrap_err();
    assert!(err.to_string().contains("noth auth codex"), "{err}");
}

#[tokio::test]
async fn file_lock_is_exclusive_and_released() {
    let dir = temp_dir("lock");
    let target = dir.join("codex-auth.json");
    let lock = FileLock::acquire(&target).await.unwrap();
    let lock_path = dir.join("codex-auth.json.lock");
    assert!(lock_path.exists());
    assert!(
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
            .is_err()
    );
    drop(lock);
    assert!(!lock_path.exists());
    drop(FileLock::acquire(&target).await.unwrap());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn pending_tokens_survive_save_failure() {
    // 保存先を親がファイルのパスにして save を失敗させ、メモリ上のトークンが優先されることを確かめる
    let dir = temp_dir("pending");
    let blocker = dir.join("blocker");
    std::fs::write(&blocker, "x").unwrap();
    let path = blocker.join("codex-auth.json");
    let fresh_token = jwt(&json!({"exp": now_secs() + 3600}));
    let fresh = StoredTokens {
        access_token: fresh_token.clone(),
        refresh_token: "new".into(),
        last_refresh: String::new(),
    };
    assert!(save(&path, &fresh).is_err());
    let store = TokenStore::new(path, reqwest::Client::new());
    *store.state.lock().await = Some(fresh);
    assert_eq!(store.access_token().await.unwrap(), fresh_token);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn float_exp_is_accepted() {
    assert_eq!(
        jwt_exp(&jwt(&json!({"exp": 1700000000.0}))),
        Some(1700000000)
    );
    assert_eq!(jwt_exp(&jwt(&json!({"exp": 1700000000}))), Some(1700000000));
    assert_eq!(jwt_exp(&jwt(&json!({"exp": -1.5}))), None);
}

#[test]
fn debug_masks_tokens() {
    let t = StoredTokens {
        access_token: "secret-a".into(),
        refresh_token: "secret-r".into(),
        last_refresh: "2026-01-01T00:00:00Z".into(),
    };
    let dbg = format!("{t:?}");
    assert!(!dbg.contains("secret"), "{dbg}");
    assert!(dbg.contains("2026-01-01"));
}

#[cfg(unix)]
#[test]
fn store_dir_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_dir("dirmode");
    let path = dir.join("data").join("codex-auth.json");
    let t = StoredTokens {
        access_token: "a".into(),
        refresh_token: "r".into(),
        last_refresh: String::new(),
    };
    save(&path, &t).unwrap();
    let mode = std::fs::metadata(path.parent().unwrap())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
    std::fs::remove_dir_all(&dir).unwrap();
}
