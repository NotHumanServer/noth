-- MEMORY.md を書き換える前の版。reason は 'memo' か 'dreaming'
CREATE TABLE memory_versions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id INTEGER NOT NULL,
    reason TEXT NOT NULL,
    content TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX idx_memory_versions_guild ON memory_versions (guild_id, id);

-- Dreaming の実行記録。day は対象日(YYYY-MM-DD)で、同じ日を 2 回処理しないための鍵も兼ねる
CREATE TABLE dreams (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id INTEGER NOT NULL,
    day TEXT NOT NULL,
    status TEXT NOT NULL,
    report_path TEXT,
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    error TEXT,
    UNIQUE (guild_id, day)
);
