-- 黙って起動した補足・訂正の根拠を後から辿れるよう、Worker が呼んだ tool の要約(JSON 配列)を残す。旧行は NULL
ALTER TABLE worker_runs ADD COLUMN tool_log TEXT;
