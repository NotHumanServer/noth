#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
使い方: scripts/install.sh [オプション]

release ビルド → 検証 → ~/.local/bin/noth への配置 → 常駐の入れ替え を行います。

URL から直接実行できます(リポジトリ外から実行すると、ソースを clone してから本体に進みます):
  bash <(curl -fsSL https://raw.githubusercontent.com/NotHumanServer/noth/main/scripts/install.sh)
  curl -fsSL https://raw.githubusercontent.com/NotHumanServer/noth/main/scripts/install.sh | bash
TTY で実行すると gum で対話します。TTY が無い場合は --features と --service が必須です。
gum / jq / cargo が無い、または bash が古いときは、devenv があれば devenv shell に入り直します。
devenv の導入: https://devenv.sh/getting-started/

オプション:
  --features a,b          組み込む任意 feature(カンマ区切り。空文字なら任意 feature なし)
  --service MODE          常駐の扱い。restart | install | skip
                            restart: 登録済みなら再起動(バイナリのパスが違えば登録し直す)。未登録なら何もしない
                            install: 新バイナリで登録して起動(登録済みで同じパスなら再起動)
                            skip:    常駐に触らない
  --stop-foreground       launchd/systemd 管理外の `noth run` を SIGTERM で止める。
                          常駐を起動・再起動するときだけ意味を持つ(--service skip では止めない)
  --prefix DIR            配置先ディレクトリ(既定 ~/.local/bin。環境変数 NOTH_INSTALL_PREFIX でも指定可)
  --yes                   「作業ツリーが dirty」「config が要求する feature を外した」の確認を自動で続行する。
                          URL 実行では clone 先の確認・clone・fast-forward 更新も自動で承認する(--ref に届かなければ中断)。
                          常駐の停止・再登録・プロセス停止は --service / --stop-foreground を明示したときだけ行う
  --src DIR               (URL 実行時)clone 先。既定 ${XDG_DATA_HOME:-~/.local/share}/noth/src(環境変数 NOTH_SRC_DIR でも指定可)
  --ref REF               (URL 実行時)使うブランチ・タグ・コミット(既定 main)
  --dry-run               ビルド・配置・常駐操作をせず、実行予定のコマンドを表示する
  -h, --help              このヘルプを表示する

環境変数:
  NOTH_HOME               設定とログの場所(既定 ~/.noth。登録済みの常駐があればその設定を優先する)
  NOTH_INSTALL_PREFIX     --prefix の既定値
  NOTH_SRC_DIR            --src の既定値
  NOTH_REPO_URL           clone 元(テスト用。既定 https://github.com/NotHumanServer/noth.git)
  NOTH_INSTALL_PLIST      常駐の定義ファイルの場所を差し替える(テスト用)
EOF
}

# ここから bootstrap までは bash 3.2(macOS 標準)でも動く必要がある。連想配列・mapfile・空配列の展開は使わない

bs_die() {
    echo "エラー: $*" >&2
    exit 1
}

# curl | bash では stdin がスクリプト本体なので、対話は /dev/tty から取る。制御端末が無ければ非対話
bs_have_tty() {
    ( : </dev/tty ) 2>/dev/null
}

bs_confirm() {
    local ans
    printf '%s [y/N]: ' "$1" >&2
    read -r ans </dev/tty || return 1
    case "${ans}" in
        y|Y|yes|YES) return 0 ;;
        *) return 1 ;;
    esac
}

# macOS 標準の bash 3.2 / BSD には readlink -f が無い。シンボリックリンク経由で呼ばれても、
# 実体のあるリポジトリを基準にするために自前で辿る
bs_real_script() {
    local src="${BASH_SOURCE[0]:-}" l n=0
    while [ -L "${src}" ] && [ "${n}" -lt 40 ]; do
        l="$(readlink "${src}")"
        case "${l}" in
            /*) src="${l}" ;;
            *) src="$(dirname "${src}")/${l}" ;;
        esac
        n=$((n + 1))
    done
    echo "$(cd -P "$(dirname "${src}")" && pwd -P)/$(basename "${src}")"
}

bs_in_repo() {
    local src="${BASH_SOURCE[0]:-}" dir
    case "${src}" in
        ""|/dev/*|/proc/*|-) return 1 ;;
    esac
    [ -f "${src}" ] || return 1
    dir="$(dirname "$(bs_real_script)")"
    [ -f "${dir}/../Cargo.toml" ] && grep -q '^name = "noth"' "${dir}/../Cargo.toml"
}

# https / ssh:// / scp 形式のどれで clone されていても、同じ GitHub リポジトリなら一致させる。
# GitHub の owner / repo は大文字小文字を区別しない
bs_normalize_url() {
    local u="$1"
    case "${u}" in
        git@github.com:*) u="https://github.com/${u#git@github.com:}" ;;
        ssh://git@github.com/*) u="https://github.com/${u#ssh://git@github.com/}" ;;
    esac
    u="${u%/}"
    u="${u%.git}"
    u="${u%/}"
    local lower
    lower="$(echo "${u}" | tr '[:upper:]' '[:lower:]')"
    case "${lower}" in
        https://github.com/*) u="${lower}" ;;
    esac
    echo "${u}"
}

# 対象の ref に合わせる。fast-forward できない場合も、別の版のままビルドしないよう、
# 対話時は確認のうえチェックアウトし、それ以外は中断する
bs_sync_ref() {
    local dir="$1" ref="$2" target cur tgt is_branch=0 ff=0 on_branch ahead
    if git -C "${dir}" show-ref --verify --quiet "refs/remotes/origin/${ref}"; then
        target="origin/${ref}"
        is_branch=1
    else
        target="${ref}"
    fi
    tgt="$(git -C "${dir}" rev-parse --verify --quiet "${target}^{commit}")" \
        || bs_die "ref ${ref} が見つかりません(${dir})"
    cur="$(git -C "${dir}" rev-parse HEAD)"
    if [ "${cur}" = "${tgt}" ]; then
        echo "ソースは ${ref} と同じ版です"
        return 0
    fi
    if [ -n "$(git -C "${dir}" status --porcelain)" ]; then
        local msg="${dir} に未コミットの変更があり、HEAD(${cur:0:7})が ${ref}(${tgt:0:7})と異なるため更新できません"
        if [ "${BS_ASK}" = 1 ]; then
            echo "警告: ${msg}" >&2
            bs_confirm "更新せずに、いまのソースで続行しますか?" || bs_die "中断しました"
            return 0
        fi
        bs_die "${msg}。手で直す: git -C ${dir} stash && git -C ${dir} checkout ${ref}(または --src で別の場所を指定)"
    fi
    if git -C "${dir}" merge-base --is-ancestor "${cur}" "${tgt}"; then
        ff=1
    fi
    on_branch="$(git -C "${dir}" symbolic-ref --quiet --short HEAD || true)"

    # 開発用 clone を --src に指されても、未 push のコミットを到達不能にしないため、
    # ローカルブランチが origin より進んでいれば、対話でも中断する
    if [ "${is_branch}" = 1 ] && git -C "${dir}" show-ref --verify --quiet "refs/heads/${ref}"; then
        ahead="$(git -C "${dir}" rev-list --count "origin/${ref}..${ref}")"
        if [ "${ahead}" -gt 0 ]; then
            bs_die "ローカルの ${ref} に未 push のコミットが ${ahead} 件あります。確かめる: git -C ${dir} log origin/${ref}..${ref}(または --src で別の場所を指定)"
        fi
    fi
    # detached で、どの ref からも辿れないコミットを持っていると、切り替えで失われる
    if [ -z "${on_branch}" ] && [ "${ff}" = 0 ] \
        && [ -n "$(git -C "${dir}" rev-list -n 1 HEAD --not --branches --remotes --tags)" ]; then
        bs_die "${dir} の detached HEAD(${cur:0:7})にどのブランチやタグにも無いコミットがあるため、${ref} へ切り替えません。確かめる: git -C ${dir} log --oneline HEAD --not --branches --remotes --tags"
    fi

    if [ -n "${on_branch}" ] && [ "${on_branch}" != "${ref}" ]; then
        if [ "${BS_ASK}" != 1 ]; then
            bs_die "${dir} は作業ブランチ ${on_branch} にいます。${ref} への切り替えは対話時だけ確認のうえ行います。手で直す: git -C ${dir} checkout ${ref}(または --src で別の場所を指定)"
        fi
        bs_confirm "${on_branch} から ${ref}(${tgt:0:7})に切り替えますか?" || bs_die "中断しました"
    elif [ "${BS_ASK}" = 1 ] && [ "${on_branch}" = "${ref}" ]; then
        bs_confirm "ソースを ${ref}(${tgt:0:7})に fast-forward しますか?" || bs_die "中断しました"
    fi

    if [ "${is_branch}" = 1 ]; then
        if git -C "${dir}" show-ref --verify --quiet "refs/heads/${ref}"; then
            if ! { git -C "${dir}" checkout --quiet "${ref}" && git -C "${dir}" merge --quiet --ff-only "${target}"; }; then
                bs_die "${ref} の更新に失敗しました(${dir})"
            fi
        else
            git -C "${dir}" checkout --quiet -b "${ref}" --track "${target}" \
                || bs_die "${ref} のチェックアウトに失敗しました(${dir})"
        fi
    else
        git -C "${dir}" checkout --quiet --detach "${tgt}" || bs_die "${ref} のチェックアウトに失敗しました(${dir})"
    fi
}

bootstrap() {
    local repo_url="${NOTH_REPO_URL:-https://github.com/NotHumanServer/noth.git}"
    local src="${NOTH_SRC_DIR:-}" src_given=0 ref="main" dry=0 yes=0 a prev=""
    # 引き継ぎ先がまたここへ来ると exec を繰り返し続けるため、一度きりにする
    [ -z "${NOTH_INSTALL_BOOTSTRAPPED:-}" ] \
        || bs_die "bootstrap に再入しました。clone 先の scripts/install.sh をリポジトリ内のスクリプトと判定できません"
    [ -z "${src}" ] || src_given=1
    for a in "$@"; do
        case "${prev}" in
            --src|--ref)
                case "${a}" in
                    -*) bs_die "${prev} には値が必要です(指定: ${a})" ;;
                esac
                if [ "${prev}" = "--src" ]; then src="${a}"; src_given=1; else ref="${a}"; fi
                prev=""
                continue ;;
        esac
        case "${a}" in
            --src|--ref) prev="${a}" ;;
            --src=*) src="${a#--src=}"; src_given=1 ;;
            --ref=*) ref="${a#--ref=}" ;;
            --dry-run) dry=1 ;;
            --yes|-y) yes=1 ;;
            -h|--help) usage; exit 0 ;;
        esac
    done
    [ -z "${prev}" ] || bs_die "${prev} には値が必要です"

    # macOS の /usr/bin/git は CommandLineTools が無いと実行時に失敗するシムなので、存在だけでなく動作も見る
    { command -v git >/dev/null 2>&1 && git --version >/dev/null 2>&1; } || bs_die "git が見つかりません。インストールしてください(macOS: xcode-select --install)。
  本体の実行には devenv(https://devenv.sh/getting-started/)、または gum / jq / cargo と bash 4.4 以降が必要です"

    BS_ASK=0
    if bs_have_tty && [ -t 1 ] && [ "${yes}" = 0 ]; then
        BS_ASK=1
    fi
    local tty_ok=0
    if bs_have_tty && [ -t 1 ]; then
        tty_ok=1
    fi

    if [ -z "${src}" ]; then
        src="${XDG_DATA_HOME:-${HOME}/.local/share}/noth/src"
    fi
    if [ "${BS_ASK}" = 1 ] && [ "${src_given}" = 0 ]; then
        local ans
        printf 'ソースの clone 先 [%s]: ' "${src}" >&2
        read -r ans </dev/tty || ans=""
        [ -z "${ans}" ] || src="${ans}"
    fi
    case "${src}" in
        \~) src="${HOME}" ;;
        \~/*) src="${HOME}/${src#\~/}" ;;
    esac
    case "${src}" in
        /*) ;;
        *) src="${PWD}/${src}" ;;
    esac

    echo "noth のソース: ${src}(ref: ${ref})"
    if [ -d "${src}" ] && [ -n "$(ls -A "${src}" 2>/dev/null)" ]; then
        local origin
        origin="$(git -C "${src}" remote get-url origin 2>/dev/null || true)"
        if [ -z "${origin}" ] || [ "$(bs_normalize_url "${origin}")" != "$(bs_normalize_url "${repo_url}")" ]; then
            bs_die "${src} は noth のリポジトリではありません(origin: ${origin:-なし})。--src で別の場所を指定してください"
        fi
        if [ "${dry}" = 1 ]; then
            echo "[dry-run] git -C ${src} fetch --tags origin; ${ref} へ fast-forward できれば更新(確認あり)"
        else
            git -C "${src}" fetch --quiet --tags origin || bs_die "git fetch に失敗しました(${src})"
            bs_sync_ref "${src}" "${ref}"
        fi
    else
        if [ "${dry}" = 1 ]; then
            echo "[dry-run] git clone ${repo_url} ${src}"
            echo "[dry-run] git -C ${src} checkout ${ref}"
            echo "[dry-run] clone 先が無いため、ここで止めます(clone 後に scripts/install.sh --dry-run で全体を確認できます)"
            exit 0
        fi
        if [ "${BS_ASK}" = 1 ] && ! bs_confirm "${repo_url} を ${src} に clone しますか?"; then
            echo "中断しました"
            exit 1
        fi
        mkdir -p "$(dirname "${src}")"
        git clone --quiet "${repo_url}" "${src}" || bs_die "git clone に失敗しました(${repo_url})"
        if ! git -C "${src}" checkout --quiet "${ref}" 2>/dev/null; then
            # 空か未作成だったディレクトリに今 clone したものだけを消す
            rm -rf "${src}"
            bs_die "ref ${ref} が見つかりません。作成した clone(${src})は削除しました"
        fi
    fi

    [ -f "${src}/scripts/install.sh" ] \
        || bs_die "この ref(${ref})にはインストールスクリプトがありません(${src}/scripts/install.sh)"

    if [ "${tty_ok}" = 1 ]; then
        NOTH_INSTALL_BOOTSTRAPPED=1 exec bash "${src}/scripts/install.sh" "$@" </dev/tty
    fi
    NOTH_INSTALL_BOOTSTRAPPED=1 exec bash "${src}/scripts/install.sh" "$@" </dev/null
}

# 本体は関数にまとめて最後の行で呼ぶ。curl | bash で転送の途中まで読んだ状態では何も実行されない
main() {
SELF="$(bs_real_script)"
REPO_ROOT="$(cd "$(dirname "${SELF}")/.." && pwd)"
# 相対パスの --prefix / NOTH_HOME は、入り直しで cd したあとも呼び出し元の cwd 基準で解決する
CALLER_CWD="${NOTH_INSTALL_CALLER_CWD:-${PWD}}"

# devenv.nix は cwd から探されるのでリポジトリへ移ってから入り直す。
# 入り直した先でも条件を満たさないときに再実行し続けないよう、環境変数で一度きりにする
reenter_devenv() {
    cd "${REPO_ROOT}"
    NOTH_INSTALL_IN_DEVENV=1 NOTH_INSTALL_CALLER_CWD="${CALLER_CWD}" \
        exec devenv shell -- bash "${SELF}" "$@"
}

can_reenter() {
    command -v devenv >/dev/null 2>&1 && [[ -z "${NOTH_INSTALL_IN_DEVENV:-}" ]]
}

# mapfile は bash 4.0、set -u で空配列を展開できるのは 4.4 から(macOS 標準の 3.2 はどちらも不可)。
# 入り直しのために、この判定は他の処理より前に置く
if (( BASH_VERSINFO[0] < 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] < 4) )); then
    if can_reenter; then
        echo "bash ${BASH_VERSION} では動かないため、devenv shell に入り直します" >&2
        reenter_devenv "$@"
    fi
    {
        echo "エラー: bash 4.4 以降が必要です(現在 ${BASH_VERSION})"
        echo "  devenv shell の中で実行するか、brew install bash で新しい bash を入れてください"
        echo "  devenv の導入: https://devenv.sh/getting-started/"
    } >&2
    exit 1
fi

ORIG_ARGS=("$@")

LABEL="dev.noth.noth"
DEFAULT_WORKER_TIMEOUT_SECS=600
# noth service の停止待ちと同じ余裕(src/cli/service.rs の STOP_MARGIN_SECS)
STOP_MARGIN_SECS=30
DEFAULT_SERVICE_PATH="/usr/local/bin:/usr/bin:/bin"

YES=0
DRY_RUN=0
STOP_FG=0
FEATURES=""
FEATURES_SET=0
SERVICE_MODE=""
PREFIX="${NOTH_INSTALL_PREFIX:-${HOME}/.local/bin}"

die() {
    echo "エラー: $*" >&2
    exit 1
}

while (( $# > 0 )); do
    case "$1" in
        --yes|-y) YES=1 ;;
        --dry-run) DRY_RUN=1 ;;
        --stop-foreground) STOP_FG=1 ;;
        --features)
            [[ $# -ge 2 ]] || die "--features には値が必要です"
            FEATURES="$2"; FEATURES_SET=1; shift ;;
        --features=*) FEATURES="${1#*=}"; FEATURES_SET=1 ;;
        --service)
            [[ $# -ge 2 ]] || die "--service には restart | install | skip のいずれかが必要です"
            SERVICE_MODE="$2"; shift ;;
        --service=*) SERVICE_MODE="${1#*=}" ;;
        --prefix)
            [[ $# -ge 2 ]] || die "--prefix にはディレクトリが必要です"
            PREFIX="$2"; shift ;;
        --prefix=*) PREFIX="${1#*=}" ;;
        --src|--ref)
            [[ $# -ge 2 ]] || die "$1 には値が必要です"
            [[ -n "${NOTH_INSTALL_BOOTSTRAPPED:-}" ]] || echo "注意: リポジトリ内から実行しているため $1 は無視します" >&2
            shift ;;
        --src=*|--ref=*)
            [[ -n "${NOTH_INSTALL_BOOTSTRAPPED:-}" ]] || echo "注意: リポジトリ内から実行しているため ${1%%=*} は無視します" >&2 ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; die "不明なオプションです: $1" ;;
    esac
    shift
done

case "${SERVICE_MODE}" in
    ""|restart|install|skip) ;;
    *) die "--service は restart | install | skip のいずれかです(指定: ${SERVICE_MODE})" ;;
esac

# 配置先は未作成のことがあるので、realpath ではなく文字列で正規化する
absolutize() {
    local p="$1" part
    local parts=() out=()
    case "${p}" in
        \~) p="${HOME}" ;;
        \~/*) p="${HOME}/${p#\~/}" ;;
    esac
    case "${p}" in
        /*) ;;
        *) p="${CALLER_CWD}/${p}" ;;
    esac
    IFS=/ read -r -a parts <<<"${p}"
    for part in "${parts[@]}"; do
        case "${part}" in
            ""|.) ;;
            ..) (( ${#out[@]} == 0 )) || unset 'out[-1]' ;;
            *) out+=("${part}") ;;
        esac
    done
    local IFS=/
    echo "/${out[*]}"
}
PREFIX="$(absolutize "${PREFIX}")"
NOTH_HOME="$(absolutize "${NOTH_HOME:-${HOME}/.noth}")"

INTERACTIVE=0
if [[ -t 0 && -t 1 ]]; then
    INTERACTIVE=1
fi

# gum は対話するときだけ使う。非対話の --dry-run まで開発シェルを要求しないため
missing_tools=()
command -v cargo >/dev/null 2>&1 || missing_tools+=(cargo)
command -v jq >/dev/null 2>&1 || missing_tools+=(jq)
if (( INTERACTIVE )); then
    command -v gum >/dev/null 2>&1 || missing_tools+=(gum)
fi
if (( ${#missing_tools[@]} > 0 )); then
    if can_reenter; then
        echo "${missing_tools[*]} が見つからないため、devenv shell に入り直します" >&2
        reenter_devenv "${ORIG_ARGS[@]}"
    fi
    {
        echo "エラー: 次のコマンドが見つかりません: ${missing_tools[*]}"
        echo "導入方法:"
        echo "  - devenv shell(推奨。devenv.nix に gum / jq を含みます。導入: https://devenv.sh/getting-started/)"
        echo "  - brew install gum jq"
    } >&2
    exit 1
fi

if (( ! INTERACTIVE )); then
    missing_flags=()
    (( FEATURES_SET )) || missing_flags+=("--features a,b  (任意 feature。無しなら --features '')")
    [[ -n "${SERVICE_MODE}" ]] || missing_flags+=("--service restart|install|skip")
    if (( ${#missing_flags[@]} > 0 )); then
        {
            echo "エラー: TTY が無いため対話できません。次の指定が必要です:"
            printf '  %s\n' "${missing_flags[@]}"
        } >&2
        exit 2
    fi
fi

# gum の既定スタイルは色付きなので、gum にだけ NO_COLOR を渡す(cargo などの子プロセスと利用者の環境は変えない)
gum() {
    NO_COLOR=1 command gum "$@"
}

# gum confirm は NO_COLOR だと選択中のボタンを見分けられなくなる。choose ならカーソル記号 `> ` が残る。
# Esc / Ctrl-C は 1(いいえ)と区別して中断する
confirm() {
    local msg="$1" default="${2:-true}" selected="はい" answer rc=0
    [[ "${default}" == "true" ]] || selected="いいえ"
    answer="$(gum choose --header "${msg}" --selected="${selected}" はい いいえ)" || rc=$?
    if (( rc != 0 )); then
        echo "中断しました" >&2
        exit 130
    fi
    [[ "${answer}" == "はい" ]]
}

run() {
    if (( DRY_RUN )); then
        printf '[dry-run] %q' "$1"; shift
        printf ' %q' "$@"; echo
    else
        "$@"
    fi
}

spin_logged() {
    local title="$1" log="$2"; shift 2
    if (( INTERACTIVE )); then
        # shellcheck disable=SC2016 # 子 bash に展開を任せるため
        gum spin --spinner dot --title "${title}" -- bash -c '
            log="$1"; shift
            "$@" >>"${log}" 2>&1' _ "${log}" "$@"
    else
        echo "${title}"
        "$@" >>"${log}" 2>&1
    fi
}

print_log_tail() {
    echo "--- ${1} の末尾 ---" >&2
    tail -n 30 "$1" >&2
    echo "-------------------" >&2
}

cmd_str() {
    printf '%q ' "$@"
}

contains() {
    local needle="$1"; shift
    local x
    for x in "$@"; do [[ "${x}" == "${needle}" ]] && return 0; done
    return 1
}

# 同じファイルを別名(シンボリックリンク、相対パス)で指していても一致させる
canonical_path() {
    local p="$1" d
    d="$(cd "$(dirname "${p}")" 2>/dev/null && pwd -P)" || { echo "${p}"; return; }
    echo "${d}/$(basename "${p}")"
}

if [[ "$(uname -s)" == "Darwin" ]]; then
    UNIT_FILE="${NOTH_INSTALL_PLIST:-${HOME}/Library/LaunchAgents/${LABEL}.plist}"
    SERVICE_NAME="launchd"
else
    UNIT_FILE="${NOTH_INSTALL_PLIST:-${HOME}/.config/systemd/user/noth-noth.service}"
    SERVICE_NAME="systemd"
fi

REG_PROGRAM=""
REG_CONFIG=""
REG_HOME=""
REG_PATH=""

# 定義ファイルがあるのに読めないとき「未登録」と見なすと、後の `service install` が
# 既存のジョブを launchctl remove で即停止してしまうので、読めなければ中断する
read_registration() {
    [[ -f "${UNIT_FILE}" ]] || return 0
    local args_json=()
    if [[ "${UNIT_FILE}" == *.plist ]]; then
        command -v plutil >/dev/null 2>&1 || die "plutil が無く、登録済みの定義を読めません: ${UNIT_FILE}"
        local json
        json="$(plutil -extract ProgramArguments json -o - "${UNIT_FILE}")" \
            || die "登録済みの定義を読めません: ${UNIT_FILE}"
        REG_PROGRAM="$(jq -r '.[0] // empty' <<<"${json}")"
        REG_CONFIG="$(jq -r '(index("-c") // index("--config")) as $i | if $i then .[$i + 1] // empty else empty end' <<<"${json}")"
        REG_HOME="$(plutil -extract EnvironmentVariables.NOTH_HOME raw -o - "${UNIT_FILE}" 2>/dev/null || true)"
        REG_PATH="$(plutil -extract EnvironmentVariables.PATH raw -o - "${UNIT_FILE}" 2>/dev/null || true)"
    else
        local exec_line
        exec_line="$(sed -n 's/^ExecStart=//p' "${UNIT_FILE}" | head -n 1)"
        mapfile -t args_json < <(grep -o '"[^"]*"' <<<"${exec_line}" | tr -d '"' || true)
        [[ ${#args_json[@]} -gt 0 ]] || die "登録済みの定義を読めません: ${UNIT_FILE}"
        REG_PROGRAM="${args_json[0]}"
        local i
        for i in "${!args_json[@]}"; do
            if [[ "${args_json[i]}" == "-c" || "${args_json[i]}" == "--config" ]]; then
                REG_CONFIG="${args_json[i + 1]:-}"
            fi
        done
        REG_HOME="$(sed -n 's/^Environment="NOTH_HOME=\(.*\)"$/\1/p' "${UNIT_FILE}" | head -n 1)"
        REG_PATH="$(sed -n 's/^Environment="PATH=\(.*\)"$/\1/p' "${UNIT_FILE}" | head -n 1)"
    fi
    [[ -n "${REG_PROGRAM}" ]] || die "登録済みの定義に実行ファイルがありません: ${UNIT_FILE}"
}
read_registration

# 登録済みサービスが実際に読んでいる設定を基準にしないと、検証や lock 検出が別の NOTH_HOME を見てしまう
CFG_ARGS=()
if [[ -n "${REG_CONFIG}" ]]; then
    CFG_ARGS=(-c "${REG_CONFIG}")
    NOTH_HOME="$(dirname "${REG_CONFIG}")"
    CONFIG_FILE="${REG_CONFIG}"
else
    [[ -z "${REG_HOME}" ]] || NOTH_HOME="${REG_HOME}"
    CONFIG_FILE="${NOTH_HOME}/config.toml"
fi
export NOTH_HOME

DEST="${PREFIX%/}/noth"
REG_STATE="none"
if [[ -n "${REG_PROGRAM}" ]]; then
    if [[ "$(canonical_path "${REG_PROGRAM}")" == "$(canonical_path "${DEST}")" ]]; then
        REG_STATE="same"
    else
        REG_STATE="other"
    fi
fi

cd "${REPO_ROOT}"
branch="$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo '?')"
head_line="$(git log -1 --format='%h %s' 2>/dev/null || echo '?')"
echo "ブランチ: ${branch}"
echo "HEAD:     ${head_line}"
if [[ -n "$(git status --porcelain 2>/dev/null)" ]]; then
    echo "警告: 作業ツリーに未コミットの変更があります(ビルドに含まれます)" >&2
    if (( INTERACTIVE && ! YES )); then
        confirm "このまま続行しますか?" false || { echo "中断しました"; exit 1; }
    fi
fi
if [[ -n "${REG_PROGRAM}" ]]; then
    echo "登録済みの常駐の設定を基準にします: NOTH_HOME=${NOTH_HOME}${REG_CONFIG:+ / -c ${REG_CONFIG}}"
fi

metadata="$(cargo metadata --no-deps --format-version 1)"
# 既定に入っていない feature が「任意」。名前は config.toml の module 名と 1:1
mapfile -t OPTIONAL_FEATURES < <(jq -r '
    .packages[] | select(.name == "noth") | .features
    | (.default // []) as $d
    | keys[] | select(. != "default" and (. as $k | $d | index($k) | not))' <<<"${metadata}")
mapfile -t DEFAULT_FEATURES < <(jq -r '
    .packages[] | select(.name == "noth") | .features.default[]?' <<<"${metadata}")
TARGET_DIR="$(jq -r '.target_directory' <<<"${metadata}")"

# `noth config get` は新旧どちらのバイナリも無い段階では使えないので、ここは自前で読む。
# 雛形はコメントアウトした module を含み、配列も複数行に跨る
config_enabled() {
    [[ -f "${CONFIG_FILE}" ]] || return 0
    awk '
        /^\[modules\]/ { in_sec = 1; next }
        /^\[/ { in_sec = 0 }
        in_sec {
            line = $0
            sub(/#.*$/, "", line)
            if (line ~ /^[[:space:]]*enabled[[:space:]]*=/) grab = 1
            if (grab) print line
            if (grab && line ~ /\]/) exit
        }
    ' "${CONFIG_FILE}" | grep -oE "\"[^\"]*\"|'[^']*'" | tr -d "\"'" || true
}
mapfile -t CONFIG_ENABLED < <(config_enabled)

selected=()
if (( FEATURES_SET )); then
    if [[ -n "${FEATURES}" ]]; then
        IFS=',' read -r -a selected <<<"${FEATURES}"
    fi
    for f in "${selected[@]}"; do
        contains "${f}" "${OPTIONAL_FEATURES[@]}" || \
            die "未知の任意 feature です: ${f}(選べるもの: ${OPTIONAL_FEATURES[*]})"
    done
elif (( ${#OPTIONAL_FEATURES[@]} > 0 )); then
    preselect=()
    for f in "${OPTIONAL_FEATURES[@]}"; do
        contains "${f}" "${CONFIG_ENABLED[@]}" && preselect+=("${f}")
    done
    selected_csv=""
    if (( ${#preselect[@]} > 0 )); then
        selected_csv="$(IFS=','; echo "${preselect[*]}")"
    fi
    echo
    choose_rc=0
    choice="$(gum choose --no-limit --selected="${selected_csv}" \
        --header "任意 feature を選択(space で切替、enter で確定。config.toml で有効なものは選択済み)" \
        "${OPTIONAL_FEATURES[@]}")" || choose_rc=$?
    (( choose_rc == 0 )) || { echo "中断しました" >&2; exit 130; }
    if [[ -n "${choice}" ]]; then
        mapfile -t selected <<<"${choice}"
    fi
fi

built_in=("${DEFAULT_FEATURES[@]}" "${selected[@]}")
config_missing=()
for m in "${CONFIG_ENABLED[@]}"; do
    contains "${m}" "${built_in[@]}" || config_missing+=("${m}")
done
if (( ${#config_missing[@]} > 0 )); then
    echo "警告: config.toml が有効にしている module が組み込まれません: ${config_missing[*]}" >&2
    echo "      このままでは config check が通りません" >&2
    if (( INTERACTIVE && ! YES )); then
        confirm "このまま続行しますか?" false || { echo "中断しました"; exit 1; }
    fi
fi

features_csv=""
if (( ${#selected[@]} > 0 )); then
    features_csv="$(IFS=','; echo "${selected[*]}")"
fi

stop_timeout_secs() {
    local t
    t="$(awk '
        /^\[worker\]/ { in_sec = 1; next }
        /^\[/ { in_sec = 0 }
        in_sec && /^[[:space:]]*timeout_secs[[:space:]]*=/ {
            v = $0
            sub(/^[^=]*=[[:space:]]*/, "", v)
            sub(/#.*$/, "", v)
            gsub(/[[:space:]_]/, "", v)
            print v
            exit
        }' "${CONFIG_FILE}" 2>/dev/null || true)"
    [[ "${t}" =~ ^[0-9]+$ ]] || t="${DEFAULT_WORKER_TIMEOUT_SECS}"
    echo $(( t + STOP_MARGIN_SECS ))
}
STOP_WAIT_SECS="$(stop_timeout_secs)"
STOP_WAIT_MIN=$(( (STOP_WAIT_SECS + 59) / 60 ))

service_main_pid() {
    if [[ "${SERVICE_NAME}" == "launchd" ]]; then
        launchctl list "${LABEL}" 2>/dev/null | awk '/"PID"/ { gsub(/;/, "", $3); print $3 }'
    else
        systemctl --user show -p MainPID --value noth-noth.service 2>/dev/null | awk '$1 > 0'
    fi
}

# lock ファイルには終了後も pid が残り、別プロセスに再利用されうる。noth は flock の fd を開いたまま
# 常駐するので、lsof でファイルを実際に開いている pid と一致したものだけを noth と見なす
find_foreground() {
    local lock="${NOTH_HOME}/data/noth.lock" pid svc cmd
    [[ -f "${lock}" ]] || return 0
    pid="$(tr -dc '0-9' <"${lock}")"
    [[ -n "${pid}" ]] && kill -0 "${pid}" 2>/dev/null || return 0
    if command -v lsof >/dev/null 2>&1; then
        lsof -t -- "${lock}" 2>/dev/null | grep -qx "${pid}" || return 0
    else
        [[ "$(basename "$(ps -p "${pid}" -o comm= 2>/dev/null || true)")" == "noth" ]] || return 0
    fi
    svc="$(service_main_pid || true)"
    [[ "${pid}" != "${svc}" ]] || return 0
    cmd="$(ps -p "${pid}" -o command= 2>/dev/null || true)"
    printf '%s\t%s\n' "${pid}" "${cmd}"
}

FG_PID=""
FG_CMD=""
fg_line="$(find_foreground)"
if [[ -n "${fg_line}" ]]; then
    FG_PID="${fg_line%%$'\t'*}"
    FG_CMD="${fg_line#*$'\t'}"
fi

action_for_mode() {
    case "$1:${REG_STATE}" in
        restart:same|install:same) echo restart ;;
        restart:other|install:other) echo reinstall ;;
        install:none) echo register ;;
        *) echo skip ;;
    esac
}

ACTION="skip"
if [[ -n "${SERVICE_MODE}" ]]; then
    ACTION="$(action_for_mode "${SERVICE_MODE}")"
fi

# ビルド後に気づくと時間の無駄になるので、非対話で詰むことが分かっている場合は先に止める
if (( ! INTERACTIVE )) && [[ "${ACTION}" != "skip" && -n "${FG_PID}" ]] && (( ! STOP_FG )); then
    {
        echo "エラー: ${SERVICE_NAME} 管理外の noth run(PID ${FG_PID})が動いています。止めないまま常駐を起動すると二重起動になります。"
        echo "  止めてよければ --stop-foreground を、常駐に触らないなら --service skip を指定してください"
    } >&2
    exit 2
fi

LOG_DIR="${NOTH_HOME}/logs"
LOG_FILE="${LOG_DIR}/install-$(date +%Y%m%d-%H%M%S).log"
NEW_BIN="${TARGET_DIR}/release/noth"

build_cmd=(cargo build --release --locked)
[[ -n "${features_csv}" ]] && build_cmd+=(--features "${features_csv}")

echo
echo "feature:   既定 + ${features_csv:-(任意 feature なし)}"
echo "配置先:    ${DEST}"
echo "ログ:      ${LOG_FILE}"
echo

if (( DRY_RUN )); then
    run "${build_cmd[@]}"
else
    mkdir -p "${LOG_DIR}"
    : >"${LOG_FILE}"
    started=$SECONDS
    if ! spin_logged "release ビルド中(初回は数分かかります)" "${LOG_FILE}" "${build_cmd[@]}"; then
        echo "エラー: ビルドに失敗しました" >&2
        print_log_tail "${LOG_FILE}"
        exit 1
    fi
    echo "ビルド完了($(( SECONDS - started )) 秒)"
fi

new_version=""
if (( DRY_RUN )); then
    run "${NEW_BIN}" --version
    run "${NEW_BIN}" "${CFG_ARGS[@]}" config check
else
    verify_failed=0
    new_version="$("${NEW_BIN}" --version 2>&1)" || verify_failed=1
    echo "バージョン: ${new_version}"
    if ! "${NEW_BIN}" "${CFG_ARGS[@]}" config check >>"${LOG_FILE}" 2>&1; then
        verify_failed=1
        echo "エラー: 新しいバイナリの config check に失敗しました(NOTH_HOME=${NOTH_HOME})" >&2
        print_log_tail "${LOG_FILE}"
    fi
    if (( verify_failed )); then
        if (( INTERACTIVE && ! YES )); then
            confirm "検証に失敗しましたが、インストールを続行しますか?" false || { echo "中断しました"; exit 1; }
        else
            die "検証に失敗したためインストールを止めました"
        fi
    fi
fi

# 上書きコピーだと実行中のバイナリを壊し、macOS では inode を使い回すとコード署名の検証に失敗することがある。
# 同じディレクトリの一時ファイルから mv で入れ替えれば inode が新しくなり、rename も同一ファイルシステム内で済む
install_atomically() {
    local src="$1" dst="$2"
    INSTALL_TMP="$(mktemp "${PREFIX%/}/.noth.XXXXXX")" || return 1
    trap 'rm -f -- "${INSTALL_TMP}"' EXIT
    cp ${3:+"$3"} "${src}" "${INSTALL_TMP}" || return 1
    chmod 755 "${INSTALL_TMP}" || return 1
    mv -f "${INSTALL_TMP}" "${dst}" || return 1
    trap - EXIT
}

run mkdir -p "${PREFIX}"
if (( DRY_RUN )); then
    [[ ! -e "${DEST}" ]] || echo "[dry-run] 一時ファイルに cp -p ${DEST} → mv -f で ${DEST}.prev に退避"
    echo "[dry-run] 一時ファイルに cp ${NEW_BIN} → chmod 755 → mv -f で ${DEST} に入れ替え"
else
    [[ ! -e "${DEST}" ]] || install_atomically "${DEST}" "${DEST}.prev" -p || die "noth.prev への退避に失敗しました"
    install_atomically "${NEW_BIN}" "${DEST}" || die "配置に失敗しました"
    echo "配置しました: ${DEST}"
fi

case ":${PATH}:" in
    *":${PREFIX}:"*) ;;
    *) echo "警告: ${PREFIX} が PATH に含まれていません。シェルの設定に追加してください" >&2 ;;
esac

SVC_STOPPED=0
if (( ! DRY_RUN )) && [[ "${REG_STATE}" == "same" && -x "${DEST}" ]]; then
    [[ "$("${DEST}" "${CFG_ARGS[@]}" service status 2>/dev/null || true)" == *停止中* ]] && SVC_STOPPED=1
fi

echo
case "${REG_STATE}" in
    same) echo "常駐: ${SERVICE_NAME} に登録済み(${REG_PROGRAM})$( (( SVC_STOPPED )) && echo ',停止中')" ;;
    other) echo "常駐: ${SERVICE_NAME} に登録済みですが、別のバイナリを指しています: ${REG_PROGRAM}" ;;
    none) echo "常駐: ${SERVICE_NAME} には未登録です" ;;
esac

if [[ -n "${SERVICE_MODE}" ]]; then
    if [[ "${SERVICE_MODE}" == "restart" && "${REG_STATE}" == "none" ]]; then
        echo "注意: 未登録なので --service restart では何もしません(登録するなら --service install)"
    fi
else
    case "${REG_STATE}" in
        same)
            restart_prompt="常駐を再起動して新しいバイナリに切り替えますか?(実行中の Worker を待つため最長約 ${STOP_WAIT_MIN} 分かかります)"
            (( ! SVC_STOPPED )) || restart_prompt="常駐は停止中です。起動して新しいバイナリに切り替えますか?"
            if confirm "${restart_prompt}" false; then
                ACTION="restart"
            fi ;;
        other)
            if confirm "新しいバイナリ(${DEST})で登録し直しますか?(停止に最長約 ${STOP_WAIT_MIN} 分かかります)" false; then
                ACTION="reinstall"
            fi ;;
        none)
            pick_rc=0
            pick="$(gum choose --header "${SERVICE_NAME} への登録方法" \
                "${SERVICE_NAME} に登録して起動" "何もしない")" || pick_rc=$?
            (( pick_rc == 0 )) || { echo "中断しました" >&2; exit 130; }
            [[ "${pick}" == "何もしない" ]] || ACTION="register" ;;
    esac
fi

# 管理外の noth run と同じ lock を取り合うのは常駐を起動するときだけなので、触らない操作では何も聞かない
if [[ "${ACTION}" != "skip" && -n "${FG_PID}" ]]; then
    echo
    echo "${SERVICE_NAME} 管理外の noth run が動いています:"
    echo "  PID ${FG_PID}  ${FG_CMD}"
    echo "止めないまま常駐を起動すると、二重起動になります。"
    do_stop=0
    if (( STOP_FG )); then
        do_stop=1
    elif (( INTERACTIVE )); then
        confirm "SIGTERM で止めて終了を待ちますか?(進行中の Worker を待つため最長約 ${STOP_WAIT_MIN} 分)" false && do_stop=1
    fi
    if (( ! do_stop )); then
        echo "警告: 二重起動を避けるため、常駐の操作を行いません" >&2
        ACTION="skip"
    elif (( DRY_RUN )); then
        echo "[dry-run] kill -TERM ${FG_PID}; 終了を最長 ${STOP_WAIT_SECS} 秒待つ"
    else
        # すでに終了していても落ちないよう、結果は後段の生存確認で判定する
        kill -TERM "${FG_PID}" 2>/dev/null || true
        # shellcheck disable=SC2016 # 子 bash に展開を任せるため
        wait_script='
            deadline=$(( SECONDS + $1 ))
            while kill -0 "$2" 2>/dev/null; do
                (( SECONDS < deadline )) || exit 1
                sleep 1
            done'
        wait_title="noth run(PID ${FG_PID})の終了を待っています(最長 ${STOP_WAIT_SECS} 秒)"
        wait_rc=0
        if (( INTERACTIVE )); then
            gum spin --spinner dot --title "${wait_title}" -- bash -c "${wait_script}" _ "${STOP_WAIT_SECS}" "${FG_PID}" || wait_rc=$?
        else
            echo "${wait_title}"
            bash -c "${wait_script}" _ "${STOP_WAIT_SECS}" "${FG_PID}" || wait_rc=$?
        fi
        if (( wait_rc != 0 )); then
            {
                echo "エラー: SIGTERM を送りましたが、${STOP_WAIT_SECS} 秒たっても PID ${FG_PID} が終了しません。"
                echo "  進行中の Worker の完了待ちか、停止できない状態の可能性があります。"
                echo "  バイナリは ${DEST} に配置済みですが、常駐の操作は行っていません。"
                echo "  終了を確かめてから、--service を指定して再実行するか ${DEST} service start を実行してください。"
            } >&2
            exit 1
        fi
        echo "停止しました"
    fi
fi

# 現在の PATH(devenv の nix store など)を定義に焼き付けないよう、ログインシェルの PATH を使う
login_path() {
    local p
    # shellcheck disable=SC2016 # ログインシェル側で展開させるため
    p="$(env -i HOME="${HOME}" "${SHELL:-/bin/sh}" -lc 'printf %s "$PATH"' 2>/dev/null || true)"
    if [[ -z "${p}" || "${p}" == *$'\n'* || ( "${p}" != /* && "${p}" != *:/* ) ]]; then
        p="${DEFAULT_SERVICE_PATH}"
    fi
    echo "${p}"
}

SVC_NOTE=""
svc() {
    local title="$1"; shift
    if (( DRY_RUN )); then
        run "$@"
        return 0
    fi
    spin_logged "${title}" "${LOG_FILE}" "$@" || {
        echo "エラー: 失敗しました: $(cmd_str "$@")" >&2
        print_log_tail "${LOG_FILE}"
        return 1
    }
}

service_install_start() {
    local path_env="$1" install_cmd start_cmd
    install_cmd=(env "NOTH_HOME=${NOTH_HOME}" "PATH=${path_env}" "${DEST}" "${CFG_ARGS[@]}" service install)
    start_cmd=("${DEST}" "${CFG_ARGS[@]}" service start)
    echo "登録に使う PATH: ${path_env}"
    svc "登録中" "${install_cmd[@]}" || {
        SVC_NOTE="いまは未登録で、常駐していません。復旧: $(cmd_str "${install_cmd[@]}")&& $(cmd_str "${start_cmd[@]}")"
        return 1
    }
    svc "常駐を起動中" "${start_cmd[@]}" || {
        SVC_NOTE="登録済みですが停止中です。復旧: $(cmd_str "${start_cmd[@]}")"
        return 1
    }
}

do_service_action() {
    local status_cmd restart_cmd
    status_cmd="$(cmd_str "${DEST}" "${CFG_ARGS[@]}" service status)"
    case "${ACTION}" in
        restart)
            restart_cmd=("${DEST}" "${CFG_ARGS[@]}" service restart)
            svc "常駐を再起動中(最長約 ${STOP_WAIT_MIN} 分)" "${restart_cmd[@]}" || {
                SVC_NOTE="停止中のままの可能性があります。確認: ${status_cmd}/ 復旧: $(cmd_str "${DEST}" "${CFG_ARGS[@]}" service start)"
                return 1
            } ;;
        reinstall)
            svc "登録を解除中(最長約 ${STOP_WAIT_MIN} 分)" "${DEST}" "${CFG_ARGS[@]}" service uninstall || {
                SVC_NOTE="登録解除に失敗しました。登録と稼働の状態は変わっていない可能性があります。確認: ${status_cmd}"
                return 1
            }
            service_install_start "${REG_PATH:-$(login_path)}" ;;
        register)
            service_install_start "$(login_path)" ;;
    esac
}

SVC_FAILED=0
if [[ "${ACTION}" != "skip" ]]; then
    (( DRY_RUN )) || : >>"${LOG_FILE}"
    do_service_action || SVC_FAILED=1
fi

echo
echo "=== 要約 ==="
if (( DRY_RUN )); then
    echo "(dry-run: 何も変更していません)"
    echo "バージョン: (ビルドしていないため不明)"
else
    echo "バージョン: ${new_version}"
fi
echo "配置先:     ${DEST}"
echo "feature:    既定 + ${features_csv:-(任意 feature なし)}"
plan_suffix="しました"
(( DRY_RUN )) && plan_suffix="します(予定)"
if (( SVC_FAILED )); then
    echo "常駐:       失敗しました"
    echo "  ${SVC_NOTE}"
else
    case "${ACTION}" in
        restart)
            if (( SVC_STOPPED )); then echo "常駐:       起動${plan_suffix}"; else echo "常駐:       再起動${plan_suffix}"; fi ;;
        reinstall) echo "常駐:       登録し直して起動${plan_suffix}" ;;
        register) echo "常駐:       登録して起動${plan_suffix}" ;;
        *) echo "常駐:       変更なし" ;;
    esac
fi
if (( ! DRY_RUN )) && [[ -x "${DEST}" ]]; then
    "${DEST}" "${CFG_ARGS[@]}" service status 2>/dev/null | sed 's/^/            /' || true
fi
echo "ログ:       ${LOG_FILE}"
if [[ -e "${DEST}.prev" ]] || (( DRY_RUN )); then
    echo "切り戻し:   mv -f ${DEST}.prev ${DEST} && ${DEST} service restart"
fi
exit "${SVC_FAILED}"

}

for bs_arg in "$@"; do
    case "${bs_arg}" in
        -h|--help) usage; exit 0 ;;
    esac
done

if bs_in_repo; then
    main "$@"
else
    bootstrap "$@"
fi
