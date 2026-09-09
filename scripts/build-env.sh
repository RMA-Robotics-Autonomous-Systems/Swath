# Toolchain and system-library paths for building the desktop shell.
#
# Ubuntu's -dev packages for webkit2gtk are extracted into a user-owned sysroot
# rather than installed, because this machine has no root. Headers come from
# there; the shared libraries they link against are the real ones in /usr/lib,
# reached through symlinks planted in the sysroot. Nothing here is needed to
# build `swath` itself -- only `swath-app`, which links the webview.
#
#   source scripts/build-env.sh && cargo build --release -p swath-app
SR="$HOME/.local/sysroot"
export PATH="$HOME/.cargo/bin:$HOME/.local/opt/node/bin:$SR/usr/bin:$PATH"
export LD_LIBRARY_PATH="$SR/usr/lib/x86_64-linux-gnu${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export PKG_CONFIG_PATH="$SR/usr/lib/x86_64-linux-gnu/pkgconfig:$SR/usr/share/pkgconfig"
export PKG_CONFIG_SYSROOT_DIR="$SR"
export LIBRARY_PATH="$SR/usr/lib/x86_64-linux-gnu:/usr/lib/x86_64-linux-gnu"
