#!/bin/sh
# omon-gateway installer — builds from source and installs to PREFIX/bin.
# Usage:
#   sh install.sh                     # from a cloned repo, or via curl pipe
#   curl -fsSL https://raw.githubusercontent.com/trac3r00/omon-gateway/main/install.sh | sh
# Environment:
#   PREFIX      install prefix            (default: $HOME/.local)
#   OMON_SRC    source checkout location  (default: $HOME/.omon/omon-gateway)
#   SKIP_DOCTOR 1 skips the post-install doctor run
set -eu

REPO_URL="https://github.com/trac3r00/omon-gateway.git"
PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"
SRC_DIR="${OMON_SRC:-$HOME/.omon/omon-gateway}"
SKIP_DOCTOR="${SKIP_DOCTOR:-0}"

log() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

if [ -f Cargo.toml ] && grep -q '^name = "omon-gateway"' Cargo.toml 2>/dev/null; then
    SRC="$PWD"
    log "using local checkout at $SRC"
else
    command -v git >/dev/null 2>&1 || die "git is required — install it and re-run"
    if [ -d "$SRC_DIR/.git" ]; then
        log "updating existing checkout at $SRC_DIR"
        git -C "$SRC_DIR" pull --ff-only || die "failed to update $SRC_DIR — fix local changes and re-run"
    else
        log "cloning $REPO_URL to $SRC_DIR"
        git clone "$REPO_URL" "$SRC_DIR" || die "clone failed — check network and repo access"
    fi
    SRC="$SRC_DIR"
fi

if ! command -v cargo >/dev/null 2>&1; then
    die "Rust toolchain not found. Install it with: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
fi

log "building omon-gateway (release)…"
cargo build --release --locked --manifest-path "$SRC/Cargo.toml" || die "build failed"

mkdir -p "$BIN_DIR"
install -m 0755 "$SRC/target/release/omon-gateway" "$BIN_DIR/omon-gateway"
log "installed $BIN_DIR/omon-gateway"
case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) log "note: add $BIN_DIR to your PATH (e.g. export PATH=\"$BIN_DIR:\$PATH\")" ;;
esac

if [ ! -f .env ] && [ ! -f "$HOME/.omon/.env" ]; then
    log "next step: run 'omon-gateway setup' to create your configuration"
fi

if [ "$SKIP_DOCTOR" != "1" ]; then
    log "running preflight doctor (non-fatal)…"
    "$BIN_DIR/omon-gateway" doctor || true
fi

log "done."
