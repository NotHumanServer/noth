-- provider と有料 tool の呼び出しごとの費用。amount_usd が NULL なのは単価が分からないもの(トークン数や数量だけは集計できる)
CREATE TABLE costs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    source TEXT NOT NULL,
    model TEXT NOT NULL,
    purpose TEXT NOT NULL,
    run_id TEXT,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    unit TEXT,
    quantity INTEGER,
    amount_usd REAL,
    billing TEXT NOT NULL
);
CREATE INDEX idx_costs_guild_time ON costs (guild_id, created_at);

-- 外部 API が返したレート制限の残り。区分ごとに最新の値だけを持つ
CREATE TABLE rate_limits (
    guild_id INTEGER NOT NULL,
    source TEXT NOT NULL,
    bucket TEXT NOT NULL,
    "limit" INTEGER NOT NULL,
    remaining INTEGER NOT NULL,
    reset_at INTEGER,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (guild_id, source, bucket)
);
