-- `/noth ignore add` で除外したチャンネル(またはカテゴリ)。config の ignored_channels は保存せず、起動のたびに config から読む
CREATE TABLE ignored_channels (
    guild_id INTEGER NOT NULL,
    channel_id INTEGER NOT NULL,
    added_by INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (guild_id, channel_id)
);
