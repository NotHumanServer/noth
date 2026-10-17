-- どのモデルの判定かを後から比べられるようにする。既存行は不明のまま NULL
ALTER TABLE decisions ADD COLUMN model TEXT;
