# Noth アーキテクチャ: KV キャッシュ戦略

本書は provider ごとの Layer の載せ方、本線(Main Profile Engine)と Worker がキャッシュを共有するための制約、コストの目安を定義します。Layer の構造と Append-only 規約は [context-layers.md §3](../context-layers.md#3-本線プロンプトの層構造kv-キャッシュ保護規約)、`Provider` trait は [traits.md §4](traits.md#4-providersystem-2) を参照してください。

## 1. provider ごとの Layer の載せ方

| provider | Layer 0〜2 | Layer 3 / 4 | キャッシュ |
|---|---|---|---|
| anthropic | `system` ブロック。Layer 1 末尾と Layer 2 末尾に `cache_control` | `messages`。Layer 3 末尾に `cache_control` | 明示ブレークポイント3つ(上限4。トップレベルの自動キャッシュ指定は使わない)。TTL は `cache_ttl` 設定(`5m` 既定 / `1h`) |
| openai | `system` メッセージ(結合または2連) | `messages` | 自動プレフィックスキャッシュ。何もしない。互換サーバでは効かないこともある |
| google | `systemInstruction` | `contents` | implicit caching のみ。何もしない(explicit caching は Gemini 4 Argon 以降で非対応のため使わない) |

Anthropic のブレークポイントを3か所に分けるのは、Layer 2 が更新されたときに Layer 0〜1 のキャッシュを保つためと、未昇格バッファの一括追記で前回の Layer 3 末尾から 20 ブロック以上進んだ場合に備えるためです(lookback は 20 ブロック)。未昇格バッファは TopicChunk ごとに1ブロックにまとめて追記します。Layer 0+1 がモデルの最小キャッシュ長(Opus 5.5 で 512 トークン)未満なら1つ目のブレークポイントは効きませんが、害はありません。

## 2. 本線と Worker のキャッシュ共有

本線と worker は **同一 provider・同一モデル・同一 tool 定義・同一 `tool_choice`(未指定)** を使います。Anthropic のキャッシュは `tools → system → messages` の順に前方一致を見るため、worker だけが tool を持つと先頭で不一致になり、Fork しても本線のキャッシュを引き継げません。OpenAI 互換も tool 定義を含むプロンプト全体の一致が必要です。

そのため `worker` が有効な構成では **本線にも同じ tool 定義を渡します**。本線が tool を呼ぶことは Layer 0 で禁じ、それでも `tool_calls` が返ってきたら Core がそれを `spawn_task` 相当として扱い、Worker に回します。本線のコンテキストには Layer 0 の定型 Ack と tool 呼び出しから組み立てた Goal をテキストの assistant メッセージとして記録し、`tool_calls` 自体は残しません(残すと `tool_result` が必須になり、消すと assistant が空になるため)。本線が tool を「持っているように見えて、実行は常に Worker」という形で、[設計原則2](../context-layers.md#1-目的と設計原則)(本線の無菌化)を保ちます。

## 3. Worker 固有の指示の置き場

同じ理由で、Worker 固有の指示(バックグラウンド作業であること、結論メモの書式、Goal)は `system` に足さず **最初の user メッセージ** に置きます。`system` が1文字でも違えば Layer 3 のキャッシュは全部外れます。Worker は Layer 3 末尾のブレークポイントを自分のコンテキスト末尾に移し、ブレークポイント数は3のままにします。

## 4. コストの目安

Opus 5.5、Layer 0〜3 を 20k トークンとした場合:

| 項目 | 金額 |
|---|---|
| Worker 起動時の初回呼び出し(共有できた場合、キャッシュ読み) | $0.004 |
| 同上(共有できなかった場合、キャッシュ書き込み) | $0.10 |
| 本線が毎ターン余分に読む tool 定義(1.5k トークン程度) | $0.0003 |

## 5. keep-alive(MVP 外)

`Provider::keep_alive()` は 5分 TTL の期限切れ直前に `max_tokens: 0` のダミーを投げて Layer 0〜3 のキャッシュを延命する差し込み点です。沈黙が多い少人数サーバーでは効く見込みですが、効果の検証を MVP 後に回すため、MVP では呼び出し元も無効にします。
