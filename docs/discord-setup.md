# Discord Bot セットアップ

Noth を Discord サーバーに常駐させるための Bot 作成から `noth run` までの手順です。設定ファイルの全項目は [cli.md](cli.md)、`allowed_users` を使う理由は [architecture/security.md §2](architecture/security.md#2-discord-側の対策) を参照してください。

## 1. Bot が必要とするもの

`channel-discord`(MVP)は次の最小構成で動きます。

| 項目 | 内容 |
|---|---|
| Gateway intents | `GUILDS` / `GUILD_MESSAGES` / `MESSAGE_CONTENT`。特権インテントは `MESSAGE_CONTENT` だけ |
| 行う操作 | メッセージ受信、送信(リプライ含む。`allowed_mentions` は空)、typing インジケータ、絵文字リアクション。`command-context` が有効なら guild コマンド `/noth` の登録と応答([§2.6](#26-管理コマンド-noth-context-を使う)) |
| メンバー情報 | `GUILD_MEMBERS` は要求しない。`allowed_users` の各 ID について REST の Get Guild Member で取得する |

## 2. 手順

### 2.1 アプリを作ってトークンを発行する

1. [Developer Portal](https://discord.com/developers/applications) で **New Application** を作る
2. 左メニューの **Bot** を開き、**Reset Token** でトークンを発行してコピーする
3. トークンは発行時にしか表示されない。紛失したらもう一度 Reset Token で再発行する。再発行すると古いトークンは無効になるので、`.env` も更新する

### 2.2 MESSAGE_CONTENT を有効にする

**Bot** ページの **Privileged Gateway Intents** で **Message Content Intent** をオンにして保存します。

- 参加サーバーが 100 未満なら審査(verification)は不要
- 有効化せずにこのインテントを要求して接続すると、Gateway が close code `4014` で切断する
- `Server Members Intent` と `Presence Intent` はオンにしなくてよい

### 2.3 招待 URL を作ってサーバーに追加する

**OAuth2** の URL Generator で、scope に `bot` だけを選びます。Bot 権限は次の 5 つだけにします。

| 権限 | 必要な理由 | 値 |
|---|---|---|
| View Channels | チャンネルのメッセージを受け取る | `1 << 10` |
| Send Messages | 発言する | `1 << 11` |
| Send Messages in Threads | スレッド内で発言する | `1 << 38` |
| Read Message History | リプライとして投稿する(Create Message の要件) | `1 << 16` |
| Add Reactions | 発言はせず絵文字リアクションだけで反応する | `1 << 6` |

合計の permissions 整数値は `274877975616` です。URL は次の形になります。`<CLIENT_ID>` は **General Information** の Application ID です。

```text
https://discord.com/oauth2/authorize?client_id=<CLIENT_ID>&scope=bot&permissions=274877975616
```

`bot` scope だけなら `redirect_uri` や `response_type` は不要です。URL を開いて追加先のサーバーを選びます。管理者権限(Administrator)は付けません。

スラッシュコマンド(`/noth context`、[§2.6](#26-管理コマンド-noth-context-を使う))の登録に要る `applications.commands` scope は、Discord の公式ドキュメントでは `bot` scope に含まれると書かれています。そのため `bot` だけの招待 URL のままで足ります。`noth run` のログに「スラッシュコマンドを登録できませんでした」と出て 403 になるときは、古い招待で追加した Bot の可能性があるので、`scope=bot%20applications.commands` にした URL で招待し直してください(Bot の権限は変わりません)。

### 2.4 サーバー ID とユーザー ID を控える

Discord の **ユーザー設定 → 詳細設定 → 開発者モード** をオンにします。サーバーアイコンの右クリックで **サーバー ID をコピー**、メンバーの右クリックで **ユーザー ID をコピー** できます。Noth と話せるメンバー全員分の ID が要ります。

### 2.5 Noth を設定して起動する

```sh
noth init --non-interactive   # $NOTH_HOME に雛形を生成(値はプレースホルダ)
```

`$NOTH_HOME/.env`(既定 `~/.noth/.env`)にトークンを書きます。

```dotenv
DISCORD_BOT_TOKEN=<2.1 のトークン>
```

`$NOTH_HOME/config.toml` の `[channel.discord]` を埋めます。ID は数値ではなく文字列で書きます。

```toml
[channel.discord]
guild_id      = "123456789012345678"
token         = "${DISCORD_BOT_TOKEN}"
allowed_users = ["111111111111111111", "222222222222222222"]
master        = "111111111111111111"   # 管理コマンドを使える唯一のユーザー(省略可。2.6)
```

router と provider のキーも `.env` に必要です(手順は [cli.md §4](cli.md#4-設定ファイル))。そのうえで検査して起動します。

```sh
noth doctor   # トークン有効性、guild 参加、MESSAGE_CONTENT intent などを確認
noth run      # フォアグラウンドで常駐
```

OS のサービスとして常駐させる `noth service`([cli.md §2](cli.md#noth-service))は MVP では未実装です。

### 2.6 管理コマンド `/noth context` を使う

`command-context` が有効(既定のビルドは有効)で、`[channel.discord] master` にユーザー ID を 1 人書くと、起動時に guild コマンド `/noth`(サブコマンド `context`)が登録されます。サーバーでスラッシュを入力して `/noth context` を選ぶと、main の文脈の使用量が表示されます。

- 結果は実行した本人にだけ見えるメッセージ(ephemeral)で、**更新**ボタンで同じメッセージを最新の値に書き換えられる
- 実行できるのは `master` だけ。ほかの人が実行すると、その人にだけ「権限がありません」と返る
- `master` を書かないと、コマンドは登録されず、誰にも使えない。`master` を消して再起動すると、登録済みのコマンドも外れる
- 表示は Components V2 で送る。Discord に拒否されたときは embed、さらに通常のテキストに切り替える
- 表示するのは層ごとのトークン数の割合、圧縮の基準(`prompt_hard_limit_tokens`)までの残り、キャッシュの読み込み・書き込み、モデル名、Layer 3 のターン数、Worker の数などで、会話の本文は出さない

## 3. よくあるつまずき

| 症状 | 原因と対処 |
|---|---|
| 接続直後に切断される(close code `4014`) | `MESSAGE_CONTENT` が未有効。2.2 を行う。`noth doctor` でも検出される |
| 発言しても反応しない | 発言者の ID が `allowed_users` に無い。リストに無いユーザーの発言は保存もされない(他 Bot の発言は本文の無い目印だけが残る) |
| `!` で始めた発言に反応しない | 仕様。`!`(全角の `！` も)で始まる発言は Noth に向けないメモとして扱い、判定も保存もしない |
| 特定のチャンネルでだけ反応しない・発言できない | そのチャンネルで Bot のロールに View Channels / Send Messages が無い。チャンネル側の権限上書きを確認する |
| スレッドで発言できない | Send Messages in Threads が無い。招待し直すか、ロールに権限を足す |
| `/noth` が候補に出ない | `master` が未設定、または `command-context` が無効。ログに「スラッシュコマンドを登録できませんでした」とあれば §2.3 の招待の scope を確認する。再起動で同期される |
| `/noth context` が「権限がありません」と返す | 実行者が `master` ではない、または `master` の ID が違う |
| `noth doctor` がトークン無効と報告する | トークンを Reset した後に `.env` を更新していない。コピー時の空白・改行の混入も確認する |
| `noth doctor` が guild 未参加と報告する | 招待 URL でサーバーに追加していない、または `guild_id` が違う(サーバー ID であってチャンネル ID ではない) |

## 出典

- Gateway の Privileged Intents: <https://docs.discord.com/developers/events/gateway>
- Permissions(ビットフラグの値): <https://docs.discord.com/developers/topics/permissions>
- OAuth2 の `bot` scope と認可 URL: <https://docs.discord.com/developers/topics/oauth2>
- Get Guild Member / List Guild Members: <https://docs.discord.com/developers/resources/guild>
- Create Message(リプライに必要な権限): <https://docs.discord.com/developers/resources/message>
- `applications.commands` scope とコマンドの登録(Create Guild Application Command): <https://docs.discord.com/developers/interactions/application-commands>
- Interaction の応答(defer、元の応答の編集、15 分のトークン): <https://docs.discord.com/developers/interactions/receiving-and-responding>
- Components V2(`IS_COMPONENTS_V2`、部品数の上限 40): <https://docs.discord.com/developers/components/reference>、<https://docs.discord.com/developers/components/using-message-components>
