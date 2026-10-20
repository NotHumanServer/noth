CREATE TABLE messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id INTEGER NOT NULL,
    channel_id INTEGER NOT NULL,
    channel_name TEXT NOT NULL,
    message_id INTEGER NOT NULL,
    user_id INTEGER NOT NULL,
    author_name TEXT NOT NULL,
    is_self INTEGER NOT NULL,
    mentions_self INTEGER NOT NULL DEFAULT 0,
    reply_to_self INTEGER NOT NULL DEFAULT 0,
    content TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    chunk_id INTEGER REFERENCES topic_chunks(id),
    is_noise INTEGER,
    decision_id INTEGER REFERENCES decisions(id),
    promoted_turn_id INTEGER
);
CREATE INDEX idx_messages_guild_id ON messages (guild_id, id);
CREATE INDEX idx_messages_chunk ON messages (chunk_id);

CREATE TABLE topic_chunks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id INTEGER NOT NULL,
    channel_id INTEGER NOT NULL,
    started_at INTEGER NOT NULL,
    ended_at INTEGER NOT NULL,
    summary TEXT NOT NULL,
    compacted INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_chunks_channel ON topic_chunks (guild_id, channel_id, id);
CREATE INDEX idx_chunks_recent ON topic_chunks (guild_id, ended_at DESC);

CREATE TABLE decisions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id INTEGER NOT NULL,
    kind TEXT NOT NULL,
    answers TEXT NOT NULL,
    latency_ms INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX idx_decisions_guild ON decisions (guild_id, id);

CREATE TABLE active_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id INTEGER NOT NULL,
    role TEXT NOT NULL,
    kind TEXT NOT NULL,
    text TEXT NOT NULL,
    chunk_id INTEGER,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    cached_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER,
    removed INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);
CREATE INDEX idx_active_log_guild ON active_log (guild_id, removed, id);

CREATE TABLE digests (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id INTEGER NOT NULL,
    text TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX idx_digests_guild ON digests (guild_id, id);

CREATE TABLE worker_runs (
    run_id TEXT PRIMARY KEY,
    guild_id INTEGER NOT NULL,
    goal TEXT NOT NULL,
    task_category TEXT NOT NULL,
    trigger_message_id INTEGER,
    channel_id INTEGER NOT NULL,
    status TEXT NOT NULL,
    started_at INTEGER,
    finished_at INTEGER,
    conclusion TEXT,
    delivery_style TEXT
);
CREATE INDEX idx_worker_runs_status ON worker_runs (guild_id, status);

CREATE TABLE heartbeat (
    guild_id INTEGER PRIMARY KEY,
    updated_at INTEGER NOT NULL,
    state TEXT NOT NULL,
    info TEXT NOT NULL
);
