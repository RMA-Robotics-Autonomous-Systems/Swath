#!/usr/bin/env bash
# Launch the desktop shell fully detached, so it outlives the shell that starts it.
#
# Nothing is set in the environment on purpose. WEBKIT_DISABLE_COMPOSITING_MODE
# used to be, and it is what made the shell feel slower than the same page in a
# browser: with compositing off there is no GPU process, so every frame is
# painted by the CPU -- and on a fractionally scaled display GTK3 can only meet
# 1.5x by rendering at 2x, which is six megapixels a frame. Measured on this
# machine, a frame of tiles and track cost 23 ms that way against 14 ms
# composited on X11 and 11 ms composited on Wayland. GDK_BACKEND=x11 is gone for
# the same reason: on Wayland the window is the compositor's own and XWayland's
# copy is not in the way.
set -u
WORKSPACE=${SWATH_WORKSPACE:-$PWD}
cd "$(dirname "$0")/.."
pkill -x swath-app >/dev/null 2>&1
sleep 0.4
: >/tmp/swath-app.log
setsid nohup ./target/release/swath-app "$WORKSPACE" </dev/null >>/tmp/swath-app.log 2>&1 &
disown
sleep 7
grep -m1 'http' /tmp/swath-app.log || echo "no url yet"
