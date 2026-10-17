-- `/noth ignore add` で除外したチャンネル(またはカテゴリ)。config の ignored_channels は保存せず、起動のたびに config から読む
CREATE TABLE ignored_channels (
    guild_id INTEGER NOT NULL,
    channel_id INTEGER NOT NULL,
    added_by INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (guild_id, channel_id)
);

-- 除外したチャンネルに残っていた未判定・未昇格の発言を、判定にも Layer 3 にも回さないための印。Layer 3 に入った行は触らない
ALTER TABLE messages ADD COLUMN hidden INTEGER NOT NULL DEFAULT 0;
