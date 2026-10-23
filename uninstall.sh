#!/usr/bin/env bash

set -euo pipefail

APP="pump-sentry"
[ -n "${HOME:-}" ] || { printf 'error: HOME is unset\n' >&2; exit 1; }
STATE_DIR="$HOME/.pump-sentry"

log() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

print_help() {
    cat <<'USAGE'
usage: uninstall.sh [--purge] [--force]

  (default)   remove the binary, keep ~/.pump-sentry state
  --purge     also remove ~/.pump-sentry (logs, WAL, snapshots)
  --force     proceed even when a daemon is running; the daemon keeps
              running from its open file handles until it restarts.
              Prefer `pump-sentry stop` first.
  -h, --help  this text
USAGE
}

# Default arg parsing; every argument is accounted for.
PURGE=0
FORCE=0
for arg in "$@"; do
    case "$arg" in
        -h|--help) print_help; exit 0 ;;
        --purge) PURGE=1 ;;
        --force) FORCE=1 ;;
        *) die "unknown argument '$arg' (supported: --purge, --force)" ;;
    esac
done

# The daemon writes WAL/journal/day-state under STATE_DIR for its whole
# lifetime: deleting or purging under it destroys live custody data.
# Default identity file is $STATE_DIR/bot.pid (a LOG_FILE/PID_FILE env
# override in the daemon's own environment is not visible here — this
# check is best effort, and `--force` is the explicit override).
daemon_running() {
    local pid
    pid="$(grep -oE '"pid"[[:space:]]*:[[:space:]]*[0-9]+' \
        "$STATE_DIR/bot.pid" 2>/dev/null \
        | grep -oE '[0-9]+' | head -1 || true)"
    [ -n "$pid" ] || return 1
    kill -0 "$pid" 2>/dev/null || return 1
    return 0
}

if [ "$FORCE" -eq 0 ] && daemon_running; then
    RUN_PID="$(grep -oE '"pid"[[:space:]]*:[[:space:]]*[0-9]+' "$STATE_DIR/bot.pid" \
        | grep -oE '[0-9]+' | head -1)"
    die "a $APP daemon is running (pid ${RUN_PID:?}); run '$APP stop' first, or pass --force"
fi
if [ "$FORCE" -eq 1 ] && daemon_running; then
    log "warning: daemon still running; removing a live install (state may be rewritten until it stops)"
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
