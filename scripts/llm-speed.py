#!/usr/bin/env python3
"""OpenCode のセッション記録から LLM の速度を測る。

使い方:
  # 同じプロンプトを N 回投げて単発の速度を測る
  scripts/llm-speed.py bench --model opencode/beam-free -n 3

  # このディレクトリで動いたセッション(Orca ワーカーなど)を集計する
  scripts/llm-speed.py sessions --model beam-free --since "2026-10-17 09:00"
  scripts/llm-speed.py sessions ses_xxx ses_yyy --json

指標(LLM 呼び出し = assistant メッセージ 1 つ):
  TTFT     呼び出し開始から最初の出力(reasoning / text / tool)までの時間
  decode   (output + reasoning トークン) / (最初の出力から完了まで)
  e2e      (output + reasoning トークン) / (呼び出し開始から完了まで)
  llm%     セッションの経過時間のうち LLM の生成が占める割合
  tool%    セッションの経過時間のうちツール実行(cargo など)が占める割合。残りは待ちや通信
"""

import argparse
import json
import statistics
import subprocess
import sys
import tempfile
from datetime import datetime

BENCH_PROMPT = (
    "Rust で、標準ライブラリだけを使う LRU キャッシュ(容量指定、get/put、"
    "HashMap と双方向リストで O(1))を 1 ファイルで実装し、テストも 3 つ書いてください。"
    "ファイルには書き出さず、コードを回答本文にそのまま出力してください。"
)


def opencode(args, cwd=None):
    out = subprocess.run(
        ["opencode", *args], cwd=cwd, capture_output=True, text=True, check=True
    )
    return out.stdout


def export(session_id, cwd=None):
    # パイプに書かせると、大きなセッションは opencode が書き切る前に終了して途中で切れる。
    # ファイルへリダイレクトすると最後まで書かれる
    with tempfile.TemporaryFile("w+") as f:
        subprocess.run(
            ["opencode", "session", "export", session_id], cwd=cwd, stdout=f, check=True
        )
        f.seek(0)
        return json.load(f)


def first_output_ms(msg):
    times = [
        c["time"]["created"]
        for c in msg.get("content", [])
        if isinstance(c.get("time"), dict) and "created" in c["time"]
    ]
    return min(times) if times else msg["time"].get("streamed")


def model_name(info):
    m = info.get("model") or {}
    return f'{m.get("providerID", "?")}/{m.get("id", "?")}'


def analyze(data):
    info = data["info"]
    calls = []
    for m in data["messages"]:
        if m.get("type") != "assistant" or "completed" not in m.get("time", {}):
            continue
        t = m["time"]
        tok = m.get("tokens", {})
        out = tok.get("output", 0) + tok.get("reasoning", 0)
        first = first_output_ms(m) or t["created"]
        # completed にはメッセージ内で走ったツールの実行時間も含まれる。生成の終わりは streamed
        end = t.get("streamed") or t["completed"]
        tool_ms = sum(
            c["time"]["completed"] - c["time"]["ran"]
            for c in m.get("content", [])
            if c.get("type") == "tool" and {"ran", "completed"} <= set(c.get("time") or {})
        )
        calls.append(
            {
                "ttft_ms": max(first - t["created"], 0),
                "gen_ms": max(end - first, 1),
                "total_ms": max(end - t["created"], 1),
                "tool_ms": tool_ms,
                "out_tokens": out,
                "in_tokens": tok.get("input", 0) + tok.get("cache", {}).get("read", 0),
            }
        )
    created = info["time"]["created"]
    ended = info["time"].get("idle") or info["time"]["updated"]
    wall = max(ended - created, 1)
    out_sum = sum(c["out_tokens"] for c in calls)
    gen_sum = sum(c["gen_ms"] for c in calls)
    llm_sum = sum(c["total_ms"] for c in calls)
    return {
        "id": info["id"],
        "title": info.get("title", ""),
        "model": model_name(info),
        "outcome": info.get("outcome"),
        "started": datetime.fromtimestamp(created / 1000).isoformat(timespec="seconds"),
        "wall_s": wall / 1000,
        "calls": len(calls),
        "out_tokens": out_sum,
        "in_tokens": sum(c["in_tokens"] for c in calls),
        "ttft_median_s": statistics.median([c["ttft_ms"] for c in calls]) / 1000
        if calls
        else None,
        "decode_tps": out_sum / (gen_sum / 1000) if calls else None,
        "e2e_tps": out_sum / (llm_sum / 1000) if calls else None,
        "llm_share": llm_sum / wall,
        "tool_share": sum(c["tool_ms"] for c in calls) / wall,
    }


def fmt(v, spec):
    return "-" if v is None else format(v, spec)


def print_table(rows):
    print(
        f'{"session":<32} {"model":<34} {"wall":>7} {"calls":>5} {"out_tok":>8} '
        f'{"ttft":>6} {"decode":>7} {"e2e":>7} {"llm%":>5} {"tool%":>6}  title'
    )
    for r in rows:
        print(
            f'{r["id"]:<32} {r["model"]:<34} {fmt(r["wall_s"], ".0f"):>6}s {r["calls"]:>5} '
            f'{r["out_tokens"]:>8} {fmt(r["ttft_median_s"], ".2f"):>5}s '
            f'{fmt(r["decode_tps"], ".0f"):>7} {fmt(r["e2e_tps"], ".0f"):>7} '
            f'{r["llm_share"] * 100:>4.0f}% {r["tool_share"] * 100:>5.0f}%  {r["title"][:40]}'
        )
    if len(rows) > 1:
        out = sum(r["out_tokens"] for r in rows)
        decode = [r["decode_tps"] for r in rows if r["decode_tps"]]
        e2e = [r["e2e_tps"] for r in rows if r["e2e_tps"]]
        print(
            f'合計 {len(rows)} セッション、出力 {out} トークン、'
            f'decode 中央値 {fmt(statistics.median(decode) if decode else None, ".0f")} tok/s、'
            f'e2e 中央値 {fmt(statistics.median(e2e) if e2e else None, ".0f")} tok/s'
        )


def cmd_bench(a):
    rows = []
    # ファイル操作をさせないよう、空の一時ディレクトリで走らせる
    with tempfile.TemporaryDirectory() as tmp:
        for i in range(a.n):
            events = opencode(
                ["run", "-m", a.model, "--format", "json", "--title", f"llm-speed bench {i + 1}", a.prompt],
                cwd=tmp,
            )
            sid = next(
                json.loads(line)["sessionID"]
                for line in events.splitlines()
                if line.startswith("{")
            )
            rows.append(analyze(export(sid, cwd=tmp)))
            print(f"bench {i + 1}/{a.n} 完了", file=sys.stderr)
    emit(rows, a.json)


def cmd_sessions(a):
    ids = a.ids
    if not ids:
        listed = json.loads(opencode(["session", "list", "--format", "json", "-n", str(a.limit)]))
        ids = [s["id"] for s in listed]
    since = datetime.fromisoformat(a.since).timestamp() * 1000 if a.since else None
    rows = []
    for sid in ids:
        data = export(sid)
        info = data["info"]
        if since and info["time"]["created"] < since:
            continue
        if a.model and a.model not in model_name(info):
            continue
        rows.append(analyze(data))
    emit(rows, a.json)


def emit(rows, as_json):
    if as_json:
        print(json.dumps(rows, ensure_ascii=False, indent=2))
    else:
        print_table(rows)


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)

    b = sub.add_parser("bench", help="同じプロンプトを N 回投げて測る")
    b.add_argument("--model", "-m", default="opencode/beam-free")
    b.add_argument("-n", type=int, default=3)
    b.add_argument("--prompt", default=BENCH_PROMPT)
    b.add_argument("--json", action="store_true")
    b.set_defaults(func=cmd_bench)

    s = sub.add_parser("sessions", help="既存セッションを集計する(既定はカレントのプロジェクト)")
    s.add_argument("ids", nargs="*", help="セッション ID。省略時は session list から取る")
    s.add_argument("--model", help="provider/model の部分一致で絞る(例: beam-free)")
    s.add_argument("--since", help="この時刻以降に作られたものだけ(例: '2026-10-17 09:00')")
    s.add_argument("--limit", type=int, default=100)
    s.add_argument("--json", action="store_true")
    s.set_defaults(func=cmd_sessions)

    a = p.parse_args()
    a.func(a)


if __name__ == "__main__":
    main()
