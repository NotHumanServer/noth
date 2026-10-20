# Noth アーキテクチャ: KV キャッシュ戦略

本書は provider ごとの Layer の載せ方、main(Main Profile Engine)と Worker がキャッシュを共有するための制約、コストの目安を定義します。Layer の構造と Append-only 規約は [context-layers.md §3](../context-layers.md#3-main-プロンプトの層構造kv-キャッシュ保護規約)、`Provider` trait は [traits.md §4](traits.md#4-providerllm) を参照してください。

## 1. provider ごとの Layer の載せ方

| provider | Layer 0〜2 | Layer 3 / 4 | キャッシュ |
|---|---|---|---|
| anthropic | `system` ブロック。Layer 1 末尾と Layer 2 末尾に `cache_control`(Layer 1.5 は Layer 2 と同じブロックの先頭) | `messages`。Layer 3 末尾に `cache_control` | 明示ブレークポイント3つ(上限4。トップレベルの自動キャッシュ指定は使わない)。TTL は `cache_ttl` 設定(`5m` 既定 / `1h`) |
| openai | `system` メッセージ(Chat)/ `instructions`(Responses) | `messages`(Chat)/ `input`(Responses) | 自動プレフィックスキャッシュ。境界の指定はないが、Codex preset では `prompt_cache_key` をセッション単位で固定する(下記)。互換サーバーでは機能しないこともある |
| google | `systemInstruction` | `contents` | implicit caching に任せる。明示的なキャッシュ指定はしない(explicit caching は Gemini 4 Argon 以降で非対応のため使わない) |

OpenAI のキャッシュはプレフィックスの一致で決まるため、次の3つを守ります。

- **`prompt_cache_key` を固定する**: Codex preset は `noth-openai-<guild_id>` を本文の `prompt_cache_key` と、ヘッダの `session-id`・`x-client-request-id` に同じ値で入れます。公式 Codex も、キーをセッション ID で固定し(`codex-rs/core/src/client.rs` の `prompt_cache_key`)、`x-client-request-id` にリクエストごとの ID ではなくスレッド ID を入れます。以前は `instructions` と `tools` のハッシュを使っていたため、Layer 0〜2 が変わるとキーも変わりました。公式文書は、GPT-5.6 以降ではキャッシュのルーティングが自動でキーは不要としているので、害を避けるための固定です。Worker は main の base を複製するので同じ `cache_session` を引き継ぎ、同じキーで main のキャッシュを引けます。
- **Responses では同 role のターンを結合しない**: Layer 3 と Layer 4 の各ターンを別の message item のまま送ります。Chat Completions は同 role の連続を受け付けない互換サーバーがあるため、従来どおり結合します。
- **区切りは直近の user メッセージの末尾にしか置かれない**: GPT-5.6 以降の暗黙のキャッシュは、developer 群(instructions)の末尾と、最新の対象メッセージ(user メッセージ、連続する tool 結果の最後)の末尾にだけ区切りを置きます。書き込まれるのはその区切りまでで、次のリクエストはその区切りまで一致したときだけ読めます。そのため、リクエストの最後の user メッセージが次のリクエストに残らないと、instructions の末尾(≒ 6656 トークン)でしかヒットしません。以前は `[発言先]` を別の user メッセージにして Layer 3 へ昇格していなかったため、毎ターンこれに当たっていました。今は `[発言先]` をトリガーのブロックに含め、Layer 4 でも Layer 3 でも同じバイト列にして一緒に昇格します。`[判定]` は昇格しない別の user メッセージで、置かれたターンだけは最後のメッセージが残らず、次の main は 1 つ前の書き込み(前々ターンの trigger の末尾)まで戻ります。Worker の base は fork 時点の main のリクエスト(`[判定]` を含む)を先頭にそのまま残すので、最初のステップは main の書き込みを引けます。Worker のステップどうしは、tool 結果が次に残るので引けます。Join は結論と報告の指示が付くため、fork 時点の main の trigger の末尾までが重なります(`[判定]` があったターンでは trigger の末尾まで)。

この3点は `crates/noth-provider-openai/src/prefix_tests.rs` が、実際の `layers` と `wire` を通してバイト単位で検査します。再利用率は provider の info ログ(`provider の応答を受け取りました`)の `reuse_ratio`(今回の `cached_tokens` ÷ 直前の同じ流れの総入力)で見ます。Compactor は main とは別のキー(`noth-openai-compactor-<guild_id>`)を使います。Worker の最初のステップの `reuse_ratio` は、Worker の指示が末尾に加わるため 1.0 にはなりません。Worker ごとの前回値は直近 64 件までで、超えたら古い run から忘れます。流れは main→main(`main->main`)、main→Worker の最初(`main->worker`)、Worker のステップどうし(`worker->worker`)です。

Anthropic のブレークポイントを3か所に分けるのは、Layer 2 が更新されたときに Layer 0〜1 のキャッシュを保つためと、未昇格バッファの一括追記で前回の Layer 3 末尾から 20 ブロック以上進んだ場合に備えるためです(lookback は 20 ブロック)。未昇格バッファは TopicChunk ごとに1ブロックにまとめて追記します。Layer 0+1 がモデルの最小キャッシュ長(Opus 5.5 で 512 トークン)未満なら1つ目のブレークポイントは機能しませんが、害はありません。

Anthropic の TTL は既定で5分です。サーバーが5分以上静かになると、次のターンで Layer 0〜3 のキャッシュを書き直します。書き込みは入力価格の 1.25 倍です。`cache_ttl = "1h"` にすると書き込みは2倍になりますが、キャッシュが1時間保たれます。

## 2. main と Worker のキャッシュ共有

main と Worker は **同一 provider・同一モデル・同一 tool 定義・同一 `tool_choice`(未指定)** を使います。Anthropic のキャッシュは `tools → system → messages` の順に前方一致を見るため、Worker だけが tool を持つと先頭で不一致になり、Fork しても main のキャッシュを引き継げません。OpenAI 互換も tool 定義を含むプロンプト全体の一致が必要です。

そのため `worker` が有効な構成では **main にも同じ tool 定義を渡します**。main が tool を呼ぶことは Layer 0 で禁じ、それでも `tool_calls` が返ってきたら Core がそれを Worker に回す指示(`[Goal]` 相当)として扱います。main のコンテキストには Layer 0 の定型 Ack と tool 呼び出しから組み立てた Goal をテキストの assistant メッセージとして記録し、`tool_calls` 自体は残しません。残すと `tool_result` が必須になり、消すと assistant が空になるためです。こうして main は tool 定義を持ちながら、実行は常に Worker が担う形になり、[設計原則2](../context-layers.md#1-目的と設計原則)(main にノイズと生ログを入れない)を守ります。

## 3. Worker 固有の指示の置き場

同じ理由で、Worker 固有の指示(バックグラウンド作業であること、結論メモの書式、Goal)は `system` に足さず **最初の user メッセージ** に置きます。`system` が1文字でも違えば Layer 3 のキャッシュは全部外れます。Worker は Layer 3 末尾のブレークポイントを自分のコンテキスト末尾に移し、ブレークポイント数は3のままにします。

このため Worker の `system` には、main 向けの Ack・Goal と「Worker に頼めること」の節も入ります。Worker 自身にはそれが自分向けでないことを最初の user メッセージで伝えます。Compactor は Layer 1 を使わず main とキャッシュを共有しないので、Worker の節を除いた規約を渡します。

## 4. コストの目安

Opus 5.5 で Layer 0〜3 を 20k トークンとした場合の目安です。

| 項目 | 金額 |
|---|---|
| Worker 起動時の初回呼び出し(共有できた場合、キャッシュ読み) | $0.004 |
| 同上(共有できなかった場合、キャッシュ書き込み) | $0.10 |
| main が毎ターン余分に読む tool 定義(1.5k トークン程度) | $0.0003 |

## 5. keep-alive(MVP 外)

`Provider::keep_alive()` は、5分 TTL の期限切れ直前に `max_tokens: 0` のリクエストを送信し、Layer 0〜3 のキャッシュの有効期限を延ばすためのメソッドです。発言の間隔が長い少人数サーバーでは効果が見込めますが、効果の検証を MVP 後に回すため、MVP では呼び出し処理も無効にします。
