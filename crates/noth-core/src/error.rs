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
