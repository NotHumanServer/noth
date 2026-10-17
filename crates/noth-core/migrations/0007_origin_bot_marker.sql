-- task_category は判定の質問をなくしたので、Worker を起こした経緯 origin(request / supplement / correction)に置き換える
ALTER TABLE worker_runs RENAME COLUMN task_category TO origin;
UPDATE worker_runs SET origin = 'request';

-- Noth 以外の Bot の発言は本文を持たず、目印だけを残す。content は空、リプライ先の名前は reply_to_author に入れる
ALTER TABLE messages ADD COLUMN is_bot_marker INTEGER NOT NULL DEFAULT 0;
ALTER TABLE messages ADD COLUMN bot_marker_chars INTEGER;
