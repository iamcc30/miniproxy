#!/bin/bash
# SABR 补拉端到端验证：确认「下载时自动补拉」能把只抓到一部分的视频补成完整片。
#
#   用法: bash verify-sabr-refetch.sh [上游地址,默认 127.0.0.1:7890] [输出目录]
#
# 前置：MiniProxy 在 9000/34567 运行；/api/videos 里有 kind=sabr 的条目
#       （即浏览器里播过一次、抓到了 init 段）。
# 判定：产物 ffprobe 时长应逼近 durationSec，远大于补拉前的 capturedSec。
set -u

API=http://127.0.0.1:9000
UP=${1:-127.0.0.1:7890}
OUT=${2:-/tmp/sabr-verify}
FFPROBE=/opt/homebrew/bin/ffprobe

mkdir -p "$OUT"

echo "== 1/4 探上游 =="
code=$(curl --noproxy '*' -s -o /dev/null -w '%{http_code}' -m 8 -x "http://$UP" https://www.youtube.com/)
echo "   $UP -> $code"
if [ "$code" != "200" ]; then
  echo "   上游对 YouTube 不通（注意：能通百度不代表能通谷歌），等代理节点恢复后重跑。"
  exit 1
fi

echo "== 2/4 取视频列表 =="
curl --noproxy '*' -s -m 20 "$API/api/videos" -o "$OUT/videos.json"
ID=$(grep -o '"entryId": *[0-9]*' "$OUT/videos.json" | head -1 | grep -o '[0-9]*$')
BEFORE=$(grep -o '"capturedSec": *[0-9.]*' "$OUT/videos.json" | head -1 | grep -o '[0-9.]*$')
FULL=$(grep -o '"durationSec": *[0-9.]*' "$OUT/videos.json" | head -1 | grep -o '[0-9.]*$')
if [ -z "${ID:-}" ]; then
  echo "   列表里没有 sabr 条目 —— 先在浏览器里从头播一次目标视频以拿到 init 段。"
  exit 1
fi
echo "   entryId=$ID  补拉前已抓 ${BEFORE}s  全长 ${FULL}s"
if command -v awk >/dev/null; then
  awk -v b="${BEFORE:-0}" -v f="${FULL:-1}" 'BEGIN{printf "   补拉前覆盖 %.1f%%\n",(b/f)*100}'
fi

echo "== 3/4 下载（会自动补拉缺口，缺口大时可能 1-3 分钟）=="
start=$(date +%s)
code=$(curl --noproxy '*' -s --max-time 1800 -w '%{http_code}' \
            -o "$OUT/out.bin" "$API/api/entries/$ID/umpsave")
echo "   HTTP $code, $(ls -lh "$OUT/out.bin" 2>/dev/null | awk '{print $5}'), 用时 $(( $(date +%s) - start ))s"
if [ "$code" != "200" ]; then
  echo "   下载失败，前端会显示具体原因；后端日志里有 [umpsave] SABR 补拉 的统计。"
  head -c 300 "$OUT/out.bin"; echo; exit 1
fi

echo "== 4/4 校验 =="
# 容器按魔数判断（grep 直接匹配二进制魔数不可靠，用 xxd/dd）
ext=""
if [ "$(head -c4 "$OUT/out.bin" | xxd -p)" = "1a45dfa3" ]; then
  ext=webm
elif [ "$(dd if="$OUT/out.bin" bs=1 skip=4 count=4 2>/dev/null)" = "ftyp" ]; then
  ext=mp4
else
  ext=mkv   # AV1/VP9 + opus 之类只能装 mkv
fi
mv "$OUT/out.bin" "$OUT/out.$ext"
echo "   容器 .$ext"

if [ -x "$FFPROBE" ]; then
  "$FFPROBE" -v error -show_entries 'stream=codec_type,codec_name,width,height' \
             -show_entries format=duration -of default=nw=1 "$OUT/out.$ext"
  got=$("$FFPROBE" -v error -show_entries format=duration -of csv=p=0 "$OUT/out.$ext")
  awk -v g="${got:-0}" -v f="${FULL:-1}" -v b="${BEFORE:-0}" 'BEGIN{
    printf "   补拉前覆盖 %.1f%%, 补拉后 %.1f%%\n", (b/f)*100, (g/f)*100;
    if (g > f*0.95) print "   ✅ 已补成完整片";
    else print "   ⚠️ 仍有缺口：查后端日志 [umpsave] SABR 补拉 的结束原因";
  }'
else
  echo "   未找到 ffprobe（$FFPROBE），跳过时长校验"
fi
echo "产物: $OUT/out.$ext"
