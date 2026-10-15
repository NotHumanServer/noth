# Noth アーキテクチャ: 信頼境界と tool-shell の運用

本書は信頼境界の外の文字列がプロセスに入る経路と、それぞれへの対策、tool の承認ポリシー、`tool-shell` の運用上の注意を定義します。`Channel` / `Tool` trait は [traits.md](traits.md)、`allowed_users` などの設定項目は [cli.md §4.2](../cli.md#42-configtoml) を参照してください。

## 1. 信頼境界

信頼境界の外の文字列がプロセスに入る経路は2つあります。

| 経路 | 入ってくるもの | 到達範囲 |
|---|---|---|
| Discord | `allowed_users` 以外のユーザーや他 Bot の発言、引用・転送・埋め込み・添付 | Channel の時点で捨てる。SQLite にも残さない |
| Web | `tool-web-*` が取得したページ本文 | Worker のコンテキストにだけ入る。本線には結論メモを通じてしか届かない |

## 2. Discord 側の対策

- `allowed_users` にないユーザーと Bot(Noth 自身を除く)の発言は Channel の時点で捨て、SQLite にも残さない
- メッセージの `content` だけを読む。`referenced_message`(リプライ引用)、`message_snapshots`(転送)、`embeds`(URL プレビュー)、添付には `allowed_users` に含まれないユーザーや外部サイトの文字列が入り得るため、読まない
- `content` 内のユーザーメンションは `allowed_users` なら表示名に解決し、それ以外は `<user>` に置き換える
- `tool-discord` が REST で取得した発言にも同じ方針を適用する。`allowed_users` 以外のユーザーと Noth 以外の Bot の発言は `{id, author: "<user>", at}` だけを返し、本文・添付・埋め込み・リプライ引用は返さない。取得した発言内のメンションも上記と同じ規則で置き換える
- `tool-discord` は `[channel.discord] guild_id` のサーバーだけを対象にする。`guild_id` を tool の引数に持たず、`channel_id` はそのサーバーに属するか確認してから使う。`member_info` は `allowed_users`(と Noth 自身)に限る。tool は読み取り専用で、投稿・編集・削除・ロール変更などは持たない
- `tool-discord` は **出力が届く先の全員に見えるチャンネルだけ**を読む。出力が届く先とは次の和集合である。(a) 依頼した人: Worker を起動させたトリガー発言群の発言者(Noth 自身を除く)。(b) トリガーのチャンネルを `VIEW_CHANNEL` できる `allowed_users` 全員。(c) `@everyone` ロールがトリガーのチャンネルを見られるなら `@everyone`(@everyone の基本権限と @everyone の overwrite だけを持つ疑似読者)。この全員が読めるチャンネルに限る。調べた内容は結論メモとしてトリガーのチャンネルに投稿され、Layer 3 にも入るため、依頼した人だけでなく出力先を見られる人にも渡る前提で守る。ただし守る対象は上の (a)〜(c) だけで、`allowed_users` 以外でロール経由に出力先を見ている人は含まない。`allowed_users` には信頼できる人だけを入れる前提である(§4 と同じ)。トリガーのチャンネルは依頼した人が発言した場所なので、権限(`READ_MESSAGE_HISTORY` を含む)を再計算せず常に読める。出力先が分からない(トリガーのチャンネルを特定できない)ときは、トリガー以外のチャンネルを読ませない。#general から #staff は読めず、#staff の中からなら読める。他の人の発言に仕込まれた指示でも、出力先の閲覧者より広い範囲は読めない。ただし `tool-shell` が有効な構成では、shell から `.env` や SQLite を読んだり Discord API を直接呼んだりできるので、この制限は保証にならない。権限は Discord の規則(@everyone と所属ロールの和 → ADMINISTRATOR・サーバーオーナーは全権限 → チャンネルの @everyone、ロール、メンバーの overwrite の順に deny / allow)で計算し、`VIEW_CHANNEL` と `READ_MESSAGE_HISTORY` を要する(計算規則: <https://docs.discord.com/developers/topics/permissions>)。公開スレッドは親チャンネルの権限に従い、非公開スレッドはトリガーのチャンネルである場合に限る。依頼した人を特定できない(発言者が分からない、サーバーのメンバーでない)ときは、トリガーのチャンネルだけ読める。拒否時のエラーはチャンネル名や内容を含めない。`list_channels` は全員が見られるチャンネルだけを返し、`search_messages` は読めないチャンネルの行と、削除済みなど確認できないチャンネルの行を捨てる。ロール・メンバー・チャンネルの情報は 60 秒キャッシュするため、権限の変更は最大 60 秒遅れて反映される。存在しない・別のサーバー・権限なしのチャンネルは同じ拒否文にして、存在の手がかりにしない。`search_messages` は権限を確認できなかった(429/5xx/通信失敗)行を捨てたとき `skipped_unverifiable` で件数を、ページ数の上限で打ち切ったとき `truncated: true` で知らせる。なお Discord の進捗行(`-# 💬 discord ...`)は tool の入力から作るので、権限で拒否された呼び出しでも検索語やチャンネル ID が表示され得る
- 既知の制約: Layer 3(本線の会話文脈)はチャンネルをまたいで共有される。あるチャンネルでの会話が、別のチャンネルでの Noth の応答に現れ得る。`tool-discord` の閲覧権限の確認はこの経路までは塞がない
- `tool-discord` の `search_messages` は Noth の SQLite に記録した時点の発言だけを引く。Discord 側での編集・削除は反映されず、チャンネル名も記録時のものになる。記録は上記の方針で `allowed_users` と Noth の発言に限られるので、API 取得と同じ範囲の情報しか返らない
- 全チャンネルの本文を読むには Discord Developer Portal で `MESSAGE_CONTENT` 特権インテントを有効にする必要がある(100 サーバー未満なら審査不要)

## 3. Web 側の対策と承認ポリシー

Worker が tool を呼ぶ前に `ApprovalPolicy::check(&ToolSpec, &input, &ToolContext)` を通します。MVP の実装は `auto` で、次の1つだけを固定の規則として持ちます。

- `tool-shell` は `task_category == code_or_tool` の run でだけ許可し、逆に `code_or_tool` の run では Web 系の tool を拒否する。1 つの run では Web か shell のどちらかしか使えない。shell を許す run で Web を読めると、取得したページに書かれた指示で shell を動かされる経路が残るため。`web_research` / `fact_check` の run は Web から取得した文字列を読むので、そこに書かれた指示でシェルを実行させない(間接プロンプトインジェクション → RCE の経路を断つ)。tool 定義自体は全 run で同一のまま([caching.md §2](caching.md#2-本線と-worker-のキャッシュ共有))なので、拒否は `tool_result` のエラーとして Worker に返る

tool の出力は Worker のコンテキストにだけ入り、本線には届きません。将来 Discord のボタン承認などを追加できるよう、承認を差し込むインターフェースは Core に置きます。tool のサンドボックスと対話的な承認は MVP に含めません([overview.md §5](overview.md#5-mvp-に含めないもの))。

## 4. tool-shell の運用

`tool-shell` はサンドボックスなしで動きます。Discord の発言が起点で任意のコマンドが走るため、Noth は専用の VM か専用ユーザーで動かし、`allowed_users` には信頼できるメンバーだけを入れてください。

`cwd` は run ごとに分けた `$NOTH_HOME/work/<run_id>/` です。run ごとに分けるのは、複数の Worker を同時に実行してもファイルが衝突しないようにするためです。このディレクトリは run の終了後も残します。

## 5. OpenAI Codex のトークン

`provider-openai` の `preset = "codex"` は、ChatGPT のサブスクリプションで OpenAI Codex のバックエンド(`https://chatgpt.com/backend-api/codex`)を呼びます。認証は `noth auth codex` の device code ログインで得た OAuth トークンです。

- トークンは `$NOTH_HOME/data/codex-auth.json`(0600)に置く。refresh_token は更新のたびに入れ替わり、Noth 自身が書き換える必要があるため、シークレットは `.env` にだけ置く原則([cli.md §4.1](../cli.md#41-シークレットの扱い))の唯一の例外にする
- 書き込みは一時ファイルからの rename で行い、途中で落ちても壊れたファイルを残さない
- トークンは `chatgpt.com` の Codex エンドポイントにだけ送る。`preset = "codex"` では `base_url` の指定を拒否する
- リクエストには `originator: noth` と `User-Agent: Noth/<version>` を付ける。OpenAI は他社製のハーネスに名乗ることを求めている(hermes-agent `website/docs/user-guide/configuration.md`「OpenAI Codex request identity」)
- 他社製ツールからの利用を OpenAI が規約で明示的に認めた一次情報は未確認(2026-10-15)。規約の変更で使えなくなる可能性がある

