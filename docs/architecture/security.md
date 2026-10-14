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

