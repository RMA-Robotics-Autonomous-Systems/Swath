#!/usr/bin/env bash
# Restart the viewer on a fixed port, against a workspace.
#
# The workspace is the folder holding data/, out/ and projects/ -- never the
# source tree. It defaults to wherever this was run from, which is usually what
# is meant; SWATH_WORKSPACE overrides it. There is no --ui: the frontend is
# found from the binary now, so it cannot be pointed at the wrong copy.
set -u
WORKSPACE=${SWATH_WORKSPACE:-$PWD}
cd "$(dirname "$0")/.."
pkill -x swath >/dev/null 2>&1
sleep 0.3
PORT=${PORT:-8731}
LOG=${LOG:-/tmp/swath-serve.log}
nohup ./target/release/swath serve --root "$WORKSPACE" --port "$PORT" >"$LOG" 2>&1 &
sleep 0.9
echo "http://127.0.0.1:$PORT/   (workspace: $WORKSPACE, log: $LOG)"
