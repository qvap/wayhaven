#!/usr/bin/env bash
# Build wayhaven and (re)start it with the new binary.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

cargo build --release
BIN="$HOME/.local/bin/wayhaven"
mkdir -p "$(dirname "$BIN")"

running=0
if "$BIN" ping &>/dev/null; then
    running=1
    "$BIN" quit >/dev/null
fi
install -m755 target/release/wayhaven "$BIN"
echo "installed $BIN"

if [[ $running == 1 ]]; then
    setsid -f "$BIN" daemon >"${XDG_RUNTIME_DIR:-/tmp}/wayhaven.log" 2>&1
    echo "daemon restarted"
fi
