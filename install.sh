#!/usr/bin/env bash
#
# pump-sentry installer (Linux & macOS). Just run it — it installs.
#
#   curl -fsSL https://raw.githubusercontent.com/sembraniteam/pump-sentry/main/install.sh | bash
#
# Pinned version:
#   curl -fsSL .../install.sh | VERSION=vX.Y.Z bash
#
# Build from source instead of a release binary:
#   curl -fsSL .../install.sh | bash -s -- --from-source
#
# Skip checksum verification (refused by default):
#   curl -fsSL .../install.sh | bash -s -- --allow-unverified
#
# Knobs (env):
#   VERSION      release tag (default: latest)
#   INSTALL_DIR  binary destination (default: /usr/local/bin when writable,
#                else $HOME/.local/bin)

set -euo pipefail

APP="pump-sentry"
REPO="sembraniteam/pump-sentry"
[ -n "${HOME:-}" ] || die "HOME is unset"

log() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

command -v curl >/dev/null 2>&1 || die "need 'curl'"
command -v tar >/dev/null 2>&1 || die "need 'tar'"

case "$(uname -s | tr '[:upper:]' '[:lower:]')" in
    linux)  OS="linux" ;;
    darwin) OS="darwin" ;;
    *)      die "unsupported OS: $(uname -s) (linux/darwin only)" ;;
esac
case "$(uname -m)" in
    x86_64|amd64)   ARCH="x86_64" ;;
    arm64|aarch64)  ARCH="arm64" ;;
    *)              die "unsupported arch: $(uname -m) (x86_64/arm64 only)" ;;
esac

# Argument check first: --help / bad flags must not trigger network.
ALLOW_UNVERIFIED=0
FROM_SOURCE=0
for arg in "$@"; do
    case "$arg" in
        -h|--help)
            awk 'NR==1{next} /^#/{sub(/^# ?/,"");print;next} {exit}' "$0"
            exit 0
            ;;
        --from-source) FROM_SOURCE=1 ;;
        --allow-unverified) ALLOW_UNVERIFIED=1 ;;
        *) die "unknown argument '$arg' (supported: --from-source, --allow-unverified)" ;;
    esac
done

if [ -n "${VERSION:-}" ]; then
    TAG="$VERSION"
else
    # Split fetch vs parse: under `pipefail` a failed curl (or a valid
    # response with no releases) would otherwise exit silently with no
    # message — fail loudly instead.
    API_RESP="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
        || die "could not fetch latest release info (network/API down? set VERSION=vX.Y.Z to pin one)")"
    TAG="$(printf '%s\n' "$API_RESP" | grep '"tag_name"' | sed 's/.*"tag_name": *"//;s/".*//' || true)"
    [ -n "$TAG" ] || die "no releases published yet (set VERSION=vX.Y.Z once one exists)"
fi

if [ -n "${INSTALL_DIR:-}" ]; then
    DEST="$INSTALL_DIR"
elif [ -d /usr/local/bin ] && [ -w /usr/local/bin ]; then
    DEST="/usr/local/bin"
else
    DEST="$HOME/.local/bin"
fi

TMP="$(mktemp -d)"
# shellcheck disable=SC2064
trap "rm -rf '$TMP'" EXIT

if [ "$FROM_SOURCE" = "1" ]; then
    command -v git >/dev/null 2>&1 || die "need 'git' for --from-source"
    command -v cargo >/dev/null 2>&1 || die "need 'cargo' for --from-source (https://rustup.rs)"
    log "building $APP $TAG from source (release; takes a while) ..."
    git clone --depth 1 --branch "$TAG" "https://github.com/$REPO.git" "$TMP/src" \
        || die "clone failed (unknown tag '$TAG'?)"
    (cd "$TMP/src" && cargo build --release --locked) \
        || die "source build failed"
    BIN="$TMP/src/target/release/$APP"
else
    ASSET="$APP-$OS-$ARCH.tar.gz"
    BASE="https://github.com/$REPO/releases/download/$TAG"
    log "installing $APP $TAG for $OS/$ARCH ..."
    curl -fsSL --retry 3 -o "$TMP/$ASSET" "$BASE/$ASSET" \
        || die "download failed: $BASE/$ASSET (no asset for $OS/$ARCH yet? try --from-source)"
    if curl -fsSL --retry 3 -o "$TMP/$ASSET.sha256" "$BASE/$ASSET.sha256" 2>/dev/null; then
        WANT="$(awk '{print $1}' "$TMP/$ASSET.sha256")"
        case "$WANT" in
            *[^0-9a-fA-F]*|"") WANT="" ;;
        esac
        if [ -z "$WANT" ] || [ "${#WANT}" -ne 64 ]; then
            [ "$ALLOW_UNVERIFIED" = "1" ] \
                || die "checksum file malformed; refusing unverified install (retry with --allow-unverified to override)"
            log "warning: checksum file malformed; installing UNVERIFIED (--allow-unverified)"
        elif command -v sha256sum >/dev/null 2>&1; then
            HAVE="$(sha256sum "$TMP/$ASSET" | awk '{print $1}')"
            [ "$WANT" = "$HAVE" ] || die "checksum mismatch for $ASSET"
            log "checksum ok"
        elif command -v shasum >/dev/null 2>&1; then
            HAVE="$(shasum -a 256 "$TMP/$ASSET" | awk '{print $1}')"
            [ "$WANT" = "$HAVE" ] || die "checksum mismatch for $ASSET"
            log "checksum ok"
        else
            [ "$ALLOW_UNVERIFIED" = "1" ] \
                || die "no sha256 tool; refusing unverified install (retry with --allow-unverified to override)"
            log "warning: no sha256 tool; installing UNVERIFIED (--allow-unverified)"
        fi
    else
        [ "$ALLOW_UNVERIFIED" = "1" ] \
            || die "no checksum file published; refusing unverified install (retry with --allow-unverified to override)"
        log "warning: no checksum file published; installing UNVERIFIED (--allow-unverified)"
    fi
    # Tar-slip guard: validate member names before extracting (no
    # absolute paths, no `..`, no symlinks — a malicious archive must
    # not write outside $TMP).
    MEMBERS="$(tar -tzf "$TMP/$ASSET" 2>/dev/null)" || die "archive unreadable"
    if printf '%s\n' "$MEMBERS" | grep -E -e '^/' -e '\.\./' >/dev/null; then
        die "archive has unsafe members (absolute path or ..); refusing to install"
    fi
    if tar -tzvf "$TMP/$ASSET" 2>/dev/null | grep -E '^l' >/dev/null; then
        die "archive contains symlinks; refusing to install"
    fi
    tar -xzf "$TMP/$ASSET" -C "$TMP" || die "extract failed"
    [ -f "$TMP/$APP" ] || die "archive has no $APP binary"
    BIN="$TMP/$APP"
fi

mkdir -p "$DEST" || die "cannot create $DEST"
cp "$BIN" "$DEST/$APP" || die "copy to $DEST failed (permissions?)"
chmod +x "$DEST/$APP" || die "chmod failed"
log "installed $DEST/$APP"
case ":$PATH:" in
    *":$DEST:"*) ;;
    *) log "note: $DEST is not on PATH; add: export PATH=\"$DEST:\$PATH\"" ;;
esac

# No --version flag exists; `status` is side-effect free and proves it runs
# (exit 0 running, 1 stopped — anything else is a broken install).
[ -x "$DEST/$APP" ] || die "installed binary missing/not executable"
ST=0
"$DEST/$APP" status >/dev/null 2>&1 || ST=$?
[ "$ST" -eq 0 ] || [ "$ST" -eq 1 ] || die "installed binary failed to run (exit $ST)"
log "smoke test ok"

# Categorized config: best-effort seed (the daemon also auto-creates it
# on first start, so a skipped seed here is never fatal).
if "$DEST/$APP" init-config >/dev/null 2>&1; then
    log "config ready at ~/.pump-sentry/config.toml (edit by category, secrets stay in env)"
else
    log "warning: could not seed ~/.pump-sentry/config.toml (created automatically on first start)"
fi
