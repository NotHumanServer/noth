-- 補足の最短間隔と 1 日の上限を、再起動しても数え直せるよう run の作成時刻から数える。旧行は 0(数えない)
ALTER TABLE worker_runs ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0;
