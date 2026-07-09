#!/usr/bin/env bash
# spike-plan.md SPIKE-05 検証手順3相当のソークテスト(短縮版)。
# Xvfb上でTauriバイナリを起動し、
#   - SIGUSR1でウィンドウのhide/showを模擬した「閉鎖後も録音継続」の確認
#   - 一定間隔でのRSSサンプリングによるメモリ安定性の確認
# を自動化する。
#
# 【既知の制約(2026-07-09)】この開発コンテナではWebKitGTKのWebView初期化が
# 完了せずハングする(virtio-gpuにMesaドライバがbindされておらず`driver (null)`、
# WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS/WEBKIT_DISABLE_COMPOSITING_MODE/
# WEBKIT_DISABLE_DMABUF_RENDERER/LIBGL_ALWAYS_SOFTWARE等を試しても解消せず、
# Tauriアプリ単体でなくwebkit2gtk本体のMiniBrowserでも同一環境下で同様に
# ハングすることを確認済み)。実行にはWebKitGTKが実際に動くLinuxデスクトップ
# (実GPU、またはvirglやSPICE/QXL越しの3Dアクセラレーション付きVM等)が必要。
set -euo pipefail

BIN="$1"
DURATION_SECONDS="${2:-120}"
SAMPLE_INTERVAL="${3:-5}"
OUT_DIR="${4:-/tmp/spike05-soak}"
BIN_BASENAME="$(basename "$BIN")"

mkdir -p "$OUT_DIR"
export SPIKE05_LOG_PATH="$OUT_DIR/level_meter_log.jsonl"
rm -f "$SPIKE05_LOG_PATH"
RSS_CSV="$OUT_DIR/rss_samples.csv"
echo "elapsed_s,rss_kb,window_state" > "$RSS_CSV"

xvfb-run -a --server-args="-screen 0 1280x1024x24" "$BIN" &
WRAPPER_PID=$!
echo "launched xvfb-run wrapper pid=$WRAPPER_PID, logging to $OUT_DIR"

# xvfb-runはラッパー(exec しない)なので、実バイナリは別pidの子プロセスになる。
# RSS計測・シグナル送信は実バイナリのpidに対して行う必要がある。
PID=""
for _ in $(seq 1 50); do
  PID=$(pgrep -f "^${BIN}$" | head -1 || true)
  if [ -n "$PID" ]; then
    break
  fi
  sleep 0.2
done
if [ -z "$PID" ]; then
  echo "failed to locate child process for $BIN" >&2
  kill "$WRAPPER_PID" 2>/dev/null || true
  exit 1
fi
echo "actual binary pid=$PID"

START=$(date +%s)
WINDOW_STATE="shown"
HIDE_AT=$((DURATION_SECONDS / 3))
SHOW_AT=$((DURATION_SECONDS * 2 / 3))
toggled_hide=0
toggled_show=0

while true; do
  NOW=$(date +%s)
  ELAPSED=$((NOW - START))
  if [ "$ELAPSED" -ge "$DURATION_SECONDS" ]; then
    break
  fi

  if ! kill -0 "$PID" 2>/dev/null; then
    echo "process exited unexpectedly at elapsed=${ELAPSED}s" >&2
    exit 1
  fi

  if [ "$ELAPSED" -ge "$HIDE_AT" ] && [ "$toggled_hide" -eq 0 ]; then
    kill -USR1 "$PID"
    toggled_hide=1
    WINDOW_STATE="hidden"
    echo "sent SIGUSR1 (hide) at elapsed=${ELAPSED}s"
  fi
  if [ "$ELAPSED" -ge "$SHOW_AT" ] && [ "$toggled_show" -eq 0 ]; then
    kill -USR1 "$PID"
    toggled_show=1
    WINDOW_STATE="shown"
    echo "sent SIGUSR1 (show) at elapsed=${ELAPSED}s"
  fi

  RSS=$(ps -o rss= -p "$PID" 2>/dev/null | tr -d ' ' || echo "")
  if [ -n "$RSS" ]; then
    echo "${ELAPSED},${RSS},${WINDOW_STATE}" >> "$RSS_CSV"
  fi

  sleep "$SAMPLE_INTERVAL"
done

kill "$PID" 2>/dev/null || true
kill "$WRAPPER_PID" 2>/dev/null || true
wait "$WRAPPER_PID" 2>/dev/null || true
echo "soak test complete. results in $OUT_DIR"
