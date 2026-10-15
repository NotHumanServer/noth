# Discord Bot セットアップ

Noth を Discord サーバーに常駐させるための Bot 作成から `noth run` までの手順です。設定ファイルの全項目は [cli.md](cli.md)、`allowed_users` を使う理由は [architecture/security.md §2](architecture/security.md#2-discord-側の対策) を参照してください。

## 1. Bot が必要とするもの

`channel-discord`(MVP)は次の最小構成で動きます。

| 項目 | 内容 |
|---|---|
| Gateway intents | `GUILDS` / `GUILD_MESSAGES` / `MESSAGE_CONTENT`。特権インテントは `MESSAGE_CONTENT` だけ |
| 行う操作 | メッセージ受信、送信(リプライ含む。`allowed_mentions` は空)、typing インジケータ、絵文字リアクション |
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
```

router と provider のキーも `.env` に必要です(手順は [cli.md §4](cli.md#4-設定ファイル))。そのうえで検査して起動します。

```sh
noth doctor   # トークン有効性、guild 参加、MESSAGE_CONTENT intent などを確認
noth run      # フォアグラウンドで常駐
```

OS のサービスとして常駐させる `noth service`([cli.md §2](cli.md#noth-service))は MVP では未実装です。

## 3. よくあるつまずき

| 症状 | 原因と対処 |
|---|---|
| 接続直後に切断される(close code `4014`) | `MESSAGE_CONTENT` が未有効。2.2 を行う。`noth doctor` でも検出される |
| 発言しても反応しない | 発言者の ID が `allowed_users` に無い。リストに無いユーザーと他 Bot の発言は保存もされない |
| `!` で始めた発言に反応しない | 仕様。`!`(全角の `！` も)で始まる発言は Noth に向けないメモとして扱い、判定も保存もしない |
| 特定のチャンネルでだけ反応しない・発言できない | そのチャンネルで Bot のロールに View Channels / Send Messages が無い。チャンネル側の権限上書きを確認する |
| スレッドで発言できない | Send Messages in Threads が無い。招待し直すか、ロールに権限を足す |
| `noth doctor` がトークン無効と報告する | トークンを Reset した後に `.env` を更新していない。コピー時の空白・改行の混入も確認する |
| `noth doctor` が guild 未参加と報告する | 招待 URL でサーバーに追加していない、または `guild_id` が違う(サーバー ID であってチャンネル ID ではない) |

## 出典

- Gateway の Privileged Intents: <https://docs.discord.com/developers/events/gateway>
- Permissions(ビットフラグの値): <https://docs.discord.com/developers/topics/permissions>
- OAuth2 の `bot` scope と認可 URL: <https://docs.discord.com/developers/topics/oauth2>
- Get Guild Member / List Guild Members: <https://docs.discord.com/developers/resources/guild>
- Create Message(リプライに必要な権限): <https://docs.discord.com/developers/resources/message>
