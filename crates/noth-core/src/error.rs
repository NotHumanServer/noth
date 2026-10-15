#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("設定: {0}")]
    Config(String),
    /// HTTP API が非 2xx を返した。provider の再試行判定に status を使う
    #[error("api {status}: {body}")]
    Api { status: u16, body: String },
    #[error("通信: {0}")]
    Transport(String),
    #[error("SQLite: {0}")]
    Store(#[from] sqlx::Error),
    #[error("マイグレーション: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// 利用者向けのエラー文を分けるための大まかな原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// 通信断・タイムアウト・止まったストリーム・429 / 5xx。待てば直りうる
    Unavailable,
    Auth,
    Quota,
    Other,
}

impl Error {
    pub fn kind(&self) -> ErrorKind {
        match self {
            Error::Api { status, body } => {
                let body = body.to_lowercase();
                match status {
                    401 | 403 => ErrorKind::Auth,
                    // 残高・上限切れは 429 で返るが、待っても直らない
                    429 if ["quota", "usage_limit", "billing", "credit"]
                        .iter()
                        .any(|w| body.contains(w)) =>
                    {
                        ErrorKind::Quota
                    }
                    408 | 429 | 500..=599 => ErrorKind::Unavailable,
                    _ => ErrorKind::Other,
                }
            }
            Error::Transport(_) => ErrorKind::Unavailable,
            // 再ログインが要るトークン更新の失敗は Other で返ってくる
            Error::Other(m) if m.contains("ログイン") || m.contains("認証") => {
                ErrorKind::Auth
            }
            // 起動後に認証ファイルが読めなくなったときなど
            Error::Config(m) if m.contains("再ログイン") => ErrorKind::Auth,
            _ => ErrorKind::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(status: u16, body: &str) -> Error {
        Error::Api {
            status,
            body: body.into(),
        }
    }

    #[test]
    fn kind_by_cause() {
        assert_eq!(Error::Transport("x".into()).kind(), ErrorKind::Unavailable);
        assert_eq!(api(503, "").kind(), ErrorKind::Unavailable);
        assert_eq!(api(429, "rate").kind(), ErrorKind::Unavailable);
        assert_eq!(api(401, "").kind(), ErrorKind::Auth);
        assert_eq!(api(403, "").kind(), ErrorKind::Auth);
        assert_eq!(
            api(429, "limit(コード: usage_limit_reached)").kind(),
            ErrorKind::Quota
        );
        assert_eq!(api(400, "bad").kind(), ErrorKind::Other);
        assert_eq!(
            Error::Other("Codex のログインに失敗しました".into()).kind(),
            ErrorKind::Auth
        );
        assert_eq!(
            Error::Config(
                "認証ファイルが壊れています。`noth auth codex` を実行して再ログインしてください"
                    .into()
            )
            .kind(),
            ErrorKind::Auth
        );
        assert_eq!(Error::Config("不正な値".into()).kind(), ErrorKind::Other);
        assert_eq!(Error::Other("boom".into()).kind(), ErrorKind::Other);
    }
}
