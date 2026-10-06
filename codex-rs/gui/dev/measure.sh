#!/usr/bin/env bash
# Measures codex-gui against the GUI.md §7 budgets with the mock model:
# startup milestones, idle RSS with 1 and 10 tabs, and binary size.
#
# Usage: dev/measure.sh [path/to/codex-gui]   (default: target/release/codex-gui)
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
bin="${1:-${here}/../../target/release/codex-gui}"
work="$(mktemp -d)"
port="${MEASURE_PORT:-18299}"
mkdir -p "$work/home" "$work/project"
echo "hello" > "$work/project/README.md"
python3 "$here/mock_responses.py" --port "$port" --write-config "$work/home" > "$work/mock.log" 2>&1 &
mock_pid=$!
trap 'kill $mock_pid 2>/dev/null || true; rm -rf "$work"' EXIT
sleep 1

steps='[{"wait_ready": 60000}, {"new_thread": "'"$work/project"'"}, {"wait_idle": 30000},
 {"send": "hello"}, {"wait": 300}, {"wait_idle": 30000}, {"wait": 2000}, {"mark": "idle-1-tab"}, {"wait": 3000}'
for i in $(seq 2 10); do
  steps+=', {"new_thread": "'"$work/project"'"}, {"wait_idle": 30000}, {"send": "hello '"$i"'"}, {"wait": 300}, {"wait_idle": 30000}'
done
steps+=', {"wait": 2000}, {"mark": "idle-10-tabs"}, {"wait": 3000}, {"quit": true}]'
echo "$steps" > "$work/script.json"

CODEX_HOME="$work/home" CODEX_GUI_AUTOMATION="$work/script.json" CODEX_GUI_PERF=1 \
  "$bin" > "$work/run.log" 2>&1 &
gui_pid=$!

sample_rss() { ps -o rss= -p "$gui_pid" | awk '{printf "%.1f MiB", $1/1024}'; }
seen=""
while kill -0 "$gui_pid" 2>/dev/null; do
  for mark in idle-1-tab idle-10-tabs; do
    if [[ "$seen" != *"$mark"* ]] && grep -q "perf: $mark" "$work/run.log"; then
      sleep 1
      echo "RSS at $mark: $(sample_rss)"
      seen+=" $mark"
    fi
  done
  sleep 0.2
done
wait "$gui_pid" || true

echo "--- milestones"
grep "codex-gui perf:" "$work/run.log" | sed 's/codex-gui perf: //'
size="$(stat -f%z "$bin" 2>/dev/null || stat -c%s "$bin")"
echo "--- binary size: $(awk -v s="$size" 'BEGIN{printf "%.1f MiB", s/1048576}') (unstripped)"
