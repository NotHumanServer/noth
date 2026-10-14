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
