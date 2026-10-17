-- Join の合流判定で「Fork 以降に届いた発言」を取るための境界(Fork 時点の messages.id の最大値)
ALTER TABLE worker_runs ADD COLUMN fork_message_id INTEGER NOT NULL DEFAULT 0;
-- 結論メモのターンをトリガーと同じ TopicChunk に入れ、緊急コンパクションでチャンク単位に扱えるようにする
ALTER TABLE worker_runs ADD COLUMN chunk_id INTEGER;
