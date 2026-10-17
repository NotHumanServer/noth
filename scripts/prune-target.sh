#!/usr/bin/env bash
# target/ を NOTH_TARGET_LIMIT_GB(既定 16)以下に保つ。
# cargo は feature や profile の組み合わせごとに別ハッシュで成果物を作り、古いものを消さない。
# cargo-sweep はアクセス時刻で判定するが、macOS ではビルドしても atime が更新されず使用中の成果物まで消すので使わない。
#
# --hook: Claude Code の PostToolUse から呼ぶ。stdin のツール入力に cargo が含まれるときだけ動く
set -euo pipefail

if [[ "${1:-}" == "--hook" ]]; then
  grep -q 'cargo' || exit 0
fi

root="${DEVENV_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
target="$root/target"
limit_kb=$(( ${NOTH_TARGET_LIMIT_GB:-16} * 1024 * 1024 ))
keep=3

[[ -d "$target/debug" ]] || exit 0

# ビルド中に消すと壊れるので、cargo がロックを持っている間は何もしない
lock_free() {
  perl -e 'use Fcntl ":flock"; open(my $f, "<", $ARGV[0]) or exit 0; flock($f, LOCK_EX | LOCK_NB) or exit 1' "$1"
}
for profile in "$target"/*/; do
  [[ -f "$profile/.cargo-lock" ]] && ! lock_free "$profile/.cargo-lock" && exit 0
done

size_kb() { du -sk "$target" | cut -f1; }

# incremental はクレートごとに設定違いのセッションが溜まる。新しい順に keep 個だけ残す
prune_incremental() {
  local dir="$1/incremental"
  [[ -d "$dir" ]] || return 0
  ls -t "$dir" | sed -E 's/-[^-]+$//' | sort -u | while read -r crate; do
    ls -dt "$dir/$crate"-* 2>/dev/null | tail -n +$((keep + 1)) | xargs rm -rf
  done
}

# deps の実行ファイル(bin とテスト)は1つ数十〜百 MB ある。名前ごとに新しい keep 個だけ残し、
# 対応する .o / .d と fingerprint も消して cargo に作り直させる
prune_executables() {
  local dir="$1"
  find "$dir/deps" -maxdepth 1 -type f -perm -u+x -name '*-????????????????' -print0 2>/dev/null |
    xargs -0 ls -t 2>/dev/null |
    while read -r exe; do basename "$exe"; done |
    awk -F- -v keep="$keep" '{ name = $0; sub(/-[0-9a-f]{16}$/, "", name); if (++seen[name] > keep) print }' |
    while read -r stale; do
      rm -rf "$dir/deps/$stale" "$dir/deps/$stale".* "$dir/.fingerprint/$stale"
    done
}

for profile in "$target"/debug "$target"/release; do
  [[ -d "$profile" ]] || continue
  prune_incremental "$profile"
  prune_executables "$profile"
done

if (( $(size_kb) > limit_kb )); then
  rm -rf "$target"/*/incremental
fi
if (( $(size_kb) > limit_kb )); then
  cargo clean --manifest-path "$root/Cargo.toml" >/dev/null 2>&1
fi
