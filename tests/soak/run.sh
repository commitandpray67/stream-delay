#!/usr/bin/env bash
# Soak test: stream through streamdelayd for a long time while changing the delay
# and dumping the buffer at random intervals, then check that nothing degraded.
# The encoder also crashes every RESTART_MIN to RESTART_MAX seconds and comes
# back within the grace period, as a new session with its own metadata and codec
# headers: the broadcast goes on, and what each session leaves must not pile up.
#
#   DURATION=43200 tests/soak/run.sh      # the 12-hour run from docs/PLAN.md
#   DURATION=1800 MIN_GAP=20 MAX_GAP=60 RESTART_MIN=60 RESTART_MAX=180 tests/soak/run.sh
#                                         # a quick local run
#
# Checks at the end:
#   - streamdelayd is still running and never reconnected to the destination,
#     and took up every encoder session;
#   - the receiving side decoded everything without errors;
#   - memory stayed flat after the buffer filled: total RSS spread below
#     MAX_GROWTH_MB, and what the process holds beyond the buffered data no
#     higher in the last third of the run than in the middle third by more than
#     MAX_TREND_MB (catches slow leaks that a short spread hides; RSS itself
#     follows how full the buffer is, since freed buffer memory goes back to
#     the system);
#   - output timestamps kept increasing (checked by the decoder in DTS order).
#
# Samples are written to $OUT/samples.csv (time, rss_kb, phase, effective_ms,
# buffered_bytes, splices) so a run can be graphed afterwards.
#
# Requires: ffmpeg, curl, python3 and a built streamdelayd (release recommended).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="${STREAMDELAYD:-$ROOT/target/release/streamdelayd}"
DURATION=${DURATION:-43200}
MIN_GAP=${MIN_GAP:-60}
MAX_GAP=${MAX_GAP:-300}
MAX_DELAY=${MAX_DELAY:-60}
MAX_GROWTH_MB=${MAX_GROWTH_MB:-40}
MAX_TREND_MB=${MAX_TREND_MB:-8}
RESTART_MIN=${RESTART_MIN:-120}
RESTART_MAX=${RESTART_MAX:-600}
OUT=${OUT:-$(mktemp -d)}
# Override the ports to run several soaks side by side.
INGEST=127.0.0.1:${INGEST_PORT:-19450}
SINK_PORT=${SINK_PORT:-19460}
API=127.0.0.1:${API_PORT:-17888}
TOKEN=soak-test-token-0123456789
mkdir -p "$OUT"
pids=()
cleanup() {
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done
  wait 2>/dev/null || true
}
trap cleanup EXIT

api() {
  curl -fsS -X "$1" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    "http://$API$2" ${3:+-d "$3"}
}

[[ -x "$BIN" ]] || { echo "streamdelayd not found at $BIN (cargo build --release -p streamdelayd)"; exit 1; }
echo "soak: ${DURATION}s, delay changes every ${MIN_GAP}-${MAX_GAP}s, output in $OUT"

# The sink decodes everything; any decoder complaint lands in sink.log.
ffmpeg -hide_banner -nostats -loglevel error -listen 1 -i "rtmp://127.0.0.1:$SINK_PORT/live/soak" \
  -fps_mode passthrough -enc_time_base:v demux -f null - 2> "$OUT/sink.log" &
pids+=($!)
sleep 1

STREAMDELAY_KEY=soak STREAMDELAY_TOKEN=$TOKEN RUST_LOG=info "$BIN" run --ephemeral \
  --ingest $INGEST --dest "rtmp://127.0.0.1:$SINK_PORT/live" --api $API \
  --max-delay "$MAX_DELAY" --grace 5 > "$OUT/streamdelayd.log" 2>&1 &
SD=$!
pids+=($SD)
for _ in $(seq 50); do curl -fs "http://$API/healthz" >/dev/null && break; sleep 0.1; done

# Becomes ffmpeg, so its PID is the one to kill.
encode() {
  exec ffmpeg -hide_banner -nostats -loglevel error -re \
    -f lavfi -i "testsrc2=size=1280x720:rate=30" -f lavfi -i "sine=frequency=440:sample_rate=48000" \
    -t "$1" -c:v libx264 -preset veryfast -g 60 -keyint_min 60 -sc_threshold 0 -b:v 2500k \
    -c:a aac -b:a 128k -f flv "rtmp://$INGEST/live/obs" 2>> "$OUT/encoder.log"
}
# Streams until the end of the soak, which the last session reaches and stops
# at cleanly. The others are killed like a crashed encoder (no unpublish), and
# the next one starts within the grace period.
encoder_sessions() {
  local end=$(( $(date +%s) + DURATION )) left run f=
  # Waiting in `wait` (not in a foreground command) lets this stop at once.
  trap '[[ -n "$f" ]] && kill "$f" 2>/dev/null; exit' TERM
  while left=$(( end - $(date +%s) )); (( left > 0 )); do
    run=$(( RESTART_MIN + RANDOM % (RESTART_MAX - RESTART_MIN + 1) ))
    encode "$left" &
    f=$!
    if (( run >= left )); then
      wait "$f"
      return
    fi
    sleep "$run" &
    wait $!
    kill -9 "$f" 2>/dev/null || true
    wait "$f" 2>/dev/null || true
    echo x >> "$OUT/restarts"
    sleep $(( 1 + RANDOM % 2 ))
  done
}
: > "$OUT/restarts"
encoder_sessions &
ENC=$!
pids+=($ENC)

echo "time_s,rss_kb,phase,effective_ms,buffered_bytes,splices" > "$OUT/samples.csv"
sample() {
  local rss state
  rss=$(awk '/VmRSS/ {print $2}' "/proc/$SD/status" 2>/dev/null || echo 0)
  state=$(api GET /api/v1/state 2>/dev/null | python3 -c 'import json,sys
d=json.load(sys.stdin)["delay"]
print(d["phase"], d["effective_ms"], d["buffered_bytes"], d["output"]["splices"], sep=",")' 2>/dev/null || echo "error,0,0,0")
  echo "$(( $(date +%s) - START )),$rss,$state" >> "$OUT/samples.csv"
}

START=$(date +%s)
next_change=$(( START + MIN_GAP ))
changes=0
while kill -0 "$ENC" 2>/dev/null; do
  now=$(date +%s)
  sample
  if (( now >= next_change )); then
    r=$(( RANDOM % 8 ))
    secs=$(( 1 + RANDOM % (MAX_DELAY - 1) ))
    case $r in
      0|1) body="{\"seconds\":$secs}"; path=/api/v1/delay; method=PUT ;;
      2)   body="{\"seconds\":$(( 1 + secs / 3 )),\"mode\":\"mask\"}"; path=/api/v1/delay; method=PUT ;;
      3)   body='{"when":"now"}'; path=/api/v1/live; method=POST ;;
      4)   body='{"when":"after-air"}'; path=/api/v1/live; method=POST ;;
      5)   body="{\"seconds\":$(( secs / 2 ))}"; path=/api/v1/delay; method=PUT ;;
      # Refused (400) while there is no delay: logged below, not a failure.
      6)   body='{"mode":"rewind"}'; path=/api/v1/stream/dump; method=POST ;;
      7)   body='{"mode":"mask"}'; path=/api/v1/stream/dump; method=POST ;;
    esac
    api "$method" "$path" "$body" > /dev/null || echo "  change failed: $method $path $body"
    changes=$(( changes + 1 ))
    next_change=$(( now + MIN_GAP + RANDOM % (MAX_GAP - MIN_GAP + 1) ))
  fi
  if ! kill -0 "$SD" 2>/dev/null; then
    echo "FAIL: streamdelayd exited"; tail -20 "$OUT/streamdelayd.log"; exit 1
  fi
  sleep 10
done
sleep 3
sample

echo "soak finished after $(( $(date +%s) - START ))s with $changes delay changes and $(wc -l < "$OUT/restarts") encoder crashes"
python3 - "$OUT" "$MAX_GROWTH_MB" "$MAX_TREND_MB" <<'PY'
import csv, statistics, sys
out, max_growth, max_trend = sys.argv[1], float(sys.argv[2]), float(sys.argv[3])
rows = [r for r in csv.DictReader(open(f"{out}/samples.csv")) if r["phase"] != "error"]
rows = [r for r in rows if int(r["rss_kb"]) > 0]
# Ignore the first 20% while the buffer fills up to the maximum delay.
warm = rows[len(rows) // 5:]
rss = [int(r["rss_kb"]) for r in warm]
# What the process holds beyond the buffered data, in KiB.
beyond = [int(r["rss_kb"]) - int(r["buffered_bytes"]) // 1024 for r in warm]
growth = (max(rss) - min(rss)) / 1024
splices = int(rows[-1]["splices"]) if rows else 0
print(f"  RSS after warm-up: min {min(rss)/1024:.1f} MB, max {max(rss)/1024:.1f} MB, spread {growth:.1f} MB")
third = len(warm) // 3
def rise(v):
    return (statistics.median(v[2 * third:]) - statistics.median(v[third:2 * third])) / 1024
trend = rise(beyond)
print(f"  beyond the buffered data, median, last third vs middle third: {trend:+.1f} MB"
      f" (RSS itself: {rise(rss):+.1f} MB)")
print(f"  splices performed: {splices}")
if growth > max_growth:
    sys.exit(f"FAIL: memory grew by {growth:.1f} MB (limit {max_growth} MB)")
if trend > max_trend:
    sys.exit(f"FAIL: memory still rising late in the run ({trend:+.1f} MB, limit {max_trend} MB)")
PY
reconnects=$(api GET /api/v1/state | python3 -c 'import json,sys; print(json.load(sys.stdin)["egress"]["reconnects"])')
# ffmpeg reports the end of the connection as an I/O error; that happens when
# the relay ends the broadcast after the encoder stops (already once the delay
# has aired, if it is short). Dropped connections show up as reconnects instead.
errors=$(grep -v 'Input/output error' "$OUT/sink.log" | grep -cv '^\s*$' || true)
# Every session after a crash must be taken up, continuing the broadcast.
rejected=$(grep -c "rejected publish" "$OUT/streamdelayd.log" || true)
echo "  destination reconnects: $reconnects, decoder errors: $errors, encoder sessions refused: $rejected"
if [[ "$rejected" != 0 ]]; then grep "rejected publish" "$OUT/streamdelayd.log" | head -5; echo "FAIL: an encoder session was refused"; exit 1; fi
if [[ "$reconnects" != 0 ]]; then echo "FAIL: the destination connection dropped"; exit 1; fi
if [[ "$errors" != 0 ]]; then head -20 "$OUT/sink.log"; echo "FAIL: decode errors"; exit 1; fi
echo "PASS (samples in $OUT/samples.csv)"
