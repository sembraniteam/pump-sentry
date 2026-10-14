#!/usr/bin/env bash
#
# pump-sentry uninstaller (Linux & macOS).
#
#   bash uninstall.sh          # remove the binary, keep ~/.pump-sentry state
#   bash uninstall.sh --purge  # also remove ~/.pump-sentry (logs, WAL, snapshots)

set -euo pipefail

APP="pump-sentry"
[ -n "${HOME:-}" ] || { printf 'error: HOME is unset\n' >&2; exit 1; }
STATE_DIR="$HOME/.pump-sentry"

log() { printf '%s\n' "$*"; }

if [ "${1:-}" = "--purge" ]; then
    PURGE=1
elif [ -n "${1:-}" ]; then
    printf 'error: unknown argument %s (usage: uninstall.sh [--purge])\n' "$1" >&2
    exit 1
else
    PURGE=0
fi

for bin in "${INSTALL_DIR:-/usr/local/bin}/$APP" \
           /usr/local/bin/"$APP" \
           "$HOME"/.local/bin/"$APP" \
           "$HOME"/.pump-sentry/bin/"$APP"; do
    if [ -f "$bin" ]; then
        rm -f "$bin"
        log "removed $bin"
    fi
done

if [ "$PURGE" -eq 1 ]; then
    rm -rf "$STATE_DIR"
    log "purged $STATE_DIR"
else
    log "kept state in $STATE_DIR (uninstall.sh --purge to remove)"
fi
log "uninstalled $APP"
