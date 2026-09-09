#!/usr/bin/env bash
# Restart the viewer on a fixed port, against a workspace.
#
# The workspace is the folder holding data/, out/ and projects/ -- never the
# source tree. It defaults to wherever this was run from, which is usually what
# is meant; SWATH_WORKSPACE overrides it. There is no --ui: the frontend is
# found from the binary now, so it cannot be pointed at the wrong copy.
#
# HOST=0.0.0.0 serves the rest of the network, not just this machine. It is not
# the default because there is no password on any of it.
set -u
WORKSPACE=${SWATH_WORKSPACE:-$PWD}
cd "$(dirname "$0")/.."
pkill -x swath >/dev/null 2>&1
sleep 0.3
PORT=${PORT:-8731}
HOST=${HOST:-127.0.0.1}
LOG=${LOG:-/tmp/swath-serve.log}
nohup ./target/release/swath serve --root "$WORKSPACE" --host "$HOST" --port "$PORT" >"$LOG" 2>&1 &
sleep 0.9
echo "http://127.0.0.1:$PORT/   (workspace: $WORKSPACE, log: $LOG)"
grep -m1 '^  network:' "$LOG" || true
