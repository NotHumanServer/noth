-- Discord のリプライ先。リプライでない発言と旧行は NULL
ALTER TABLE messages ADD COLUMN reply_to_message_id INTEGER;
ALTER TABLE messages ADD COLUMN reply_to_author TEXT;
ALTER TABLE messages ADD COLUMN reply_to_is_self INTEGER;
ALTER TABLE messages ADD COLUMN reply_to_excerpt TEXT;
