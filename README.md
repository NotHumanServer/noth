# noth

## セットアップ

開発環境は [devenv](https://devenv.sh/) で管理しています。Rust のバージョンは `rust-toolchain.toml` で固定されます。

1. Nix と devenv をインストールする([Getting started](https://devenv.sh/getting-started/))
2. 開発シェルに入る

   ```sh
   devenv shell
   ```

   [direnv](https://direnv.net/) を使う場合は、初回に `direnv allow` すればディレクトリに入るだけで有効になります。

開発シェルに入ると、Rust ツールチェーン(cargo / clippy / rustfmt / rust-analyzer)と、コミット時に走る git hooks(rustfmt / clippy / nixfmt)が自動でセットアップされます。

## ビルド

開発シェル内で実行します。

```sh
cargo build            # デバッグビルド
cargo build --release  # リリースビルド
cargo run              # ビルドして実行
```

## インストール

release ビルドから `~/.local/bin/noth` への配置、常駐(launchd / systemd)の入れ替えまでを、対話で行います。

前提ツール: [git](https://git-scm.com/) と、[devenv](https://devenv.sh/getting-started/)(推奨。gum / jq / cargo / bash 4.4 以降を含む開発シェルを自動で用意します)。devenv を使わない場合は gum / jq / cargo と bash 4.4 以降を自分で入れてください。

URL から直接実行できます。ソースを clone してから、続けて本体を実行します。

```sh
bash <(curl -fsSL https://raw.githubusercontent.com/NotHumanServer/noth/main/scripts/install.sh)
```

`curl -fsSL … | bash` でも実行できます(対話は `/dev/tty` から取り、制御端末が無ければ非対話として扱います)。clone 済みなら、リポジトリ内で次を実行します。

```sh
scripts/install.sh
```

URL 実行ではまず bootstrap が動きます(macOS 標準の bash 3.2 でも動きます)。clone 先(既定 `${XDG_DATA_HOME:-~/.local/share}/noth/src`)が既にあって origin が同じリポジトリなら `git fetch` し、`--ref` の指す先へ fast-forward できる場合だけ、確認のうえ更新します(未コミットの変更があれば更新しません)。`--ref` に fast-forward できないときは、対話時は確認のうえチェックアウトし、`--yes` や非対話では中断します。別のディレクトリなら中断します。その後は clone 先の `scripts/install.sh` に引き継ぎます。

gum / jq / cargo が無い、または bash が古い(4.4 未満)ときは、`devenv` があれば自動で `devenv shell` に入り直します。リポジトリ外の cwd からも実行できます。

任意 feature の選択(`config.toml` で有効な module は選択済み)、ビルド、`--version` と `config check` による検証、`noth.prev` への退避つきの配置、常駐の再起動・登録を順に進めます。常駐の定義が登録済みなら、その `-c` / `NOTH_HOME` の設定を基準にし、登録し直すときも引き継ぎます(PATH は既存の定義の値を引き継ぎ、新規登録ではログインシェルの PATH を使います。`.zshrc` だけで足している PATH は入りません)。常駐の操作と、launchd / systemd 管理外の `noth run` の停止は、`--service` / `--stop-foreground` を指定しないかぎり、そのつど確認します。

主なフラグ(TTY が無い場合は `--features` と `--service` が必須です)。

| フラグ | 意味 |
| --- | --- |
| `--features a,b` | 組み込む任意 feature(`provider-openai` など) |
| `--service restart\|install\|skip` | `restart`: 登録済みなら再起動(配置先と別のバイナリを指していれば登録し直す。未登録なら何もしない)。`install`: 未登録なら登録して起動、登録済みなら `restart` と同じ(同じパスなら再起動、別パスなら登録し直す)。`skip`: 常駐に触らない |
| `--stop-foreground` | 管理外の `noth run` を SIGTERM で止めて終了を待つ。常駐を起動・再起動するときだけ意味を持ち、`--service skip` では止めない |
| `--src DIR` | (URL 実行時)clone 先。既定 `${XDG_DATA_HOME:-~/.local/share}/noth/src`。環境変数 `NOTH_SRC_DIR` でも指定可 |
| `--ref REF` | (URL 実行時)使うブランチ・タグ・コミット(既定 `main`) |
| `--prefix DIR` | 配置先(既定 `~/.local/bin`) |
| `--yes` | 「作業ツリーが dirty」「config が要求する feature を外した」の確認を自動で続行する。URL 実行では clone 先の確認・clone・fast-forward 更新も自動で承認する(`--ref` に届かなければ中断)。常駐やプロセスの停止は自動では行わない |
| `--dry-run` | 何も実行せず、実行予定のコマンドを表示する |

ログは `$NOTH_HOME/logs/install-*.log` に残ります。切り戻しは `mv -f ~/.local/bin/noth.prev ~/.local/bin/noth && noth service restart` です。

## ドキュメント

設計書は [docs/index.md](docs/index.md) から辿れます。
