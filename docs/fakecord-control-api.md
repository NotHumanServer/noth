# fakecord 操作用 API(`/_fake/...`)

fakecord の CLI(`crates/noth-fakecord/src/cli.rs`)は、このページの API だけを叩きます。実装は `crates/noth-fakecord/src/control.rs` です。リクエストとレスポンスの Rust 型は `control.rs` に置いてあり、ここの JSON はそれと一致させます。

Discord 互換の REST(`/api/v10/...`)とは別物です。Bot(noth)はこの API を使いません。

## 共通

- ベース URL は `http://127.0.0.1:3900`(`serve --port` で変わります)。127.0.0.1 からしか届きません。
- リクエストとレスポンスはどちらも JSON です。POST / PATCH には `Content-Type: application/json` を付けます。
- ID は文字列で出します(Discord と同じ)。リクエストでは文字列と整数のどちらも受けます。
- エラーは `{"error": "<理由>"}` で、ステータスは次のとおりです。`wait-reply` のタイムアウトはエラーにしません。
  - 400: 入力が不正(必須項目がない、カテゴリに発言しようとした、など)
  - JSON そのものが読めないときだけは axum の既定の応答になり、本文は `{"error": ...}` ではなくプレーンテキストです(JSON の構文エラーは 400、`Content-Type` 違いは 415、型の不一致や必須項目の欠落は 422)。CLI は本文をそのまま `error:` に出します。
  - 404: チャンネル、ユーザー、メッセージが見つからない
- 名前の解決:
  - チャンネルは名前(先頭の `#` は無視)か ID で指定します。同名が複数あるときは ID が小さい方を使います。スレッドも名前で指定できます。
  - ユーザーはユーザー名(大文字小文字を区別しない。先頭の `@` は無視)か ID で指定します。
  - 発言者 `as` を省略したときは `kurazu` です。チャンネルを省略できる操作(slash、click、react)では `general` です。
- 状態を変える操作はすべて、対応する Gateway イベントを配信します(末尾の表)。配信は状態の更新と同じロックの中で、更新の後に `AppState::publish` で行います。
- `cursor` は Bot の操作ログ(`World.bot_actions`)の位置です。操作の直前の末尾 seq(記録がなければ 0)を返します。`wait-reply` の `since` に渡すと、その操作より後の Bot の動きだけを見られます。send、slash、click、react は実行時に `World.cursor_mark` を同じ値に更新します。

## POST /_fake/send

発言します。Bot ユーザーとして発言することもできます。

```json
{ "channel": "general", "as": "kurazu", "content": "のす、おはよ", "reply_to": "1560797906099765300" }
```

- `channel` は必須、`as` と `reply_to` は省略可です。`content` は必須で、空文字は 400 です。
- `channel` が種別 4(カテゴリ)なら 400 です。
- 本文のメンション:
  - `<@ID>` はそのまま `mentions` に入れます。
  - 本文中の `@name` が既知のユーザー名と一致したら(長い名前を先に照合)`<@ID>` に置き換えて `mentions` に入れます。
- `reply_to` を指定すると、同じチャンネルのメッセージでなければ 404 です。存在するときは `kind = 19`、`message_reference = {message_id, channel_id, guild_id}` にし、返信先の作者も `mentions` に加えます(Discord の既定の挙動)。
- 返信のメッセージには `message_reference` に加えて、参照先を `referenced_message` として 1 段だけ埋め込みます(Gateway と REST のどちらの JSON も同じ)。参照先がさらに返信でも、その中の `referenced_message` は付けません。参照先が無いときは `null` です。noth はこれで「noth の発言への返信か」を判定します。
- 作ったメッセージを `MESSAGE_CREATE` で配信します。`flags` は 0、`components` は空です。

レスポンス(200):

```json
{ "message": { "id": "...", "channel_id": "...", "author": { "id": "...", "username": "kurazu", "bot": false }, "content": "...", "...": "..." }, "cursor": 12 }
```

`message` は `World::message_to_discord_json(msg, false)` の形です。

## POST /_fake/channels

テキストチャンネル(種別 0)を作ります。

```json
{ "name": "random", "parent": "雑談" }
```

- `name` は必須です。小文字にし、空白は `-` に置き換えます(Discord と同じ)。結果が空なら 400 です。
- `parent` は省略可です。カテゴリの名前か ID で、種別 4 でなければ 400、見つからなければ 404 です。
- `position` は同じ親(または親なし)の中の最大値 + 1 です。
- `CHANNEL_CREATE` を配信します。

レスポンス(200): `{ "channel": { "id": "...", "type": 0, "name": "random", "parent_id": "..." | null, "...": "..." } }`(`Channel::to_discord_json`)

## POST /_fake/categories

カテゴリ(種別 4)を作ります。

```json
{ "name": "雑談" }
```

- 名前はそのまま使います(正規化しません)。空なら 400 です。
- `CHANNEL_CREATE` を配信します。
- レスポンスは `/_fake/channels` と同じ形です。

## POST /_fake/threads

公開スレッド(種別 11)を作ります。

```json
{ "channel": "general", "name": "相談", "message_id": "1560797906099765300", "as": "kurazu" }
```

- `channel` は親テキストチャンネルで、種別 0 でなければ 400 です。
- `message_id` を指定すると、そのメッセージ(親チャンネルのもの)から作るスレッドになり、スレッドの ID はメッセージの ID と同じにします(Discord と同じ)。なければ 404 です。省略時は新しい ID を採番します。
- `owner_id` は `as` のユーザーです。`parent_id` は親チャンネルの ID です。
- `THREAD_CREATE` を配信します。`d` はスレッドの JSON に `"newly_created": true` を足したものです。
- レスポンスは `/_fake/channels` と同じ形です。

## POST /_fake/thread-list-sync

アクティブなスレッドを全部含む `THREAD_LIST_SYNC` を配信します。本物の Discord が接続の直後などに送るもので、Bot はこれでスレッドの一覧を入れ替えます。

```json
{ "channel": "general" }
```

- `channel` は省略可です。省略時は guild 内の全アクティブスレッド(アーカイブ済みを除く)を同期します。指定すると、その親テキストチャンネルのスレッドだけを同期し、`channel_ids` にそのチャンネルの ID を入れます。テキストチャンネルでなければ 400、見つからなければ 404 です。
- ボディは必須です(省略するなら `{}`)。
- `d` は `{ "guild_id", "threads", "members" }` です(`channel_ids` は `channel` を指定したときだけ)。この 3 つは serenity の `ThreadListSyncEvent` が必須とします。`members` は各スレッドに Bot が参加している `ThreadMember`(`id`、`user_id`、`join_timestamp`、`flags`)です。

レスポンス(200): `{ "guild_id": "...", "threads": 2 }`(`threads` は同期したスレッドの数)

## PATCH /_fake/channels/{channel}

チャンネル、カテゴリ、スレッドの名前を変えます。`{channel}` は名前か ID(URL エンコード済み)です。

```json
{ "name": "random-2" }
```

- 種別 0 と 11 は `/_fake/channels` と同じ正規化をします。種別 4 はそのままです。
- 配信するイベントは、種別 0 と 4 が `CHANNEL_UPDATE`、種別 11 が `THREAD_UPDATE` です。
- レスポンスは `{ "channel": {...} }` です。

## DELETE /_fake/channels/{channel}

チャンネル、カテゴリ、スレッドを消します。ボディはありません。見つからなければ 404 です。

- 種別 0: 配下のスレッドを先に消し(各 `THREAD_DELETE`)、そのチャンネルのメッセージも消してから `CHANNEL_DELETE`(`d` はチャンネルの JSON)を配信します。
- 種別 4: 子チャンネルの `parent_id` を null にして、子ごとに `CHANNEL_UPDATE` を配信します。そのあと `CHANNEL_DELETE` を配信します。
- 種別 11: `THREAD_DELETE` を配信します。`d` は `{ "id", "guild_id", "parent_id", "type": 11 }` です。
- `bot_actions` と `interactions` は消しません。

レスポンス(200): `{ "deleted": { ...消したチャンネルの JSON } }`

## POST /_fake/slash

スラッシュコマンドを実行します。

```json
{
  "name": "noth",
  "path": ["ignore", "add"],
  "options": [{ "name": "channel", "value": "general" }],
  "as": "kurazu",
  "channel": "general"
}
```

- `name` は必須です。`path`、`options`、`as`、`channel` は省略可です。
- `Interaction` を `kind = 2`、`state = pending` で作り、`INTERACTION_CREATE` を配信します。token は一意な文字列なら何でもかまいません(例: 64 文字の 16 進)。
- `d.data` は次の形です。コマンド ID は、Bot が登録済み(`World.commands`、同名)ならそれを、未登録なら新しい ID を使います。未登録でもエラーにしません。

```json
{ "id": "<command id>", "name": "noth", "type": 1, "guild_id": "<guild>", "options": [ ... ] }
```

- `options` の組み立て:
  - `path` が空なら、`options` をそのまま並べます。
  - `path` が 1 つなら `[ { "name": "<sub>", "type": 1, "options": [...] } ]` です。
  - `path` が 2 つなら `[ { "name": "<group>", "type": 2, "options": [ { "name": "<sub>", "type": 1, "options": [...] } ] } ]` です。
  - 3 つ以上は 400 です。
  - 各オプションの `type` は `value` から決めます。文字列は 3、整数は 4、真偽値は 5、小数は 10 です。
- `d` の残りのフィールドは `World::interaction_to_discord_json` が組み立てます。

レスポンス(200): `{ "interaction_id": "...", "token": "...", "cursor": 12 }`

## POST /_fake/click

メッセージのボタンを押します。

```json
{ "custom_id": "noth:refresh:context", "message_id": "1560797906099765400", "channel": "general", "as": "kurazu" }
```

- `custom_id` は必須です。
- メッセージの特定:
  - `message_id` があればそのメッセージを使います。なければ 404 です。
  - なければ `channel`(既定 general)の中で、`components` のどこかに `custom_id` が一致する要素(JSON を再帰的に探します)を持つ最新のメッセージを使います。見つからなければ 404 です。
- `Interaction` を `kind = 3`、`message_id` 付き、`state = pending` で作り、`INTERACTION_CREATE` を配信します。
- `d.data` は `{ "custom_id": "<id>", "component_type": <一致した要素の type。なければ 2> }` です。`d.message` には対象メッセージの JSON(`gateway = false`)が入ります。
- レスポンスは `/_fake/slash` と同じ形です。

## POST /_fake/react

メッセージにリアクションを付けます。

```json
{ "emoji": "👍", "message_id": "1560797906099765400", "channel": "general", "as": "kurazu" }
```

- `emoji` は必須です。Unicode の絵文字そのもの、またはカスタム絵文字の `name:id` です。
- `message_id` がなければ `channel` の最新メッセージを使います。メッセージがなければ 404 です。
- `Reaction` がなければ作り、同じユーザーの二重追加は何もしません(エラーにしない)。
- `MESSAGE_REACTION_ADD` を配信します。`d` は `{ "user_id", "channel_id", "message_id", "guild_id", "emoji": { "id": null | "<id>", "name": "<name>" } }` です。

レスポンス(200): `{ "message_id": "...", "cursor": 12 }`

## GET /_fake/log

メッセージと Bot の操作の記録を返します。

クエリ:

| 名前 | 内容 |
|---|---|
| `channel` | 省略時は全チャンネル。指定すると、そのチャンネルのメッセージと Bot の操作だけ |
| `limit` | 返すメッセージ数の上限(新しい方から数える)。既定 20 |
| `since` | この seq より後の Bot の操作だけ返す。省略時は全部 |

レスポンス(200):

```json
{
  "messages": [ { "id": "...", "content": "...", "...": "..." } ],
  "actions": [ { "seq": 3, "at": "2026-10-17T00:00:00Z", "action": "post", "channel_id": "...", "message_id": "...", "content": "...", "reply_to": null } ],
  "cursor": 3
}
```

- `messages` は古い順で、`message_to_discord_json(msg, false)` の形です。
- `actions` は `BotAction` をそのまま JSON にしたものです。`action` の値は `post`、`edit`、`reaction`、`typing`、`interaction_response`、`interaction_edit`、`interaction_followup` です。各フィールドは `state.rs` の `BotActionKind` を見てください。
- `channel` を指定したとき、チャンネルを持たない interaction 系の操作は、その interaction の `channel_id` で判定します。
- `cursor` は返した時点の `bot_actions` の末尾 seq です。

## POST /_fake/wait-reply

Bot の返信が来るまで待ちます。

```json
{ "since": 12, "timeout_secs": 60, "channel": "general", "settle_ms": 2000 }
```

- 項目はすべて省略可です。`since` の既定は `World.cursor_mark`、`timeout_secs` は 60、`settle_ms` は 2000 です。
- 次の操作を「返信」と数えます。
  - `post`
  - `edit`
  - `interaction_followup`
  - `interaction_edit`
  - `interaction_response`(`response_type` が 4 か 7 のとき)

  `typing`、`reaction`、遅延応答(`response_type` が 5 か 6)だけでは返信になりません。
- 動き:
  1. `seq > since` の操作を 50ms ごとに調べます。`channel` があればそのチャンネルの操作だけを見ます。
  2. 返信が最初に見つかった時点から、新しい操作が `settle_ms` のあいだ来なくなるまで待ちます。
  3. 待っているうちに `timeout_secs` が過ぎたら、その時点までの操作を返します。
  4. `timeout_secs = 0` なら、待たずに今ある分を返します。
- どのケースでも 200 で返します。`timed_out` は、返信が 1 つも来ないまま時間切れになったときだけ true です(このとき `replied` は false)。返信が来たあとで時間切れになった場合は `replied: true`、`timed_out: false` で、それまでの操作を返します。
- 待っているあいだ、World のロックを握ったまま `.await` しないでください。ロックは調べるたびに取り直します。

レスポンス(200):

```json
{ "timed_out": false, "replied": true, "actions": [ { "seq": 13, "action": "typing", "...": "..." }, { "seq": 14, "action": "post", "...": "..." } ], "cursor": 14 }
```

`actions` は `since` より後の操作を seq 順に並べたもので、`cursor` はその最後の seq です(操作がなければ `since` と同じ値)。

## POST /_fake/users

ユーザーを追加して、guild のメンバーにします。

```json
{ "name": "alice", "role": "allowed", "bot": false }
```

- `name` は必須です。同じユーザー名(大文字小文字を区別しない)が既にあれば 400 です。
- `role` は `allowed`(既定)、`master`、`none` のどれかで、ほかは 400 です。`master` は allowed 役と master 役の両方を付けます。`none` は役なしです。
- `bot` は既定 false です。
- 役はサーバー起動時の `World::seed` が作った `allowed` と `master`(名前で引きます)を使います。
- `GUILD_MEMBER_ADD` を配信します。`d` はメンバーの JSON に `guild_id` を足したものです。

レスポンス(200): `{ "user": {...}, "member": {...} }`(`User::to_discord_json`、`World::member_to_discord_json`)

## POST /_fake/snapshot

現在の状態を JSON スナップショットに保存します。

```json
{ "path": "/tmp/fakecord.json" }
```

- `path` は省略可です。省略時は `serve --snapshot` のパス(`World.snapshot_path`)で、それも無ければ 400 です。
- `World::save` を使います(一時ファイル経由で置き換え)。書けなければ 500 です。

レスポンス(200): `{ "path": "<保存した絶対パス>" }`

## GET /_fake/channels, GET /_fake/users, GET /_fake/status

名前の解決や起動確認のための読み取り専用 API です。

- `GET /_fake/channels`: `{ "channels": [ ...Channel::to_discord_json ] }`。スレッドも含め、ID 順です。
- `GET /_fake/users`: `{ "users": [ { ...User::to_discord_json, "roles": ["allowed", "master"] } ] }`。`roles` は guild の役の名前で、`@everyone` は含めません。
- `GET /_fake/status`: `{ "guild_id": "...", "application_id": "...", "bot_user_id": "...", "port": 3900, "cursor": 14, "channels": 3, "messages": 25 }`。`cursor` は `bot_actions` の末尾 seq です。起動待ちのスクリプトはこれが 200 を返すまでポーリングします。

## 配信する Gateway イベント

| 操作 | イベント(`t`) | `d` |
|---|---|---|
| send | `MESSAGE_CREATE` | `message_to_discord_json(msg, true)` |
| channels(作成) | `CHANNEL_CREATE` | チャンネルの JSON |
| categories | `CHANNEL_CREATE` | チャンネルの JSON |
| threads | `THREAD_CREATE` | スレッドの JSON + `"newly_created": true` |
| thread-list-sync | `THREAD_LIST_SYNC` | `{guild_id, threads, members}`(+ `channel_ids`) |
| rename(種別 0 / 4) | `CHANNEL_UPDATE` | チャンネルの JSON |
| rename(種別 11) | `THREAD_UPDATE` | スレッドの JSON |
| delete(種別 0) | `THREAD_DELETE`(配下のスレッド)、`CHANNEL_DELETE` | `{id, guild_id, parent_id, type}` / チャンネルの JSON |
| delete(種別 4) | `CHANNEL_UPDATE`(子)、`CHANNEL_DELETE` | チャンネルの JSON |
| delete(種別 11) | `THREAD_DELETE` | `{id, guild_id, parent_id, type}` |
| slash / click | `INTERACTION_CREATE` | `interaction_to_discord_json` |
| react | `MESSAGE_REACTION_ADD` | 上記 react の `d` |
| users | `GUILD_MEMBER_ADD` | メンバーの JSON + `guild_id` |

Bot 側の REST が引き起こすイベント(Bot の投稿に対する `MESSAGE_CREATE`、編集に対する `MESSAGE_UPDATE`、リアクションに対する `MESSAGE_REACTION_ADD`)は REST の担当(`rest/messages.rs`、`rest/interactions.rs`)が配信します。Bot の typing は `TYPING_START` を配信しません(ephemeral な interaction 応答も `MESSAGE_CREATE` を配信しません)。

## Gateway(`/ws`)の挙動

Discord 本物の Gateway に合わせている点です。

- Identify で `session_id` を発行し、`READY`(seq 1)、`GUILD_CREATE`(guild ごと)の順に送ります。`READY` の `shard` は Identify の `shard` をそのまま返し、`application.flags` は `GATEWAY_MESSAGE_CONTENT_LIMITED`(`1 << 19`)です(`GET /api/v10/oauth2/applications/@me` の `flags` も同じ。noth の `doctor` が要求します)。
- 購読位置の決定と `GUILD_CREATE` のスナップショット作成は、World の同じロックの中で行います。スナップショットに入っている変更のイベントは、そのあと二重に届きません。
- セッションは接続が切れても残ります。セッションごとに、送ったイベントの履歴(seq と payload)を直近 1000 件まで持ち、切断中のイベントも全体のログ(直近 1000 件)から取り込み続けます。
- Resume(op 6)では、Bot が受け取った seq より後のイベントを再送し、続けて `RESUMED`(`d` は `{}`)を送ります。seq はセッションを通して続きます。再開できないとき(セッションが無い、seq が未来、履歴またはログから溢れた)は op 9(`d: false`)を返し、Bot は Identify からやり直します。
- 古い接続がまだ生きているうちに Resume が来たら、セッションは新しい接続に移り、古い接続は閉じます。
- fakecord 側から閉じるとき(終了時)の Close フレームは、code 1012 を付けます。フレーム無しの Close を送ると、serenity が CPU 100% のビジーループに入ります。

## 既知の制限

次は実装していません。

- Identify の `intents` を見ません。Bot がどのインテントを要求しても、全イベントを流します。
- レートリミット(429)を返しません。
- interaction の応答メッセージの `type` は、スラッシュコマンドへの応答だけ 20 にしています。コンポーネント操作への応答は 0 のままで、本物の Discord とは異なる場合があります。
- `wait-reply` は、リアクションだけの応答(`reaction`)を `replied` に数えません。リアクションだけで返す Bot の動きは `log` の `actions` で確認してください。

## CLI との対応

| CLI | API |
|---|---|
| `send` | `POST /_fake/send` |
| `channel create` | `POST /_fake/channels` |
| `channel delete` | `DELETE /_fake/channels/{channel}` |
| `channel rename` | `PATCH /_fake/channels/{channel}` |
| `category create` | `POST /_fake/categories` |
| `thread create` | `POST /_fake/threads` |
| `thread-list-sync` | `POST /_fake/thread-list-sync` |
| `slash` | `POST /_fake/slash` |
| `click` | `POST /_fake/click` |
| `react` | `POST /_fake/react` |
| `log` | `GET /_fake/log` |
| `wait-reply` | `POST /_fake/wait-reply` |
| `user add` | `POST /_fake/users` |
| `snapshot` | `POST /_fake/snapshot` |

`--json` を付けたときは、レスポンスの JSON をそのまま標準出力に出します。付けないときは人が読める短い要約にします。エラーのときは標準エラーに `error` の内容を出し、終了コード 1 にします。`wait-reply` は返信がなくてタイムアウトしたとき(`replied: false`)に終了コード 2 を返します。
