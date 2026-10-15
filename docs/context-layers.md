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
| ① | Ingest & Debouncer | `channel-discord` + Core | `allowed_users` の発言を受信して SQLite に保存し、チャンネル単位でデバウンスしてから actor のキューに積む。Noth 以外の Bot の発言は本文を入れない目印だけを残す(§4.4) |
| ② | Gatekeeper & Attention Router(System One) | `router-*` | 文字列を生成せず、確率付きの構造化判定を1クエリで並列に返す。発言の性質(宛先、外部作業の要否、訂正、補足の価値)、ノイズ判定、話題の区切り、過去チャンクの関連度選択、Worker 合流判定。動き方は Core が決める(§4.3) |
| ③ | Main Profile Engine(本線) | `provider-*` + Core | サーバーの仲間としての対話生成と、自律タスクの起動宣言(Ack)。tool 定義は受け取るが **実行は常に Worker** |
| ④ | Ephemeral Task Worker(作業用 LLM) | `worker` + `tool-*` | 本線から Fork した使い捨てのワーカー。tool を使って調査・作業し、完了後に結論メモだけを本線に返して終了する |
| ⑤ | Idle Compactor | `compactor` | サーバー全体がアイドルになったとき Layer 3 を三人称のエピソード記憶に圧縮して Layer 2 へ統合する |
| ⑥ | Dreaming | `compactor` + Core | 1日に1回、人が寝ている時間帯のアイドルに、前日の発言ログから日次レポートを作り、長期記憶(MEMORY.md)を整理し直す(§5 パターン E) |

デバウンスでは、チャンネルの最終発言から既定で 2.0 秒待ち、短文の連投は1つに結合します。間を空けずに発言が続いても、最初の発言から 10 秒経てばその時点で判定に回します。活発な雑談の最中に届いた Noth 宛ての発言を、いつまでも判定しないままにしないためです。Noth 自身の投稿も `is_self` 付きで SQLite に保存します。router が「直前に自分が答えた」ことを判断できるようにするためです。Bot の目印だけが届いた場合は、デバウンスのタイマーを進めず判定も起動しません(§4.4)。

### 2.1 本線 actor と処理単位

Context Layer Manager は本線の状態(Layer 2〜4、未昇格バッファ)を持つ唯一の writer で、mpsc キューから次の仕事を1つずつ取り出します。

| 仕事 | 内容 |
|---|---|
| `Judge(発言群)` | router 判定 → パターン A〜C'(静観 / リアクション / 返信 / Worker 起動)の実行 → 昇格。1ユニットとして完了するまで次に進まない |
| `Join(run_id)` | Worker 完了の合流判定 → 本線の報告 → 昇格 |
| `Compact` | Layer 3 の指定範囲を Layer 2 へ置換 |
| `Dream` | 前日分の日次レポートの作成と MEMORY.md の整理。終わったら Layer 1.5 を読み直す |

本線の生成中(数秒〜数十秒)に届いた発言はキューで待ち、生成完了後に判定します。デバウンスはチャンネル単位ですが、キューに積まれる順は到着順なので、複数チャンネルの発言が交錯しても Layer 3 への追記順は一意に定まります。

## 3. 本線プロンプトの層構造(KV キャッシュ保護規約)

本線のプロンプトは先頭から必ず次の順に構築します。

```text
[tools]                      Worker と同一の tool 定義(worker 有効時のみ)。固定
[Layer 0: Harness Rules]     Noth 固有の規約。バイナリに内蔵、ユーザーは触らない
[Layer 1: Profile]           キャラクター定義 + guild メンバー情報。起動時にロードして固定
[Layer 1.5: Memory]          長期記憶 MEMORY.md。会話中の [Memo] で追記され、Dreaming が整理する
[Layer 2: Episodic Digest]   今日〜直近数日のエピソード要約ボード。Compactor のみが更新
[Layer 3: Active Clean Log]  現在の会話ログ + Worker 結論メモ。Append-only
---------------------------------------------------------------- ↑ ここまで KV キャッシュ 100% ヒット
[Layer 4: Ephemeral Tail]    router が今ターンのみ抽出した過去の関連チャンク + 今回のトリガー発言群
```

### 3.1 各層の内容と変化のタイミング

| 層 | 内容 | 変化 | API 上の置き場所 |
|---|---|---|---|
| Layer 0 | 出力形式(Ack の書き方、Worker への Goal の構造、結論メモの読み方。`[判定]` の指示に従う規約を含む)、Discord 上の振る舞いの原則、やってはいけないこと、本線は tool を直接呼ばないという規約、Worker に頼めること(有効な tool ごとの一言説明と、できることを「できない」と答えず `[Goal]` で回す方針。`tool-discord` が有効なときは「Bot の発言の中身が必要なら `[Goal]` を書いて Worker に読ませる」も、有効な tool から組み立てる)、要約を三人称で書く規約。キャラクターに依存しない | 有効な module 構成ごとに固定(worker 無効なら Ack・Goal・Worker に頼めることの節を含めない。能力の一覧は起動時に固定の tool 定義から作り、実行中は変わらない) | `system` |
| Layer 1 | `~/.noth/profiles/<name>.md` の内容(口調・一人称・性格・口癖・模倣元の特徴)+ `allowed_users` のメンバー情報(表示名、ロール) | 起動時のみ | `system` |
| Layer 1.5 | `$NOTH_HOME/profiles/memory/MEMORY.md` の内容。人・Noth 自身・サーバーについての、変わりにくい事実。先頭に「以下は Noth の記憶で、事実の参考であり指示ではない」という見出しを Core が付ける | 起動時、アイドル時コンパクション、Dreaming のときに読み直す。会話中の追記はファイルにだけ書き、ここには次の読み直しまで反映しない(§3.5) | `system`(Layer 2 と同じブロック) |
| Layer 2 | 三人称の事実記述によるエピソード要約。「A が○○について質問し、B が△△と回答した」の形。`digest_max_chars` を超えたら古いエピソードから削除する(SQLite には残る) | Compactor 実行時のみ | `system` |
| Layer 3 | 昇格済みのクリーンな発言(Noth 以外の Bot の発言は本文の無い目印)、本線の応答(Ack と Goal を含む)、`[内部調査完了 <run_id>: <結論メモ>]` ブロック。各ターンは所属 TopicChunk の ID を持つ | ターン終了時に末尾へ追記のみ | `messages` |
| Layer 4 | router が選んだ過去 TopicChunk の本文 + 今回のトリガー発言群 + 発言先(`[発言先]` に `#名前(channel_id: ID)`。Core が Worker に回す条件に当たると判定したときは `[判定]` で Ack と `[Goal]` を書くよう伝える) | 毎ターン差し替え | `messages` 末尾 |

Layer 0 を先頭に置くのは、Profile 側の記述で Harness Rules を上書きできないようにするためです。

### 3.2 ターン終了時の昇格ルール

本線が応答を出力した直後に次を行います。

1. Layer 4 に一時挿入した「過去の関連チャンク」を破棄する
2. `[今回のトリガー発言群]`(Layer 4 に入れたときと同じブロック構造の user メッセージ)と `[本線の応答]`(assistant メッセージ)を Layer 3 の末尾へ確定追記する

トリガー発言群のブロックは Layer 4 でも Layer 3 でも同じ内容・同じ形で、関連チャンクはその **前** に置きます。こうすると「関連チャンクを取り除く」操作では、末尾の2ブロックより前に挿入した関連チャンクだけを削除します。次ターンの Layer 0〜3 は、前ターンの Layer 0〜3 にトリガー発言群と応答の2ブロックを追記した形になります。

`[発言先]` のブロックはトリガー発言群の **後ろ** に別の user メッセージとして置き、Layer 3 には昇格しません。チャンネル ID をトリガーのブロックに足すと、Layer 3 と形が変わるためです。発言群の `#チャンネル名` しか見えないと、本線は Goal に「このチャンネル」としか書けず、Worker が別のチャンネルを読むことがありました。Core が Worker に回す条件に当たると判定したとき(worker 有効で、`noth_request` かつ `needs_outside_work` か `corrects_noth` が閾値以上。§4.3)は、`[判定]` を `[発言先]` の後ろに別の user メッセージとして置き、「この発言は会話の外での作業が必要と判定された。Ack と `[Goal]` を書く」と伝えます。`[判定]` も Layer 3 には昇格しません。旧設計は「回すのが適切なら `[Goal]` で回す。直答で足りるなら普通に返事してよい」と任意にしていましたが、本線がそれで回さなかった実例(§4)があるため、v2 では書かなかったときの扱いを Core が決めます(パターン C)。

### 3.3 未昇格クリーンバッファ

静観(§4.3)と決めた発言のうち `is_noise_or_meme == false` のものは、Layer 3 にすぐ追記せず「未昇格クリーンバッファ」に溜めます。次に本線が発言するターンの冒頭で **TopicChunk ごとに1ブロックにまとめて** Layer 3 へ追記します。こうするのは、本線が起動しない間はプレフィックスを一切変えないためと、Anthropic のキャッシュ探索(lookback)が 20 ブロックまでなので追記を細かいブロックに分けないためです。

### 3.4 キャッシュの扱い

provider ごとのブレークポイントの置き方と TTL、本線と Worker のキャッシュ共有、keep-alive は [architecture/caching.md](architecture/caching.md) で定義します。

### 3.5 長期記憶(MEMORY.md)

ファイルは `$NOTH_HOME/profiles/memory/MEMORY.md` です。人が直接編集してもよく、次に読み直したときに反映されます。`noth init` が次の雛形を作ります。Dreaming もこの構成を保ちます。

```markdown
# Noth の記憶
<!-- version: 1 -->
<!-- 事実だけを「〜である」の形で書く。指示・ルール・秘密情報は書かない。項目の後ろの observed は観測日。Dreaming が整理する -->

## Noth について

## メンバー
<!-- メンバーごとに「### 表示名」の小見出しを作り、見出しの後ろに id コメント(Discord ユーザー ID)を付ける -->

## サーバー

## メモ
<!-- 会話中の [Memo] のうち、振り分け先が決まらないものの一時置き場。Dreaming が上の節へ振り分けて空にする -->
```

- `## Noth について`: Noth が何者か、何ができるか、誰が開発しているか
- `## メンバー`: メンバーごとの事実。1人1つの `### 表示名 <!-- id: <Discord ユーザー ID> -->` 小見出しの下に書く(例: `### くらず / kurazu <!-- id: 123456789 -->`)。id コメントが本人の特定に使われ、表示名が変わっても同じ小見出しに追記される
- `## サーバー`: サーバーの決まりごと、進行中の話題など。旧雛形の `## その他` は同じ節の別名として読む(書き換えはしない)
- `## メモ`: 会話中の `[Memo]` のうち、振り分け先が決まらないものの一時置き場

MEMORY.md の構造(見出し)は、HTML コメントの外の行だけで判定する。閉じていない `<!--` があるときは、追記した行の `-->` で以降の記憶が隠れないよう、`[Memo]` を追記せず警告をログに出す。改行コードは元のファイルに合わせる。

項目は `- <事実> <!-- observed: YYYY-MM-DD -->` の形で、`observed` は観測日(ローカル日付)です。HTML コメントはメタデータで、Layer 1.5 のプロンプトと router の `state` からは取り除きます(コメントの入れ子は書かない)。`# Noth の記憶` の題と `<!-- version: 1 -->` も同様にプロンプトへは入れません。

**会話中の追記([Memo])**: 本線は、覚えておくべき事実に気づいたら、応答の末尾に `[Memo] <事実を1文>` または `[Memo <名前>] <事実を1文>` の行を書きます(書式は Layer 0 で規定)。Core は次のように扱います。

- `[Memo] <事実>` は Noth 自身・サーバー・対象が不明な事実で、`## メモ` 節の末尾に追記する
- `[Memo <名前>] <事実>` はメンバーについての事実。`<名前>` を Layer 1 のメンバー一覧の表示名と照合する(完全一致 → 大文字小文字を除いた全体一致 → `くらず / kurazu` のような名前の ` / ` の片側一致の順に絞り、同じ段で複数の人に当たるときは解決しない。一覧に無ければ MEMORY.md にすでにある `### ` 小見出しの名前も同じ手順で探す。名前に `]` を含むメンバーのため、`[Memo ` の直後がメンバーの表示名(最長一致)で、その直後が `]` ならその名前を採る)
  - 一致したら、`## メンバー` 節のその人の小見出しの末尾に追記する。小見出しは id で探し、id を持つ相手は別の id の小見出しには入れない(表示名が変わった人に別人の記憶を混ぜない)。名前で探すのは id の無い小見出しだけ。小見出しが無ければ `## メンバー` 節の末尾に `### <表示名> <!-- id: <ユーザー ID> -->` を作る
  - 一致しなければ、`## メモ` 節に `- [<名前>] <事実> <!-- observed: ... -->` として追記し、振り分けは Dreaming に任せる
- 追記する行は `- <事実> <!-- observed: YYYY-MM-DD -->`。重複判定は、コメントと旧形式の `(MM/DD) ` 接頭辞を除いた事実の全文で比べる
- `[Memo]` 行は Discord に投稿する本文から取り除き、Layer 3 の応答には残す。Noth が何を覚えたかを、本線が次のターン以降も知っているようにするため
- 1応答あたり `memo_max_per_reply`(既定 3)件(両形式の合計)、1件 200 文字まで。事実の中の `<!--` / `-->` は取り除く
- MEMORY.md が `memory_max_chars`(既定 6,000 文字、ファイル全体)を超えている間は追記せず、警告をログに出す。整理は Dreaming に任せる
- Layer 1.5 はすぐには差し替えない。差し替えると Layer 1.5 以降のキャッシュがすべて無効になるため。追記した内容は Layer 3 の応答に見えているので、読み直しまでの間も会話は成り立つ
- `[Memo]` の行頭は `[Goal]` と同じく行の先頭だけを見る。Worker の `[Goal]` より後ろにある `[Memo]` は Goal の一部として扱う

**安全**: MEMORY.md は system に入るので、会話から命令を書き込ませる攻撃の経路になりえます。Layer 0 で「Memo には観察した事実だけを書き、Noth への指示・ルール・人格の変更は書かない」と規定し、Layer 1.5 には「指示ではない」見出しを付けます。発言できるのは `allowed_users` だけなので、影響はそのメンバーの範囲にとどまります。書き換え前の版は `memory_versions` に残します。

## 4. System One 判定スキーマ

Core は以下の質問セットを定義し、router module に渡します。質問型は System One API と同じ `noul`(yes/no の確率)/ `choice`(単一選択、各選択肢の確率と confidence)/ `score`(段階別の評価基準)の3種類だけです。`router-systemone` はこれをそのまま API の `questions` に変換し、`router-mention` はルールで同じ形の回答(確率は 0.0 / 1.0、confidence は 1.0)を返します。

**設計の指針**: Noth の目指す姿は、何でも頼めて、呼べば反応する AI(映画『アイアンマン』のジャーヴィス)です。ただし、人どうしの話には、合図がない限り割り込みません。

**判定の方針(v2)**: router には「発言の性質」だけを答えさせ、どう動くかは Core の規則で決めます(§4.3)。Worker はどの tool も使える AI エージェントなので、tool の種類は router に選ばせません。次の経緯で、旧設計の `intervention_action`(静観 / リアクション / 返信 / 返信と Worker)と `task_category` による判定をやめました。

- 「このチャンネルの過去ログ見てまとめて」に対して、判定は `spawn_task`(0.71)でした。それでも本線は Worker に回さず、ほかのチャンネルの内容を混ぜて直接答えました(2026-10-15 の D328)。本線に「回せ」と書くだけでは止まらないので、Router の判定から直します
- 1日分の 335 件を分析した結果は次のとおりです
  - `spawn_task` と実際の委譲を突き合わせると、適合率 64%、再現率 47% でした
  - `task_category` の `code_or_tool` は、一度も実際と合いませんでした
  - 関連チャンクの問いは router の入力の約半分を占めていますが、役立ったのは 4% 未満でした
  - Jev の confidence は1位の確率の言い直しで、`min_confidence` は一度も効いていませんでした
  - 確率は 0.01 刻みなので同点が起き、同点は選択肢の並び順で決まっていました

### 二値化

二値化は Core が行います。`noul` は `[router.thresholds]` の閾値以上なら true とします。`choice` は最大確率の選択肢を採り、同点なら**先頭の選択肢**を採ります。そのため選択肢は安全な側(誤っても害の小さい側)を先頭に並べます。intervention の判定に `min_confidence` は使いません(合流判定の `abort` だけ、§4.5)。確率そのものも判定したモデル名と一緒に SQLite に保存し、閾値の調整や後の分析に使えるようにします。

閾値の値は、v2 の質問で評価を取り直してから決めます。旧設計の暫定値(`is_noise_or_meme` 0.5、`is_topic_shift` 0.6。2026-10-15 の実測 26 件)は、新しい質問の分だけ「評価で決める」としています。

### 4.1 `state` の組み立て

`state` は次の JSON オブジェクトで、全体を `state_max_chars`(既定 24,000 文字)に収めます。超える場合は `recent` の古い発言から除外します。ローカルではトークン数を計算しないため、Clef-flash(24,576 トークン)を使うときは文字数上限を余裕のある小さめの値に設定してください。

| フィールド | 内容 |
|---|---|
| `noth` | MEMORY.md の `## Noth について` 節。Noth が何者かを判定に教える。無ければ省く |
| `members` | `{list: [{name, roles, memory?}]}`。Layer 1 のメンバー一覧(表示名とロール)に、MEMORY.md の `## メンバー` 節のうちその人の `### ` 小見出しの本文(コメントを除く)を `memory` として添える。小見出しの id でユーザーを突き合わせ(同じ id の小見出しが複数あれば本文をつなげる)、本文が空なら `memory` を省く。小見出しに属さない `## メンバー` 直下の項目は、誰の事実か決められないので `state` には入れない(Layer 1.5 には入る) |
| `trigger` | デバウンス後の今回の発言群(チャンネル、投稿者の表示名、本文、時刻、`reply_to`、名前で呼ばれたかを示す `calls_noth_by_name`) |
| `recent` | 全チャンネルの直近 `recent_messages` 件(既定 30)。Noth 自身の発言も `is_self: true` で含める。各発言に `reply_to`。ほかの Bot の発言は本文の無い目印(§4.4)として同じ並びに入れる |
| `candidates` | 候補 TopicChunk の `{id, channel, summary}`。関連判定の対象。関連チャンクの問いを外す場合は含めない(§4.2) |

`reply_to` は Discord のリプライ先で、`{author, is_self, excerpt}`(`excerpt` は本文の先頭 60 文字)です。リプライでなければ省きます。`state_max_chars` を超えるときは `recent` の古い発言から削り、それでも超えるなら各メンバーの `memory`(長いものから)、`noth` の順に削ります。

### 4.2 流入・介入判定(発言受信時)

質問は1回のリクエストにまとめます。

| キー | 型 | 意味 |
|---|---|---|
| `is_noise_or_meme` | noul | スタンプ連打・コピペ連投など文脈価値のないノイズか |
| `is_topic_shift` | noul | そのチャンネルの直前の会話から話題が切り替わったか |
| `addressee` | choice: `other` / `about_noth` / `other_bot` / `noth_request` | 最後の発言は誰に向けたものか |
| `needs_outside_work` | noul | 応えるのに会話の外での作業が必要か |
| `corrects_noth` | noul | Noth が前に述べたことや調べた結果の誤りを指摘・反論しているか |
| `worth_supplement` | noul | 確かめたり補ったりすると会話の役に立つ点があるか |
| `reaction` | choice: `none` / `r0`〜(`[[router.reactions]]` の順) | 付ける絵文字。先頭の `none` は「付けない」で、`none` 以外が選ばれたときだけ付ける。選択肢の説明は「絵文字 + 意味」。`[[router.reactions]]` が空なら出さない |
| `relevant_chunk_<id>` | noul × 候補数 | 候補 TopicChunk `<id>` が今回の発言に関連するか。候補ごとに1問。減らすか外すかは評価で決める |

`task_category` はなくしました。`intervention_action` もなくなり、動き方は §4.3 の規則で決めます。

**問いの文言(初期案)**: 最終的な文言は評価で詰めます。

`addressee`: 最後の発言は誰に向けたものか。選択肢は同点のとき先頭が選ばれるため、安全な側を先頭に並べます。

| 選択肢 | 説明 |
|---|---|
| `other` | 人どうしの会話、全体への呼びかけ、独り言、人どうしの質問で、Noth への合図(下記)がなく、Noth の話でもない |
| `about_noth` | Noth の話をしているが、Noth に返事や作業を求めてはいない(Noth の挙動の感想、開発の相談、Noth の設計の話などで名前が話題として出ているだけのもの、ほかの人への説明) |
| `other_bot` | Noth 以外の Bot に話しかけている、または Bot の発言に返している |
| `noth_request` | Noth に返事、回答、作業を求めていて、Noth への合図がある(Noth に向けた質問や依頼) |

合図とは、名前による呼びかけ(文頭や文末の愛称や崩した表記も含む)、メンション、Noth の発言への返信、Noth とのやりとりの直後の続きのどれかです。合図のない独り言や人どうしの質問は `other` にします。答える価値があれば `worth_supplement` で拾います。

`needs_outside_work`(noul): 「この発言に応えるには、会話に出ている情報だけでは足りず、会話の外での作業が必要である。たとえば Web で調べる、チャンネルの過去ログを読む、ファイルやコマンドを扱う、長い文書を書く、モデルの知識では古いかもしれない最近の事柄(新しい製品、サービス、モデルの版、料金など)を確かめる」。一般的な知識は本線が答えるので、これには含めません。しきい値は 0.5 とし、取りこぼさない側に倒します(評価では、Worker に回すべき行を 12/12 拾いました)。

`corrects_noth`(noul): 「この発言は、Noth が前に述べたことや調べた結果の誤りを指摘している、または反論している」

`worth_supplement`(noul): 「この発言には、確かめたり補ったりすると会話の役に立つ点がある」。次のどちらかがあるときだけ拾います。(1) 数字、仕様、料金、日付、固有名詞についての具体的な主張。(2) 人どうしで出たまま答えのない、事実についての問い。リンクだけの発言、相づち、感想、冗談は除きます。補足の費用は Core 側の上限で抑えます(§4.3)。

- TopicChunk は **チャンネルごと** に管理する。`is_topic_shift == true` ならそのチャンネルで新しい TopicChunk の ID を割り当てる。チャンネルが違えば別のチャンクなので、2チャンネルの交互発言で毎回 topic shift になることはない
- `is_noise_or_meme == true` なら SQLite には記録するが、Layer 3 への昇格対象から外す。TopicChunk の要約にも足さず、topic shift でも新しいチャンクを作らない。ただし同時に本線が返事をする(`noth_request`)と決まった場合は、その発言をトリガーとして Layer 3 に入れる。応答した発言が Layer 3 に無いと会話が成り立たないため
- 関連チャンクの問いは router の入力の約半分を占めるのに、役立ったのは 4% 未満だった。減らす(`candidate_chunks` を小さくする)か外すかは評価で決める。外した場合、Layer 4 の「過去の関連チャンク」は空になり、`state.candidates` も含めない
- 候補 TopicChunk は直近のものから `candidate_chunks` 個。上限は `Router::max_questions() - 7`(他の質問が 7 問のため。Clef なら 57)。Layer 3 のターンが属しているチャンクと、未昇格クリーンバッファの発言が属しているチャンクは候補から外す。バッファはこのターンで Layer 3 に昇格するので、候補に入れると同じ本文が Layer 3 と Layer 4 に二重に入るため。候補の要約は TopicChunk 作成時に冒頭の数件の発言から機械的に作成する(LLM は使わない)
- System One API に複数選択型は無いため、関連チャンクは候補ごとの `noul` で表現する。`choice` の選択肢上限 255 は本設計では使わない

### 4.3 Core の決め方

router の回答から動き方を決めるのは Core です。次の順に見ます。

**決定的な `noth_request`**: Noth へのメンションと、Noth の発言へのリプライだけです。router の判定や失敗に関係なく `addressee == noth_request` として扱い、ノイズ扱いもしません。名指しされたのに無視すると、利用者には故障にしか見えないためです。名前による呼びかけは決定的に扱いません。「Noth の設計中に名前が出るたびに話しかけられると困るので、前の文脈から router が選ぶ」ためです(ユーザーの言葉)。本文に `[router] call_names` の呼び名(単語の一部としての一致は除く。呼び名の後ろに続くひらがなは単語の切れ目とみなす)が入っている発言は、`state.trigger` の `calls_noth_by_name` で router に伝え、`addressee` の問いで、呼びかけなら `noth_request`、名前が話題として出ているだけなら `about_noth` に分けます。`addressed_to_noth` の値で応答を決める旧設計の override(0.6 以上で静観を返答に引き上げる)と、呼び名での `name_call_reply` による応答はなくしました。

**続きの窓**: Noth がそのチャンネルで応答してから `conversation_window_secs`(既定 120 秒)の間は、そのとき話しかけてきた人の発言も `noth_request` として扱います。窓は外さずに残します。評価では、窓で返事をしていた 57 行のうち、`addressee` の問いで拾えたのは 39 行だったためです。この時刻と相手はメモリにだけ持ち、再起動で消えます。

| 条件 | 動き方 |
|---|---|
| `noth_request` で、`needs_outside_work` または `corrects_noth` が閾値以上 | **Worker に回す**(パターン C)。本線には `[判定]` で「Ack と `[Goal]` を書く」よう伝える。それでも本線が `[Goal]` を書かなかったら、本線の返事は投稿しない。Core がトリガーと発言先を依頼文にして Worker を起動し、Ack は 👀 のリアクションで代える。調べずに書いた返事が先に出るのを防ぐため |
| `noth_request` で、それ以外 | 本線が返事をする(パターン B)。本線が自分の判断で `[Goal]` を書いて Worker に回すことはできる。返事の要らない名指し(お礼など)は、本線が短く返すか、空の返事にして `reaction` の絵文字だけで済ませる |
| `about_noth` で、`corrects_noth` が閾値未満 | 静観する(パターン A)。`reaction` が `none` 以外ならリアクションだけ付ける(パターン A') |
| `about_noth` で、`corrects_noth` が閾値以上 | 下の「訂正」 |
| `other_bot` | 静観する。`corrects_noth` が高くても調べ直さない |
| `other` で、`worth_supplement` が閾値以上 | **黙って Worker を起動する**(パターン C')。下の「補足」 |
| `other` で、`corrects_noth` が閾値以上 | 下の「訂正」 |
| それ以外 | 静観する。リアクションは付けない(`other` の人どうしの会話や独り言には付けない) |

- **補足**: Ack は出さない。Worker は調べたうえで、「本当に指摘や補足が必要か」を結論に含める。必要なときだけ、短い補足を投稿する。投稿しなかったときも、結論はいつもの Worker の結果と同じく Layer 3 に入れる(調べた結果を捨てないため)。検索の費用がかかるので、`[router]` の設定で、チャンネルごとの最短間隔 `supplement_min_interval_secs`(既定 300 秒)と1日の上限 `supplement_max_per_day`(既定 50 回。MVP は1 guild なので guild 全体の回数)を持つ。上限に当たった発言は補足せず静観する。投稿は、トリガーの最後の発言への Discord のリプライとして出し、`allowed_mentions` は空にする(メンションの通知を飛ばさない)
- **訂正**: `corrects_noth` が高いときは、`addressee` が `other_bot` でない限り、Worker で調べ直す。`noth_request` ならパターン C で Ack を出して調べる。それ以外は補足と同じく黙って起動する。調べ直した結果を投稿するかは、補足と同じく Worker が決める。誤りを認める場合も、根拠を添えて元の結論を保つ場合もある。黙って調べ直す訂正の Worker には上限を設けず、補足の上限にも数えない。投稿は補足と同じ形(トリガーへのリプライ、`allowed_mentions` は空)
- `worker` module が無効な構成では、Worker を起動する行はすべて「本線が返事をする」(`noth_request`)か「静観する」(それ以外)に読み替える
- `router-mention` は `addressee` を決定的な規則だけで返し、`needs_outside_work` / `corrects_noth` / `worth_supplement` は常に 0.0 とする。そのため Worker は起動せず、本線が自分の判断で `[Goal]` を書いたときだけ回る

### 4.4 Bot の発言の目印

旧設計では Bot の発言を `channel-discord` が受け取った時点で落としていました。v2 では Noth 以外の Bot の発言を、**本文を入れずに目印だけ**残します。

```text
[Bot 秘書ちゃん が発言(くらず への返信、約1,200字。embed を含む) message_id: 123456789012345678]
```

- 対象は Bot アカウント(`author.bot`)の発言だけ。webhook 経由の発言は今までどおり落とし、目印も残さない。字数は `content` に embed のタイトル・説明・フィールドを足して数える
- Bot の本文は長いので入れない。目印があれば、router も本線も「ほかの Bot とのやりとりの最中だ」と分かる
- 目印は Layer 3 / Layer 4 と `RouterState` の `recent` に、ほかの発言と同じ並びで入れる。未昇格クリーンバッファにも他の発言と同じように溜め、判定(ノイズ判定を含む)は受けない。表示名は長さを制限し、改行を除く。リプライ先が `allowed_users` でなければ `<user>` にする
- **目印だけでは判定を起動しない。** Bot どうしの応酬を防ぐため。デバウンスのタイマーも進めず、アイドル(§5 パターン D)も解除しない。次に人間の発言で判定が走ったとき、その発言群と一緒に目印が `recent` と Layer 3 に現れる。再起動時の復元(§6.1)でも、目印だけの未判定を `Judge` にしない
- `tool-discord` が有効なときは、次の案内を入れる。案内は Layer 0 の「Worker に頼めること」と同じく、有効な tool から組み立てる
  - **本線(Layer 0)**: 本線は tool を持たないので、「Bot の発言の中身が必要なら、`[Goal]` を書いて Worker に読ませる」と伝える
  - **Worker の指示**: 「目印の message_id を指定すれば、discord tool で本文を読める」と伝える。目印のチャンネルはトリガーと同じとは限らないので、Layer 3 の `#名前` から `channel_id` を指定させる

### 4.5 Worker 合流判定(Worker 完了時)

`state` は Worker の結論メモ(または失敗理由)と、Fork 以降に届いた発言(`recent_messages` 件まで、Noth 自身の発言を含む)です。

| キー | 型 | 意味 |
|---|---|---|
| `delivery_style` | choice: `normal_report` / `brief_supplement` / `reply_anchor` / `abort` | 報告の仕方。人間側で自己解決済みなら `brief_supplement`、別の話題に移っていれば `reply_anchor`、報告の価値が無ければ `abort` |

判断材料(自己解決したか、話題が移ったか)は `instructions` の中で説明し、別の質問には分けません。この `choice` にだけ、`confidence` が `min_confidence` 未満なら `abort` とする安全側の扱いを残します。`min_confidence` が実際に効くか(Jev の confidence は1位の確率の言い直しだった)は評価で確かめ、効かなければ外します。

### 4.6 Jev と Clef の差異とモデル

Jev(TypeSafe)と Clef / Clef-flash(Cloudflare Workers AI)はどちらも「`state` + `questions` → 各質問の確率付き回答」という System One API の形で、質問型名 `noul` / `choice` / `score` とレスポンス構造(`answers.<key>`、`probabilities`、`confidence`)も一致します。Cloudflare は「エンドポイントとモデル名を変えるだけで切り替え可能」と説明しています。`router-systemone` は `preset` で URL の組み立てと body の `model` 値だけを切り替えます。

| | Jev | Clef | Clef-flash |
|---|---|---|---|
| エンドポイント | `POST https://api.typesafe.ai/v1/systemone` | `POST https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/run/@cf/cloudflare/clef` | 同左、末尾 `clef-flash` |
| 認証 | `Authorization: Bearer <TYPESAFE_API_KEY>` | `Authorization: Bearer <CF_API_TOKEN>` | 同左 |
| body の `model` | 版を固定した名前(下記) | `clef` | `clef-flash` |
| コンテキスト上限 | 64k tokens / リクエスト、`state` + 最長質問で 32k | 65,536 | 24,576 |
| 質問数 / リクエスト | 記載なし | 64 | 64 |
| 公称レイテンシ(中央値) | 524 ms(Cloudflare 計測) | 209 ms | 38.8 ms |
| 入力の料金(100万トークンあたり) | $0.042 | $0.240 | $0.038 |

料金は入力トークンだけにかかります。安いのは Clef-flash だけで、Clef は Jev より高くなります。

- **モデル名は版を固定する。** `jev-latest` や `jev-preview` は中身が変わる別名なので使いません。2026-10-15 の 237 件は `jev-preview`(1.14.0)で動いていました。固定する名前は TypeSafe のモデル一覧([TypeSafe docs: Models](https://docs.typesafe.ai/models))で確かめて決めます。`decisions.model` にも固定名を記録します
- **Clef-flash の無料枠**: 問いを減らせば、Cloudflare Workers AI の無料枠(1日 10,000 neurons)に Clef-flash が収まる見込みです。Jev と Clef-flash のどちらにするかは評価で決めます
- 参考: 旧設計の質問セットで同じ会話を比べると、Jev は Noth 宛てかどうかを 0.6〜0.9 の確率ではっきり分け、Noth の応答直後の返事も Noth 宛てと判定できた一方、Clef と Clef-flash は確率が 0.3〜0.6 に寄って区別がつきませんでした(2026-10-15 の実測)。`addressee` の問いで同じ差が出るかも評価で確かめます

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
    alt 静観(about_noth / other_bot / other)
        R-->>A: クリーン発言はバッファへ(リアクションだけ付ける場合あり)
    else noth_request(通常)
        A->>M: バッファ昇格、Layer 4 = 関連チャンク + トリガー
        M->>D: 応答
        A->>A: トリガー + 応答を Layer 3 へ昇格
    else noth_request + 外部作業 / 訂正
        A->>M: バッファ昇格、Layer 4 = 関連チャンク + トリガー + [判定]
        M->>D: Ack(Goal が無ければ返事を捨て、👀 で代える)
        A->>A: トリガー + Ack(+ Goal)を Layer 3 へ昇格
        A->>W: Layer 0〜3 + tools を引き継ぎ Fork(並行)
        W->>W: tool 実行(隔離コンテキスト)
        W->>A: Join(run_id)
        A->>R: 結論メモ + Fork 以降のログ
        R-->>A: delivery_style
        A->>M: 結論メモを Layer 3 に追記
        M->>D: 報告
    else other + 補足 / 訂正(黙って起動)
        A->>W: 依頼文 + Layer 0〜3 を引き継ぎ Fork(Ack なし)
        W->>A: Join(run_id)(補足の要否を含む)
        A->>A: 結論メモを Layer 3 に追記
        opt 補足が必要
            A->>M: 短い補足を生成
            M->>D: 補足
        end
    end
    Note over A: 人間の発言が N 分間なく、実行中の Worker もいない
    A->>C: Compact(ターン範囲)
    C-->>A: 要約を Layer 2 へ、範囲を置換
```

### パターン A: 静観(§4.3 で静観になる場合)

1. Core が静観と決める(`about_noth` / `other_bot` / `other` で、Worker を起動する条件に当たらないとき)。本線は起動しない
2. `is_noise_or_meme == false` の発言だけを未昇格クリーンバッファに入れる

### パターン A': リアクションだけ付ける(`about_noth` で `reaction` が `none` 以外のとき、または本線が空の返事をしたとき)

Noth の話をしているが返事を求められていない発言に、絵文字で反応します。付けるのは、`addressee` が `about_noth` のときと、本線が空の返事をしたときだけです。`other`(人どうしの会話や独り言)には付けません。

1. `about_noth` で静観と決まった発言群について、`reaction` の回答が `none` 以外のときだけ付ける(`none`、または回答が欠けるときは付けない)
2. トリガー発言群の最後の発言にリアクションを付ける。本線は起動しない。失敗(権限不足など)はログに残して無視する
3. 発言していないので Layer 3 には何も足さない。発言はパターン A と同じく未昇格クリーンバッファに入れる
4. メンションとリプライは `noth_request` になり、本線が返事をするので、このパターンには来ない。お礼のような返事のいらない名指しは、本線が空の返事を返したとき、`reaction` が `none` 以外ならその絵文字だけを付ける(投稿はしない)。Layer 3 には「[リアクション <絵文字>]」の assistant メッセージを残す

### パターン B: 通常の会話介入(`noth_request` で、Worker に回す条件に当たらないとき)

1. 未昇格クリーンバッファの発言を TopicChunk ごとに1ブロックにまとめて Layer 3 に追記する
2. router が true と判定した `relevant_chunk_<id>` のチャンク本文(関連チャンクの問いを外した場合は無し)と今回のトリガー発言を Layer 4 にセットする
3. 本線が推論し、Channel 経由で発言する。生成中は typing インジケータを出す。Channel は 2,000 文字を超える応答を分割投稿し、`allowed_mentions` を空にして `@everyone` やロールメンションを抑止する
4. provider のエラー(429 / 5xx・タイムアウト・止まったストリーム)は `retry_max` 回まで指数バックオフで再試行する(既定は `retry_max = 2` で、初回と合わせて最大 3 回)。それでも失敗したら typing を止め、トリガーのチャンネルに `⚠️ 応答を生成できませんでした(<モデル名> が応答しませんでした)。少し待ってからもう一度話しかけてください` と投稿(認証失敗は「の認証に失敗しました。管理者に確認してもらってください」、利用上限は「の利用上限に達しました」、それ以外は「でエラーが発生しました」)してターンを終了する。この通知は Layer 3 に入れず、トリガー発言は昇格せずバッファに戻す。同じチャンネルへの通知は本線と Join で別々に 60 秒に 1 回までで、router の失敗では通知しない
5. Layer 4 の過去チャンクを取り除き、トリガー発言と本線の応答を Layer 3 末尾へ確定追記する

### パターン C: 自律タスク起動(`noth_request` で `needs_outside_work` か `corrects_noth` が閾値以上)

`worker` module が無効な構成では、Core はこの条件を無視してパターン B を実行します。

1. **Ack と昇格**
   - パターン B の 1〜2 と同じ手順で Layer 3 / 4 を準備する
   - Layer 4 に `[判定]` を置き、「Ack と `[Goal]` を書く」よう伝える(§3.2)
   - 本線が「それ、ちょっと調べてみますね」などの宣言(Ack)を投稿し、同時に Worker への指示(Goal)を生成する。Ack と Goal の書式は Layer 0 で規定する。Goal は Discord には出さない
   - Ack はトリガー発言のチャンネルへ新規メッセージとして投稿する
   - Goal には指示語(「このチャンネル」「ここ」)ではなく、`[発言先]` の `#名前` と channel_id を書く(Layer 0 で規定)。Worker の最初の user メッセージにも「依頼された場所: #名前(channel_id: ID)」を入れ、`tool-discord` の既定もトリガーのチャンネルにするので、取り違えを二重に防ぐ
   - 本線の応答に `[Goal]` 行があるか tool を呼んでいれば、Worker に回す。これは `[判定]` が無い(パターン B の)ときも同じで、本線が自分の判断で回せる(そのまま投稿すると Goal が Discord に漏れるため)。`[Goal]` の前が空なら Ack は Layer 0 の定型文にする
   - **`[判定]` を出したのに本線が `[Goal]` を書かなかったとき**は、本線の返事を投稿しない(再生成もしない)。調べずに書いた返事が先に出るのを防ぐため。Core がトリガーと発言先(`#名前` と channel_id)を依頼文にして Worker を起動し、Ack はトリガー発言群の最後の発言への 👀 のリアクションで代える。Layer 3 にはトリガーと、Core が作った Goal だけのメッセージ(Ack の本文は空)を記録する。投稿しなかった本線の返事は Layer 3 に残さず、ログ(INFO)にだけ残す。Ack のメッセージが無いので、進捗の編集(下記)は行わず、入力中表示だけを出す
   - 本線が委譲しなかったときは、判定の確率と本線の応答の冒頭を INFO で1行記録する。本線の応答ごとに、渡した tool の数と応答の `tool_calls` の数も INFO に残す。Layer 0 に Worker でできることを書くのは、tool 定義だけでは本線がそれを「自分には使えないもの」と受け取り、委譲せずに「できない」と答えることがあるため
   - トリガー発言と本線の応答(Ack + Goal)を Layer 3 に昇格する。**Fork より前に昇格する**ことで、Worker の文脈にもトリガーと Goal が含まれる
   - 本線の応答がテキストの Ack ではなく `tool_calls` だった場合は、Core が次のように処理する
     - Layer 0 の定型文から Ack を作って投稿する
     - tool 呼び出しの内容から Goal を組み立て、「定型 Ack + Goal」をテキストの assistant メッセージとして Layer 3 に記録する
     - 元の `tool_calls` は履歴に残さない。残すと `tool_result` が必須になり、消すと assistant が空になるため
     - run には起動の経緯 `origin`(`request` / `supplement` / `correction`)のラベルを付ける。記録とプロンプト用で、tool の可否には使わない。有効な tool はどの run でも呼べる([security.md §3](architecture/security.md#3-web-側の対策と承認ポリシー))。tool の種類を router に選ばせる `task_category` はなくした
   - Worker の同時実行数が `max_concurrent` に達していたら run をキューに入れ、先に入ったものから順に実行する(FIFO)。Ack は投稿済みなので、待ち時間も `timeout_secs` に含める
2. **Fork**
   - 本線の Layer 0〜3 と tool 定義をそのまま引き継いで、バックグラウンドで Worker を起動する。本線と Worker は同一 provider・同一モデル・同一 tool 定義・同一 `tool_choice`(未指定)なので、Worker の初回呼び出しは本線の KV キャッシュに当たる([architecture/caching.md §2](architecture/caching.md#2-本線と-worker-のキャッシュ共有))
   - Worker 固有の指示(「これはバックグラウンド作業である」「結論メモの書式」「Goal」)は **最初の user メッセージ** に置く。`system` には1文字も足さない。理由は [architecture/caching.md §3](architecture/caching.md#3-worker-固有の指示の置き場) を参照
   - Worker は隔離された自分のコンテキストで tool を実行し、呼び出しと結果をコンテキストの末尾に追記する。反復は `max_iterations` 回まで、各 tool の出力は `tool_output_max_chars` で切り詰める。有効な tool(`tool-shell` と `discord` を含む)はどの run でも承認なしで呼ばれ、`tool-shell` の `cwd` は run ごとの `$NOTH_HOME/work/<run_id>/`
   - Worker の実行中も本線は別の会話を処理できるため、雑談が進めばパターン B で通常どおり応答できる
   - Worker の実行中(キュー待ちを含む)はトリガーのチャンネルに入力中表示を出し続ける。`[worker] show_progress`(既定 true)なら、Worker が tool を呼ぶたびに Ack のメッセージを編集し、`-# 🔍 web_search「検索語」` や `-# 💬 discord search_messages「検索語」` のような行を末尾に足していく。編集は 1.5 秒に 1 回までにまとめ、終了時に完了か失敗の行を足す。この進捗は Discord にだけ出し、Layer 3 には Ack と Goal の本文だけを残す。tool の出力は進捗にも含めない
3. **Join と報告**
   - Worker は完了時に途中の tool 出力をすべて捨て、構造化された結論メモ(数百トークン)だけを出力して終了する。timeout・API エラー・反復上限で失敗したときは Core が `[内部調査失敗 <run_id>: <理由>]` を結論メモの代わりにする
   - actor が `Join(run_id)` を処理し、router の合流判定(§4.5)を実行する
   - `abort` なら結論メモは SQLite に残すだけで Layer 3 には入れず、Ack はそのままにする。router のエラーと `min_confidence` 未満も `abort` として扱い、判定は `decisions` に kind `join` / `join_error` で記録する
   - 報告の生成に失敗したときは、チャンネルに `⚠️ 調査結果の報告を生成できませんでした(…)` と投稿する(60 秒に 1 回まで)。報告の生成・投稿に失敗したときは、結論メモのブロックだけを Layer 3 に残す。Ack で待たせているので、次に本線が話すときに調査結果を踏まえられるようにするため
   - `abort` 以外なら、次の順に報告する
     1. 未昇格バッファを昇格する
     2. Layer 3 に `[内部調査完了 <run_id>: <結論メモ>]` を user メッセージとして1ブロック追記する
     3. Layer 4 には `delivery_style` の指示だけを入れ、本線が発言する。`reply_anchor` のときだけ `worker_runs.trigger_message_id` への Discord リプライとして投稿する
     4. 応答を Layer 3 に昇格する。Layer 4 の指示は残さない

### パターン C': 黙って調べる(`other` の補足、または `noth_request` 以外の訂正)

`worker` module が無効な構成では起動しません(静観)。

1. 補足なら、Core が `supplement_min_interval_secs` と `supplement_max_per_day` を確認する。当たれば静観にする。訂正は確認しない(上限なし)
2. Ack は出さず、`[Goal]` も本線に書かせない。Core がトリガーと発言先を依頼文にし、補足なら「本当に指摘や補足が必要かを調べて判断する」、訂正なら「Noth が前に述べたことが誤りでないか調べ直す」と書く。トリガー発言群は未昇格バッファのままで、Layer 3 には入れない。Worker の最初の user メッセージにトリガー発言群と依頼された場所を入れる
3. Fork して Worker を実行する(パターン C の 2 と同じ。キューと `timeout_secs` も同じ)。入力中表示と進捗の編集は出さない。投稿するか分からないうちに、調べていることを見せないため
4. Worker の結論メモに、「補足(または訂正)が必要か」と、必要なら投稿する短い本文の案を含める。Layer 0 で結論メモの書式として規定する
5. 完了時の Join は次のように分かれる
   - **不要**: 未昇格バッファを昇格し、`[内部調査完了 <run_id>: <結論メモ>]` を Layer 3 に追記するだけ。router の合流判定も本線も呼ばず、何も投稿しない。調べた結果を捨てないため
   - **必要**: パターン C の 3 と同じ。結論メモを Layer 3 に追記し、合流判定(`abort` もありうる)を経て、本線が短い補足を、トリガーの最後の発言へのリプライとして投稿する(`allowed_mentions` は空)。誤りを認める場合も、根拠を添えて元の結論を保つ場合もある
   - **失敗**: 結論メモの代わりの `[内部調査失敗 <run_id>: <理由>]` は SQLite に残すだけで、Layer 3 にも Discord にも出さない。利用者は調べていることを知らないため

### パターン D: アイドル時コンパクション

`compactor` module が無効な構成では、Layer 3 がハード上限を超えた時点で古い TopicChunk を要約せずに破棄します。

**通常トリガー**: 人間の最終発言から N 分(既定 10 分)経過。Noth 自身の投稿はアイドルを解除しない。Worker が実行中なら完了を待ってから始める。

1. actor が `Compact(ターン ID 範囲)` を処理する。対象は Layer 3 のうち「残す直近ターン」(`keep_recent_turns`、既定 6)を除いた範囲。要約生成中に届いた発言はキューで待つ
2. 対象範囲を三人称の客観的なエピソード要約に変換し、Layer 2 に統合する。各エピソードの先頭には `[エピソード MM/DD HH:MM]` の見出しを付け、上限を超えたときに古いものから削る単位にする。残す側の先頭が assistant のターンになる場合は、切る位置を後ろへずらす。要約の生成には Layer 1(Profile)を入れず、Worker の節(Ack と Goal、Worker に頼めること)を除いた Layer 0 の規約だけを与える。キャラクターの口調が要約に混ざり、本線の文体を乱すのを防ぐため
3. Layer 3 から対象範囲を取り除く。新しい Layer 2 を SQLite に保存する。Layer 2 が `digest_max_chars` を超えたら古いエピソードから削除する

**緊急トリガー**: アイドルにならないまま直近のレスポンスの `usage.input_tokens`(tools + Layer 0〜4 の合計)が `[context] prompt_hard_limit_tokens` を超えた場合。

1. 古い TopicChunk から順に、そのチャンクに属する Layer 3 のターンを要約して Layer 2 へ退避する。チャンク境界(`is_topic_shift` で切った位置)で切るため話題の分断は起きない
2. このときのキャッシュ無効化は許容する。キャッシュの維持より、上限超過の回避を優先するため

### パターン E: Dreaming(1日1回)

人が寝るのと同じく、1日の記録を振り返って記憶を整理します。`compactor` module と `[memory] dreaming = true` が必要です。

**トリガー**: ローカル時刻で `dreaming_hour`(既定 4 時)を過ぎ、まだその日の Dreaming をしていなくて、パターン D と同じアイドル条件(人間の最終発言から N 分、Worker が実行中でない)を満たしたとき。対象は「前日の `dreaming_hour` から当日の `dreaming_hour` まで」の1日で、日付は開始側で呼ぶ(10/15 04:00 に始まる1日は 10/15 分)。止まっていた日はさかのぼらず、直近の1日だけを処理する。

1. actor が `Dream(対象日)` を処理する。処理中に届いた発言はキューで待つ
2. **日次レポート**: 対象日の `messages`(ノイズを除く。返信関係とチャンネルを含む)を時系列の行にして、Compactor に日次レポートを書かせる。Layer 1 は入れず、Worker の節を除いた Layer 0 の規約だけを与える(要約と同じ理由)。入力が `dream_input_max_chars`(既定 60,000 文字)を超えるときは古い発言から削る。`$NOTH_HOME/profiles/memory/daily/YYYY-MM-DD.md` に保存する
3. **記憶の整理**: ディスクから読み直した MEMORY.md と日次レポートを渡し、LLM に MEMORY.md 全体ではなく**項目ごとの操作**を返させる。操作は `add`(新しい事実を節または人の小見出しへ追加)、`merge`(重複する項目をまとめる)、`replace`(項目を言い直す)、`supersede`(古くなった項目を新しい事実で置き換える)、`move`(`## メモ` の項目を上の節へ振り分ける)、`drop`(項目を削除する)。`## メモ` の `[名前]` 接頭辞つきの項目は、`[Memo <名前>]` と同じ手順で名前を解決し、一致するメンバーの小見出しへ `move` する(解決できないものは `## メモ` に残す)。操作は項目への参照(節・小見出しと項目の本文)を持ち、コードが検証して適用する。ファイル全体を書き直させないのは、LLM が黙って項目を落としたり文面を変えたりするのを防ぐため(OpenClaw の dreaming の設計に倣う)。メモリ上の内容ではなくディスクから読み直すのは、実行中に人が手で直した内容を消さないため
4. 適用後の MEMORY.md が次の検証をすべて満たすときだけ採用する: (a) 後継のないまま消えた項目が、`## メモ` 以外の前の項目数の 25% 以下(`drop`、または後継のない `supersede` を数える。新しい項目に置き換える `supersede` と、`## メモ` から出す `move` は失ったことにしない)、(b) Dreaming が追加・変更した項目が `observed` の日付を持つ(手で書かれた項目や旧形式の項目に `observed` が無いのは許し、そのまま残す)、(c) 全体が `memory_max_chars` 以下。満たさない(または操作が不正で適用できない)ときはファイルを変えず、日次レポートだけを残して終わる。`supersede` で置き換えられた古い事実は消す(書き換え前の版が `memory_versions` に残るため)。採用するときは、書き換え前の版を `memory_versions` に保存してから、一時ファイル経由で置き換える
5. Layer 1.5 を読み直す。アイドル中なのでキャッシュは切れており、追加の費用はほぼ無い
6. 結果を `dreams` に記録する。LLM 呼び出しは2回で、`noth cost` には用途 `dreaming` として出る

日次レポートは今のところプロンプトに入れません。Layer 2 のエピソードが直近の数日分を持っているためです。

## 6. SQLite に持つデータ

ファイルは `$NOTH_HOME/data/noth.sqlite` の1つで、すべてのテーブルが `guild_id` を持ちます。

| テーブル | 内容 |
|---|---|
| `messages` | `allowed_users` と Noth 自身の発言の生ログ、Bot の目印。チャンネル、投稿者、`is_self`、本文、時刻、所属 TopicChunk、`is_noise_or_meme`、自分宛てのメンション・リプライか、リプライ先の発言 ID・投稿者名・Noth の発言か、`decision_id`(NULL なら未判定)、`promoted_turn_id`(NULL なら未昇格) |
| `topic_chunks` | TopicChunk の ID、チャンネル、開始・終了時刻、候補用の機械要約、Compactor による要約が済んでいるか |
| `decisions` | router の判定結果と各回答の確率・confidence、Core が決めた動き方、レイテンシ、判定したモデル(版を固定した名前。`jev/<固定名>` など) |
| `active_log` | Layer 3 のターン列。各ターンの種別(バッファ / トリガー / 応答 / 結論メモ)、`chunk_id`、その時点の `usage` |
| `digests` | Layer 2 の履歴 |
| `worker_runs` | `run_id`、Goal、`origin`(request / supplement / correction)、`trigger_message_id`、`channel_id`、状態(queued / running / finished / failed / aborted)、開始・終了時刻、結論メモ、`delivery_style` |
| `heartbeat` | 常駐プロセスの最終更新時刻と接続状態。`noth status` がプロセス外から読む |
| `memory_versions` | MEMORY.md を書き換える前の版。書き換えた理由(`memo` / `dreaming`)と時刻 |
| `dreams` | Dreaming の対象日、状態(finished / failed / skipped)、日次レポートのパス、開始・終了時刻、失敗理由 |

`allowed_users` 以外のユーザーの発言は Channel で捨てるため、どのテーブルにも入りません。Noth 以外の Bot の発言は本文を保存せず、`messages` に目印だけを `is_bot_marker` 付きで入れます(§4.4)。`search_messages` の対象にはしません。

### 6.1 再起動時の復元

1. `worker_runs` で queued / running 状態の run を aborted にする。Ack は投稿済みだが報告はしない
2. `active_log` から Layer 3 を復元し、`digests` の最新から Layer 2 を復元する
3. `messages` のうち `promoted_turn_id` が NULL で `decision_id` があるものを未昇格バッファに戻す
4. `decision_id` が NULL の発言(判定前に落ちたもの)はまとめて1つの `Judge` としてキューに積む。Bot の目印だけが残っているときは積まない

## 7. 実装で直すこと

v2 の設計を、2026-10-16 時点のコードが満たしていない箇所です。コードは先に確かめた結果で、この節の項目は実装で直します。

1. **Bot の目印(`channel-discord`)**: 受信ハンドラが Noth 以外の Bot の発言を、目印も残さず落としている(`crates/noth-channel-discord/src/handler.rs:50` の `msg.author.bot || msg.webhook_id.is_some() || ...` で return)。`author.bot` の Noth 以外は、本文を読まずに目印(表示名、リプライ先、embed を含む文字数、message_id)だけを `IncomingMessage` に流す。webhook は落としたままにする。判定は起動しない(§4.4)
2. **`tool-discord` の `fetch_messages`**: message_id を指定した読み出しは `before` / `after` / `around` で可能だが(`crates/noth-tool-discord/src/lib.rs:318-326`、`manifest.rs:101-103`)、Noth 以外の Bot の発言は本文を返さず `{id, author: "<user>", at}` に伏せている(`format.rs:99-107`。`tests.rs:335` の `bot says hi` が伏せられることをテストで固定している)。読めるのは `allowed_users` と Noth 自身(`viewer.self_id`)だけ。Bot の発言は、`content` に加えて embed のタイトル・説明・フィールドもテキストにして返すように変え、外部の指示が紛れうるものとして `bot: true` を付ける([security.md §2](architecture/security.md#2-discord-側の対策))。ID 1 件だけを取る専用の引数は無いので、`around` に `limit: 1` を組み合わせて使えるか(`around` が対象を含むか)を実機で確かめ、使えなければ専用の引数を足す
3. **`tool-discord` の `search_messages`**: Noth の SQLite の `messages` だけを引く(`crates/noth-core/src/history.rs` の `HistoryQuery` に message_id は無い)。`channel-discord` が Bot を落とすので Bot の発言は含まれず、目印を保存しても対象外とする(`is_bot_marker` を除く)。message_id で引く手段は無いので、Bot の本文は `fetch_messages` で読む
4. **`RouterState` と質問セット(`noth-core`)**: `reaction` の先頭に `none` を足す。リアクションを付ける条件を `about_noth` と空の返事だけにする。呼び名だけの発言を決定的に扱っている処理をやめ、メンションとリプライだけにする。`intervention_action` / `task_category` / `addressed_to_noth` を `addressee` / `needs_outside_work` / `corrects_noth` / `worth_supplement` に置き換える。`ToolContext.task_category` と `worker_runs.task_category` を `origin` に置き換える。`[router.thresholds]` の `addressed_to_noth` と `name_call_reply` をなくす
5. **`[判定]` と、`[Goal]` が無いときの Core による起動**(パターン C)、**黙って調べる Worker**(パターン C')、`supplement_min_interval_secs`(既定 300)/ `supplement_max_per_day`(既定 50。訂正は数えない)、補足・訂正の投稿をトリガーへのリプライ(`allowed_mentions` は空)にすること、空の返事+リアクションだけの返事、投稿しなかった返事をログにだけ残すこと、結論メモの「補足の要否」欄
6. **コメントの誤り(`crates/noth-core/src/config.rs:711`)**: 雛形のコメント「Clef(Cloudflare Workers AI)は安いが、…」は誤り。入力の料金は Jev $0.042、Clef $0.240、Clef-flash $0.038(100万トークンあたり)で、安いのは Clef-flash だけ(§4.6)。「Clef は安い」という趣旨の記述を、Clef-flash だけに直す
7. **モデル名**: 既定の `jev-latest`(`crates/noth-router-systemone/src/preset.rs:59`)を、版を固定した名前にする(§4.6)
