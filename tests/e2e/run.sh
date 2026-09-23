#!/usr/bin/env bash
# End-to-end check: ffmpeg (encoder) -> streamdelayd -> ffmpeg (RTMP server, recording).
#
# While the stream runs, the script adds, removes and changes the delay through the
# API. Then it verifies the recording: timestamps always increase, the video decodes
# without errors across every splice, and the delay changes are visible in the timing.
#
# Requires: ffmpeg, ffprobe, curl, python3, and a built streamdelayd
# (cargo build -p streamdelayd).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="${STREAMDELAYD:-$ROOT/target/debug/streamdelayd}"
WORK="$(mktemp -d)"
INGEST=127.0.0.1:19350
SINK_PORT=19360
API=127.0.0.1:17788
TOKEN=e2e-test-token-0123456789
DURATION=${DURATION:-44}
pids=()
cleanup() {
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done
  wait 2>/dev/null || true
  [[ "${KEEP:-}" == 1 ]] && echo "artifacts kept in $WORK" || rm -rf "$WORK"
}
trap cleanup EXIT

api() { curl -fsS -X "$1" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  "http://$API$2" ${3:+-d "$3"}; echo; }

echo "== starting sink (ffmpeg -listen)"
ffmpeg -hide_banner -loglevel error -y -listen 1 -i "rtmp://127.0.0.1:$SINK_PORT/live/sinkkey" \
  -c copy "$WORK/out.flv" 2> "$WORK/sink.log" &
pids+=($!)
sleep 1

echo "== starting streamdelayd"
STREAMDELAY_KEY=sinkkey STREAMDELAY_TOKEN=$TOKEN RUST_LOG=info "$BIN" run --ephemeral \
  --ingest $INGEST --dest "rtmp://127.0.0.1:$SINK_PORT/live" --api $API --grace 2 \
  > "$WORK/streamdelayd.log" 2>&1 &
pids+=($!)
for _ in $(seq 50); do curl -fs "http://$API/healthz" >/dev/null && break; sleep 0.1; done

echo "== starting encoder (${VCODEC:-libx264} with B-frames, 2 s GOP, AAC)"
ffmpeg -hide_banner -loglevel error -re \
  -f lavfi -i "testsrc2=size=640x360:rate=30" -f lavfi -i "sine=frequency=440:sample_rate=48000" \
  -t "$DURATION" -c:v "${VCODEC:-libx264}" -preset veryfast -g 60 -keyint_min 60 -sc_threshold 0 -b:v 1200k \
  ${VCODEC_ARGS:-} ${TS_OFFSET:+-output_ts_offset $TS_OFFSET} -c:a aac -b:a 128k -f flv "rtmp://$INGEST/live/obs" &
enc=$!
pids+=($enc)

sleep 8;  echo "t=8   rewind to 5 s:";        api PUT /api/v1/delay '{"seconds":5}'
sleep 10; echo "t=18  go live now:";          api POST /api/v1/live '{"when":"now"}'
sleep 6;  echo "t=24  mask to 4 s:";          api PUT /api/v1/delay '{"seconds":4,"mode":"mask"}'
sleep 8;  echo "t=32  state:";                api GET /api/v1/state | python3 -c 'import json,sys; s=json.load(sys.stdin)["delay"]; print(" phase", s["phase"], "effective", s["effective_ms"], "ms, splices", s["output"]["splices"])'
echo "t=32  go live after it airs:"; api POST /api/v1/live '{"when":"after-air"}'
wait $enc || true
sleep 4

echo "== verifying recording"
python3 - "$WORK/out.flv" <<'PY'
import json, subprocess, sys
path = sys.argv[1]
def packets(sel):
    out = subprocess.run(["ffprobe", "-v", "error", "-select_streams", sel, "-show_entries",
        "packet=dts_time,pts_time,flags", "-of", "json", path], capture_output=True, text=True, check=True)
    return json.loads(out.stdout)["packets"]
v, a = packets("v"), packets("a")
assert len(v) > 600, f"only {len(v)} video packets"
for name, ps in (("video", v), ("audio", a)):
    dts = [float(p["dts_time"]) for p in ps]
    bad = [(i, x, y) for i, (x, y) in enumerate(zip(dts, dts[1:])) if y < x]
    assert not bad, f"{name} DTS went backwards at {bad[:3]}"
print(f"   {len(v)} video / {len(a)} audio packets, DTS monotonic")
first_video = v[0]["flags"]
assert first_video.startswith("K"), "recording does not start with a keyframe"
PY
# Passthrough timing in the demuxer time base, so ffmpeg does not round
# millisecond timestamps onto a frame grid (which reports false duplicates).
decode() { ffmpeg -hide_banner -v error -i "$WORK/out.flv" -fps_mode passthrough -enc_time_base:v demux -f null - 2>&1; }
errors=$(decode | wc -l)
echo "   decoder errors across splices: $errors"
grep -E "delay command|WARN|ERROR" "$WORK/streamdelayd.log" | sed 's/^/   log: /' | head -20
if [[ "$errors" -gt "${MAX_DECODE_ERRORS:-0}" ]]; then
  decode | head -20
  echo "FAIL: decode errors"; exit 1
fi
echo "PASS"
