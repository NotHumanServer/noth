# Noth コンテキスト仕様: Layer 構造と System One ルーティング

本書は Main Profile Engine(本線 LLM)に渡すプロンプトの層構造、System One(router)による判定スキーマ、発言からの応答・自律タスク・圧縮に至るライフサイクルを定義します。module の分割は [architecture/modules.md](architecture/modules.md)、trait は [architecture/traits.md](architecture/traits.md)、設定項目は [cli.md](cli.md) を参照してください。文書全体の入口は [index.md](index.md) で、本書に出てくる用語の意味も[用語表](index.md#用語)にまとめています。

## 1. 目的と設計原則

Noth はサーバーの全チャンネルを常時観測し、「同じ空間にいる仲間」として文脈を共有しながら、自然な雑談への介入と自律的な調査・作業を行います。

1. **生ログの確認は System One に委ねる。** 大量の生ログを常時走査する役割は、入力コストが低く文字列を生成しない System One モデル(router)に担わせる
2. **本線 LLM にノイズと生ログを入れない。** 本線には「router がノイズと判定した発言」と「tool 実行の生ログ(検索結果など)」を一切入れない
3. **プレフィックスを不変に保ち KV キャッシュを維持する。** 本線のコンテキストは先頭から Layer 3 までを厳格な追記専用(Append-only)とし、動的な要素はすべて末尾の Layer 4 に置く
4. **本線への書き込みは直列に処理する。** router 判定、本線の生成、Layer 3 への昇格、Compactor による要約への置換はすべて1つの actor が順番に処理する。並行に実行するのは Worker だけで、その結果の合流も actor のキューに積む

## 2. システムコンポーネント

| # | コンポーネント | 担当 module | 役割 |
|---|---|---|---|
| ① | Ingest & Debouncer | `channel-discord` + Core | Bot 以外の `allowed_users` の発言を受信して SQLite に保存し、チャンネル単位でデバウンスしてから actor のキューに積む |
| ② | Gatekeeper & Attention Router(System One) | `router-*` | 文字列を生成せず、確率付きの構造化判定を1クエリで並列に返す。介入判定、ノイズ判定、話題の区切り、過去チャンクの関連度選択、Worker 合流判定 |
| ③ | Main Profile Engine(本線) | `provider-*` + Core | サーバーの仲間としての対話生成と、自律タスクの起動宣言(Ack)。tool 定義は受け取るが **実行は常に Worker** |
| ④ | Ephemeral Task Worker(作業用 LLM) | `worker` + `tool-*` | 本線から Fork した使い捨てのワーカー。tool を使って調査・作業し、完了後に結論メモだけを本線に返して終了する |
| ⑤ | Idle Compactor | `compactor` | サーバー全体がアイドルになったとき Layer 3 を三人称のエピソード記憶に圧縮して Layer 2 へ統合する |

デバウンスでは、チャンネルの最終発言から既定で 2.0 秒待ち、短文の連投は1つに結合します。間を空けずに発言が続いても、最初の発言から 10 秒経てばその時点で判定に回します。活発な雑談の最中に届いた Noth 宛ての発言を、いつまでも判定しないままにしないためです。Noth 自身の投稿も `is_self` 付きで SQLite に保存します。router が「直前に自分が答えた」ことを判断できるようにするためです。

### 2.1 本線 actor と処理単位

Context Layer Manager は本線の状態(Layer 2〜4、未昇格バッファ)を持つ唯一の writer で、mpsc キューから次の仕事を1つずつ取り出します。

| 仕事 | 内容 |
|---|---|
| `Judge(発言群)` | router 判定 → パターン A〜C の実行 → 昇格。1ユニットとして完了するまで次に進まない |
| `Join(run_id)` | Worker 完了の合流判定 → 本線の報告 → 昇格 |
| `Compact` | Layer 3 の指定範囲を Layer 2 へ置換 |

本線の生成中(数秒〜数十秒)に届いた発言はキューで待ち、生成完了後に判定します。デバウンスはチャンネル単位ですが、キューに積まれる順は到着順なので、複数チャンネルの発言が交錯しても Layer 3 への追記順は一意に定まります。

## 3. 本線プロンプトの層構造(KV キャッシュ保護規約)

本線のプロンプトは先頭から必ず次の順に構築します。

```text
[tools]                      Worker と同一の tool 定義(worker 有効時のみ)。固定
[Layer 0: Harness Rules]     Noth 固有の規約。バイナリに内蔵、ユーザーは触らない
[Layer 1: Profile]           キャラクター定義 + guild メンバー情報。起動時にロードして固定
[Layer 2: Episodic Digest]   今日〜直近数日のエピソード要約ボード。Compactor のみが更新
[Layer 3: Active Clean Log]  現在の会話ログ + Worker 結論メモ。Append-only
---------------------------------------------------------------- ↑ ここまで KV キャッシュ 100% ヒット
[Layer 4: Ephemeral Tail]    router が今ターンのみ抽出した過去の関連チャンク + 今回のトリガー発言群
```

### 3.1 各層の内容と変化のタイミング

| 層 | 内容 | 変化 | API 上の置き場所 |
|---|---|---|---|
| Layer 0 | 出力形式(Ack の書き方、Worker への Goal の構造、結論メモの読み方)、Discord 上の振る舞いの原則、やってはいけないこと、本線は tool を直接呼ばないという規約、要約を三人称で書く規約。キャラクターに依存しない | 有効な module 構成ごとに固定(worker 無効なら Ack と Goal の節を含めない) | `system` |
| Layer 1 | `~/.noth/profiles/<name>.md` の内容(口調・一人称・性格・口癖・模倣元の特徴)+ `allowed_users` のメンバー情報(表示名、ロール) | 起動時のみ | `system` |
| Layer 2 | 三人称の事実記述によるエピソード要約。「A が○○について質問し、B が△△と回答した」の形。`digest_max_chars` を超えたら古いエピソードから削除する(SQLite には残る) | Compactor 実行時のみ | `system` |
| Layer 3 | 昇格済みのクリーンな発言、本線の応答(Ack と Goal を含む)、`[内部調査完了 <run_id>: <結論メモ>]` ブロック。各ターンは所属 TopicChunk の ID を持つ | ターン終了時に末尾へ追記のみ | `messages` |
| Layer 4 | router が選んだ過去 TopicChunk の本文 + 今回のトリガー発言群 | 毎ターン差し替え | `messages` 末尾 |

Layer 0 を先頭に置くのは、Profile 側の記述で Harness Rules を上書きできないようにするためです。

### 3.2 ターン終了時の昇格ルール

本線が応答を出力した直後に次を行います。

1. Layer 4 に一時挿入した「過去の関連チャンク」を破棄する
2. `[今回のトリガー発言群]`(Layer 4 に入れたときと同じブロック構造の user メッセージ)と `[本線の応答]`(assistant メッセージ)を Layer 3 の末尾へ確定追記する

トリガー発言群のブロックは Layer 4 でも Layer 3 でも同じ内容・同じ形で、関連チャンクはその **前** に置きます。こうすると「関連チャンクを取り除く」操作では、末尾の2ブロックより前に挿入した関連チャンクだけを削除します。次ターンの Layer 0〜3 は、前ターンの Layer 0〜3 にトリガー発言群と応答の2ブロックを追記した形になります。

### 3.3 未昇格クリーンバッファ

router が `silent` と判定した発言のうち `is_noise_or_meme == false` のものは、Layer 3 にすぐ追記せず「未昇格クリーンバッファ」に溜めます。次に本線が発言するターンの冒頭で **TopicChunk ごとに1ブロックにまとめて** Layer 3 へ追記します。こうするのは、本線が起動しない間はプレフィックスを一切変えないためと、Anthropic のキャッシュ探索(lookback)が 20 ブロックまでなので追記を細かいブロックに分けないためです。

### 3.4 キャッシュの扱い

provider ごとのブレークポイントの置き方と TTL、本線と Worker のキャッシュ共有、keep-alive は [architecture/caching.md](architecture/caching.md) で定義します。

## 4. System One 判定スキーマ

Core は以下の質問セットを定義し、router module に渡します。質問型は System One API と同じ `noul`(yes/no の確率)/ `choice`(単一選択、各選択肢の確率と confidence)/ `score`(段階別の評価基準)の3種類だけです。`router-systemone` はこれをそのまま API の `questions` に変換し、`router-mention` はルールで同じ形の回答(確率は 0.0 / 1.0、confidence は 1.0)を返します。

二値化は Core が行います。`noul` は `[router.thresholds]` の閾値以上なら true、`choice` は最大確率の選択肢を採り、`confidence` が `min_confidence` 未満なら安全側の値を採ります(介入判定なら `silent`、合流判定なら `abort`)。Clef の choice の confidence は最大確率が 0.6 程度でも 0.15〜0.25 に収まるため、既定の `min_confidence` は 0.15 にしています。`is_noise_or_meme` も「ん」「んにゃ」で 0.5〜0.6 にとどまるため、既定の閾値は 0.5 です(いずれも 2026-10-15 の実測 26 件から決めた暫定値)。ただし Noth へのメンションか Noth の発言へのリプライを含む発言群は、router の判定や失敗に関係なく `reply_only` 以上として扱い、ノイズ扱いもしません。名指しされたのに無視すると、利用者には故障にしか見えないためです。本文で `[router] call_names` の呼び名(単語の一部としての一致は除く)が出た発言群は、介入判定だけ `min_confidence` を問わず、router が静観を選んでも `reply_only` と `spawn_task` の確率の合計が `name_call_reply`(既定 0.25)以上なら応答します。呼び名の後ろに続くひらがな(「くん」「さん」や助詞)は単語の切れ目とみなします。Noth がそのチャンネルで応答してから `conversation_window_secs`(既定 120 秒)の間は、そのとき話しかけてきた人の発言も同じ扱いにします。Clef は Noth の応答直後の返事を Noth 宛てと見なさないことが多いためです(2026-10-15 の実測)。この時刻と相手はメモリにだけ持ち、再起動で消えます。`state.trigger` にも `calls_noth_by_name` として渡します。必ず応答にはしません。開発や設定の話題で名前が出るたびに割り込まないためです。確率そのものも判定したモデル名と一緒に SQLite に保存し、閾値の調整や後の分析に使えるようにします。なお同じ会話で比べると、Jev は Noth 宛てかどうかを 0.6〜0.9 の確率ではっきり分け、Noth の応答直後の返事も Noth 宛てと判定できた一方、Clef と Clef-flash は確率が 0.3〜0.6 に寄って区別がつかなかったため、雛形の既定は Jev にしています(2026-10-15 の実測)。上記の補正の多くは Clef 向けで、Jev ではほとんど効きません。

### 4.1 `state` の組み立て

`state` は次の JSON オブジェクトで、全体を `state_max_chars`(既定 24,000 文字)に収めます。超える場合は `recent` の古い発言から除外します。ローカルではトークン数を計算しないため、Clef-flash(24,576 トークン)を使うときは文字数上限を余裕のある小さめの値に設定してください。

| フィールド | 内容 |
|---|---|
| `trigger` | デバウンス後の今回の発言群(チャンネル、投稿者の表示名、本文、時刻) |
| `recent` | 全チャンネルの直近 `recent_messages` 件(既定 30)。Noth 自身の発言も `is_self: true` で含める |
| `candidates` | 候補 TopicChunk の `{id, channel, summary}`。関連判定の対象 |

### 4.2 流入・介入判定(発言受信時)

| キー | 型 | 意味 |
|---|---|---|
| `is_noise_or_meme` | noul | スタンプ連打・コピペ連投など文脈価値のないノイズか |
| `is_topic_shift` | noul | そのチャンネルの直前の会話から話題が切り替わったか |
| `intervention_action` | choice: `silent` / `reply_only` / `spawn_task` | 静観するか、本線だけを起動するか、本線と Worker を起動するか |
| `task_category` | choice: `none` / `web_research` / `fact_check` / `code_or_tool` | Worker に渡す作業の種類 |
| `relevant_chunk_<id>` | noul × 候補数 | 候補 TopicChunk `<id>` が今回の発言に関連するか。候補ごとに1問 |

- TopicChunk は **チャンネルごと** に管理する。`is_topic_shift == true` ならそのチャンネルで新しい TopicChunk の ID を割り当てる。チャンネルが違えば別のチャンクなので、2チャンネルの交互発言で毎回 topic shift になることはない
- `is_noise_or_meme == true` なら SQLite には記録するが、Layer 3 への昇格対象から外す。TopicChunk の要約にも足さず、topic shift でも新しいチャンクを作らない。ただし同時に `reply_only` / `spawn_task` と判定された場合は応答し、その発言をトリガーとして Layer 3 に入れる。応答した発言が Layer 3 に無いと会話が成り立たないため
- `intervention_action == spawn_task` かつ `task_category == none` は `reply_only` として扱う
- 候補 TopicChunk は直近のものから `candidate_chunks` 個(既定 32)。上限は `Router::max_questions() - 4`(Clef なら 60)。Layer 3 のターンが属しているチャンクと、未昇格クリーンバッファの発言が属しているチャンクは候補から外す。バッファはこのターンで Layer 3 に昇格するので、候補に入れると同じ本文が Layer 3 と Layer 4 に二重に入るため。候補の要約は TopicChunk 作成時に冒頭の数件の発言から機械的に作成する(LLM は使わない)
- System One API に複数選択型は無いため、関連チャンクは候補ごとの `noul` で表現する。`choice` の選択肢上限 255 は本設計では使わない

### 4.3 Worker 合流判定(Worker 完了時)

`state` は Worker の結論メモ(または失敗理由)と、Fork 以降に届いた発言(`recent_messages` 件まで、Noth 自身の発言を含む)です。

| キー | 型 | 意味 |
|---|---|---|
| `delivery_style` | choice: `normal_report` / `brief_supplement` / `reply_anchor` / `abort` | 報告の仕方。人間側で自己解決済みなら `brief_supplement`、別の話題に移っていれば `reply_anchor`、報告の価値が無ければ `abort` |

判断材料(自己解決したか、話題が移ったか)は `instructions` の中で説明し、別の質問には分けません。

### 4.4 Jev と Clef の差異

Jev(TypeSafe)と Clef / Clef-flash(Cloudflare Workers AI)はどちらも「`state` + `questions` → 各質問の確率付き回答」という System One API の形で、質問型名 `noul` / `choice` / `score` とレスポンス構造(`answers.<key>`、`probabilities`、`confidence`)も一致します。Cloudflare は「エンドポイントとモデル名を変えるだけで切り替え可能」と説明しています。`router-systemone` は `preset` で URL の組み立てと body の `model` 値だけを切り替えます。

| | Jev | Clef | Clef-flash |
|---|---|---|---|
| エンドポイント | `POST https://api.typesafe.ai/v1/systemone` | `POST https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/run/@cf/cloudflare/clef` | 同左、末尾 `clef-flash` |
| 認証 | `Authorization: Bearer <TYPESAFE_API_KEY>` | `Authorization: Bearer <CF_API_TOKEN>` | 同左 |
| body の `model` | `jev-latest` など | `clef` | `clef-flash` |
| コンテキスト上限 | 64k tokens / リクエスト、`state` + 最長質問で 32k | 65,536 | 24,576 |
| 質問数 / リクエスト | 記載なし | 64 | 64 |
| 公称レイテンシ(中央値) | 524 ms(Cloudflare 計測) | 209 ms | 38.8 ms |

出典: [TypeSafe API reference](https://docs.typesafe.ai/api)、[TypeSafe docs: Models](https://docs.typesafe.ai/models)、[Cloudflare: Clef-flash](https://developers.cloudflare.com/workers-ai/models/clef-flash/)、[Cloudflare changelog: Introducing Clef](https://developers.cloudflare.com/changelog/post/2026-10-01-clef-workers-ai/)

## 5. ライフサイクル

```mermaid
sequenceDiagram
    participant D as channel-discord
    participant A as 本線 actor
    participant R as router
    participant M as Main Profile Engine
    participant W as worker
    participant C as compactor

    D->>A: Judge(デバウンス済み発言群)
    A->>R: state + 質問セット
    alt silent
        R-->>A: クリーン発言はバッファへ
    else reply_only
        A->>M: バッファ昇格、Layer 4 = 関連チャンク + トリガー
        M->>D: 応答
        A->>A: トリガー + 応答を Layer 3 へ昇格
    else spawn_task
        A->>M: バッファ昇格、Layer 4 = 関連チャンク + トリガー
        M->>D: Ack
        A->>A: トリガー + Ack(+ Goal)を Layer 3 へ昇格
        A->>W: Layer 0〜3 + tools を引き継ぎ Fork(並行)
        W->>W: tool 実行(隔離コンテキスト)
        W->>A: Join(run_id)
        A->>R: 結論メモ + Fork 以降のログ
        R-->>A: delivery_style
        A->>M: 結論メモを Layer 3 に追記
        M->>D: 報告
    end
    Note over A: 人間の発言が N 分間なく、実行中の Worker もいない
    A->>C: Compact(ターン範囲)
    C-->>A: 要約を Layer 2 へ、範囲を置換
```

### パターン A: 静観(`intervention_action == silent`)

1. router が `silent` と判定する。本線は起動しない
2. `is_noise_or_meme == false` の発言だけを未昇格クリーンバッファに入れる

### パターン B: 通常の会話介入(`intervention_action == reply_only`)

1. 未昇格クリーンバッファの発言を TopicChunk ごとに1ブロックにまとめて Layer 3 に追記する
2. router が true と判定した `relevant_chunk_<id>` のチャンク本文と今回のトリガー発言を Layer 4 にセットする
3. 本線が推論し、Channel 経由で発言する。生成中は typing インジケータを出す。Channel は 2,000 文字を超える応答を分割投稿し、`allowed_mentions` を空にして `@everyone` やロールメンションを抑止する
4. provider のエラー(429 / 5xx)は `retry_max` 回まで再試行し、それでも失敗したら typing を止め、応答を投稿せずにターンを終了する。トリガー発言は昇格せずバッファに戻す
5. Layer 4 の過去チャンクを取り除き、トリガー発言と本線の応答を Layer 3 末尾へ確定追記する

### パターン C: 自律タスク起動(`intervention_action == spawn_task`)

`worker` module が無効な構成では、Core がこの判定を `reply_only` に読み替えてパターン B を実行します。

1. **Ack と昇格**
   - パターン B の 1〜2 と同じ手順で Layer 3 / 4 を準備する
   - 本線が「それ、ちょっと調べてみますね」などの宣言(Ack)を投稿し、同時に Worker への指示(Goal)を生成する。Ack と Goal の書式は Layer 0 で規定する。Goal は Discord には出さない
   - Ack はトリガー発言のチャンネルへ新規メッセージとして投稿する
   - トリガー発言と本線の応答(Ack + Goal)を Layer 3 に昇格する。**Fork より前に昇格する**ことで、Worker の文脈にもトリガーと Goal が含まれる
   - 本線の応答がテキストの Ack ではなく `tool_calls` だった場合は、Core が次のように処理する
     - Layer 0 の定型文から Ack を作って投稿する
     - tool 呼び出しの内容から Goal を組み立て、「定型 Ack + Goal」をテキストの assistant メッセージとして Layer 3 に記録する
     - 元の `tool_calls` は履歴に残さない。残すと `tool_result` が必須になり、消すと assistant が空になるため
     - `task_category` は tool 名から決める(`web_search` / `web_fetch` / `web_agent` → `web_research`、`shell` → `code_or_tool`)
   - Worker の同時実行数が `max_concurrent` に達していたら run をキューに入れ、先に入ったものから順に実行する(FIFO)。Ack は投稿済みなので、待ち時間も `timeout_secs` に含める
2. **Fork**
   - 本線の Layer 0〜3 と tool 定義をそのまま引き継いで、バックグラウンドで Worker を起動する。本線と Worker は同一 provider・同一モデル・同一 tool 定義・同一 `tool_choice`(未指定)なので、Worker の初回呼び出しは本線の KV キャッシュに当たる([architecture/caching.md §2](architecture/caching.md#2-本線と-worker-のキャッシュ共有))
   - Worker 固有の指示(「これはバックグラウンド作業である」「結論メモの書式」「Goal」)は **最初の user メッセージ** に置く。`system` には1文字も足さない。理由は [architecture/caching.md §3](architecture/caching.md#3-worker-固有の指示の置き場) を参照
   - Worker は隔離された自分のコンテキストで tool を実行し、呼び出しと結果をコンテキストの末尾に追記する。反復は `max_iterations` 回まで、各 tool の出力は `tool_output_max_chars` で切り詰める。`tool-shell` は `task_category == code_or_tool` の run でだけ承認され、`cwd` は run ごとの `$NOTH_HOME/work/<run_id>/`
   - Worker の実行中も本線は別の会話を処理できるため、雑談が進めばパターン B で通常どおり応答できる
3. **Join と報告**
   - Worker は完了時に途中の tool 出力をすべて捨て、構造化された結論メモ(数百トークン)だけを出力して終了する。timeout・API エラー・反復上限で失敗したときは Core が `[内部調査失敗 <run_id>: <理由>]` を結論メモの代わりにする
   - actor が `Join(run_id)` を処理し、router の合流判定(§4.3)を実行する
   - `abort` なら結論メモは SQLite に残すだけで Layer 3 には入れず、Ack はそのままにする
   - `abort` 以外なら、次の順に報告する
     1. 未昇格バッファを昇格する
     2. Layer 3 に `[内部調査完了 <run_id>: <結論メモ>]` を user メッセージとして1ブロック追記する
     3. Layer 4 には `delivery_style` の指示だけを入れ、本線が発言する。`reply_anchor` のときだけ `worker_runs.trigger_message_id` への Discord リプライとして投稿する。Ack メッセージは編集しない
     4. 応答を Layer 3 に昇格する。Layer 4 の指示は残さない

### パターン D: アイドル時コンパクション

`compactor` module が無効な構成では、Layer 3 がハード上限を超えた時点で古い TopicChunk を要約せずに破棄します。

**通常トリガー**: 人間の最終発言から N 分(既定 10 分)経過。Noth 自身の投稿はアイドルを解除しない。Worker が実行中なら完了を待ってから始める。

1. actor が `Compact(ターン ID 範囲)` を処理する。対象は Layer 3 のうち「残す直近ターン」(`keep_recent_turns`、既定 6)を除いた範囲。要約生成中に届いた発言はキューで待つ
2. 対象範囲を三人称の客観的なエピソード要約に変換し、Layer 2 に統合する。要約の生成には Layer 1(Profile)を入れず、Layer 0 の規約だけを与える。キャラクターの口調が要約に混ざり、本線の文体を乱すのを防ぐため
3. Layer 3 から対象範囲を取り除く。新しい Layer 2 を SQLite に保存する。Layer 2 が `digest_max_chars` を超えたら古いエピソードから削除する

**緊急トリガー**: アイドルにならないまま直近のレスポンスの `usage.input_tokens`(tools + Layer 0〜4 の合計)が `[context] prompt_hard_limit_tokens` を超えた場合。

1. 古い TopicChunk から順に、そのチャンクに属する Layer 3 のターンを要約して Layer 2 へ退避する。チャンク境界(`is_topic_shift` で切った位置)で切るため話題の分断は起きない
2. このときのキャッシュ無効化は許容する。キャッシュの維持より、上限超過の回避を優先するため

## 6. SQLite に持つデータ

ファイルは `$NOTH_HOME/data/noth.sqlite` の1つで、すべてのテーブルが `guild_id` を持ちます。

| テーブル | 内容 |
|---|---|
| `messages` | `allowed_users` と Noth 自身の発言の生ログ。チャンネル、投稿者、`is_self`、本文、時刻、所属 TopicChunk、`is_noise_or_meme`、自分宛てのメンション・リプライか、`decision_id`(NULL なら未判定)、`promoted_turn_id`(NULL なら未昇格) |
| `topic_chunks` | TopicChunk の ID、チャンネル、開始・終了時刻、候補用の機械要約、Compactor による要約が済んでいるか |
| `decisions` | router の判定結果と各回答の確率・confidence、レイテンシ、判定したモデル(`jev/jev-latest` など) |
| `active_log` | Layer 3 のターン列。各ターンの種別(バッファ / トリガー / 応答 / 結論メモ)、`chunk_id`、その時点の `usage` |
| `digests` | Layer 2 の履歴 |
| `worker_runs` | `run_id`、Goal、`task_category`、`trigger_message_id`、`channel_id`、状態(queued / running / finished / failed / aborted)、開始・終了時刻、結論メモ、`delivery_style` |
| `heartbeat` | 常駐プロセスの最終更新時刻と接続状態。`noth status` がプロセス外から読む |

`allowed_users` 以外のユーザーと Bot(Noth 自身を除く)の発言は Channel で捨てるため、どのテーブルにも入りません。

### 6.1 再起動時の復元

1. `worker_runs` で queued / running 状態の run を aborted にする。Ack は投稿済みだが報告はしない
2. `active_log` から Layer 3 を復元し、`digests` の最新から Layer 2 を復元する
3. `messages` のうち `promoted_turn_id` が NULL で `decision_id` があるものを未昇格バッファに戻す
4. `decision_id` が NULL の発言(判定前に落ちたもの)はまとめて1つの `Judge` としてキューに積む
