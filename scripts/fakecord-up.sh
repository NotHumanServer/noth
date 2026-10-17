#!/usr/bin/env bash
# fakecord と noth をまとめて起動する開発用スクリプト
set -euo pipefail

# wait -n に pid を渡すのは bash 4.3 以降(macOS 標準の 3.2 では動かない)
if (( BASH_VERSINFO[0] < 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] < 3) )); then
    echo "警告: bash 4.3 以降が必要です(現在 ${BASH_VERSION})" >&2
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

NOTH_HOME="${NOTH_HOME:-${HOME}/.noth-dev}"
FAKECORD_PORT="${FAKECORD_PORT:-3900}"
# 既定の feature に無い provider などを noth に足す(カンマ区切り)
NOTH_FEATURES="${NOTH_FEATURES:-provider-openai}"

mkdir -p "${NOTH_HOME}/logs"

cd "${REPO_ROOT}"
cargo build -p noth-fakecord -p noth --features "${NOTH_FEATURES}"
"${REPO_ROOT}/scripts/prune-target.sh"

CONFIG_FILE="${NOTH_HOME}/config.toml"
# [channel.discord] の api_base の値(引用符は外す)。無ければ空
configured_api_base() {
    [[ -f "${CONFIG_FILE}" ]] || return 0
    awk '
        /^\[channel\.discord\]/ { in_section = 1; next }
        /^\[/ { in_section = 0 }
        in_section && /^[[:space:]]*api_base[[:space:]]*=/ {
            v = $0
            sub(/^[^=]*=[[:space:]]*/, "", v)
            sub(/[[:space:]]*#.*$/, "", v)
            gsub(/["\047[:space:]]/, "", v)
            sub(/\/+$/, "", v)
            print v
            exit
        }
    ' "${CONFIG_FILE}"
}
API_BASE="$(configured_api_base)"
if [[ "${API_BASE}" != "http://127.0.0.1:${FAKECORD_PORT}" && "${API_BASE}" != "http://localhost:${FAKECORD_PORT}" ]]; then
    echo "警告: ${CONFIG_FILE} の [channel.discord] api_base が http://127.0.0.1:${FAKECORD_PORT} (または http://localhost:${FAKECORD_PORT}) ではありません(現在: ${API_BASE:-未設定})。本物の Discord に繋がないため、起動を止めます。" >&2
    exit 1
fi

FAKECORD_BIN="${REPO_ROOT}/target/debug/fakecord"
NOTH_BIN="${REPO_ROOT}/target/debug/noth"

FAKECORD_PID=""
NOTH_PID=""

cleanup() {
    if [[ -n "${NOTH_PID}" ]]; then
        kill "${NOTH_PID}" 2>/dev/null || true
        wait "${NOTH_PID}" 2>/dev/null || true
    fi
    if [[ -n "${FAKECORD_PID}" ]]; then
        kill "${FAKECORD_PID}" 2>/dev/null || true
        wait "${FAKECORD_PID}" 2>/dev/null || true
    fi
}
trap cleanup INT TERM EXIT

"${FAKECORD_BIN}" serve \
    --port "${FAKECORD_PORT}" \
    --snapshot "${NOTH_HOME}/fakecord.json" \
    >>"${NOTH_HOME}/logs/fakecord.log" 2>&1 &
FAKECORD_PID=$!

# /_fake/status が応答するまで待つ
for _ in $(seq 1 50); do
    if curl -sf "http://127.0.0.1:${FAKECORD_PORT}/_fake/status" >/dev/null 2>&1; then
        break
    fi
    sleep 0.2
done
if ! curl -sf "http://127.0.0.1:${FAKECORD_PORT}/_fake/status" >/dev/null 2>&1; then
    echo "警告: fakecord が起動しませんでした (port ${FAKECORD_PORT})" >&2
    exit 1
fi

NOTH_HOME="${NOTH_HOME}" "${NOTH_BIN}" run \
    >>"${NOTH_HOME}/logs/noth.log" 2>&1 &
NOTH_PID=$!

# どちらかが終わったらもう片方も止める(cleanup が EXIT で止める)
status=0
wait -n "${FAKECORD_PID}" "${NOTH_PID}" || status=$?
if ! kill -0 "${FAKECORD_PID}" 2>/dev/null; then
    echo "警告: fakecord が終了したため、noth も止めます" >&2
elif ! kill -0 "${NOTH_PID}" 2>/dev/null; then
    echo "noth が終了したため、fakecord も止めます" >&2
fi
exit "${status}"