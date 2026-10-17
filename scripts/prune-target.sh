#!/usr/bin/env bash
# target/ を NOTH_TARGET_LIMIT_GB(既定 16)以下に保つ。消すのは、もう使われていない成果物だけ。
# cargo は feature や profile の組み合わせごとに別ハッシュで成果物を作り、古いものを消さない。
# cargo-sweep はアクセス時刻で判定するが、macOS ではビルドしても atime が更新されず使用中の成果物まで消すので使わない。
#
# 「使われている」は、普段使う cargo コマンド(下の keep_cmds)を --message-format=json で流し、
# cargo が報告する成果物(fresh なものも報告される)のハッシュで決める。どのコマンドにも出てこない
# ハッシュの deps / .fingerprint / build だけを消す。cargo clean はしない。
# 普段と違うコマンドの成果物は消えるが、次に使うときに作り直されるだけで壊れはしない。
# 足したいコマンドは NOTH_PRUNE_KEEP に「;」区切りで書く(例: "test -p noth-core --features x")
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

keep_cmds=(
  "build"
  "build -p noth-fakecord -p noth --features provider-openai" # scripts/fakecord-up.sh
  "clippy"                                                     # pre-commit
  "clippy --workspace --all-targets"
  "test --workspace --no-run"
)
if [[ -n "${NOTH_PRUNE_KEEP:-}" ]]; then
  IFS=';' read -r -a extra <<<"$NOTH_PRUNE_KEEP"
  keep_cmds+=("${extra[@]}")
fi

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

# keep_cmds の成果物のハッシュを 1 行ずつ出す。どれかが失敗したら(コンパイルエラーなど)、
# 使われているものを数え漏らすので何も出さずに失敗する
live_hashes() {
  local cmd out
  out=$(mktemp)
  trap 'rm -f "$out"' RETURN
  for cmd in "${keep_cmds[@]}"; do
    # shellcheck disable=SC2086 # cmd は意図して単語に分ける
    cargo $cmd --manifest-path "$root/Cargo.toml" --message-format=json >>"$out" 2>/dev/null || return 1
  done
  python3 -c '
import json, re, sys
h = re.compile(r"-([0-9a-f]{16})(?=[./]|$)")
live = set()
for line in open(sys.argv[1]):
    try:
        m = json.loads(line)
    except ValueError:
        continue
    for p in list(m.get("filenames") or []) + [m.get("executable") or "", m.get("out_dir") or ""]:
        live.update(h.findall(p))
print("\n".join(sorted(live)))
' "$out"
}

# deps のファイル、.fingerprint と build のディレクトリのうち、ハッシュが live に無いものを消す
prune_unused() {
  local profile="$1" live="$2"
  python3 - "$profile" "$live" <<'PY'
import os, re, shutil, sys
profile, live_file = sys.argv[1], sys.argv[2]
live = set(open(live_file).read().split())
h = re.compile(r"-([0-9a-f]{16})(?=[./]|$)")
removed = 0
for sub in ("deps", ".fingerprint", "build"):
    d = os.path.join(profile, sub)
    if not os.path.isdir(d):
        continue
    for name in os.listdir(d):
        m = h.search(name)
        if not m or m.group(1) in live:
            continue
        p = os.path.join(d, name)
        removed += 1
        if os.path.isdir(p) and not os.path.islink(p):
            shutil.rmtree(p, ignore_errors=True)
        else:
            os.remove(p)
print(f"prune-target: 使われていない成果物を {removed} 個消しました", file=sys.stderr)
PY
}

for profile in "$target"/debug "$target"/release; do
  [[ -d "$profile" ]] && prune_incremental "$profile"
done

(( $(size_kb) > limit_kb )) || exit 0

live=$(mktemp)
trap 'rm -f "$live"' EXIT
if ! live_hashes >"$live" || [[ ! -s "$live" ]]; then
  echo "prune-target: 使われている成果物を割り出せなかったので、何も消しません" >&2
  exit 0
fi
# release は keep_cmds に無いので、debug だけを対象にする
prune_unused "$target/debug" "$live"

if (( $(size_kb) > limit_kb )); then
  echo "prune-target: 使われている成果物だけで target/ が ${NOTH_TARGET_LIMIT_GB:-16}GB を超えています。NOTH_TARGET_LIMIT_GB を上げてください" >&2
fi
