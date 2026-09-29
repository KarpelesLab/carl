#!/bin/sh
# Install Carl, the MCP server that gives AI agents hands.
#
#   curl -fsSL https://raw.githubusercontent.com/KarpelesLab/carl/master/install.sh | sh
#
# Downloads the latest static Linux x86_64 build from GitHub Releases, checks
# its SHA-256, and installs it to $CARL_INSTALL_DIR (default ~/.local/bin).
# That directory must stay writable by you: Carl updates itself in place.
set -eu

REPO="KarpelesLab/carl"
DIR="${CARL_INSTALL_DIR:-$HOME/.local/bin}"
BASE="https://github.com/$REPO/releases/latest/download"
ASSET="carl-linux-x86_64"

say() { printf '%s\n' "$*"; }
die() { say "error: $*" >&2; exit 1; }

case "$(uname -s)/$(uname -m)" in
    Linux/x86_64 | Linux/amd64) ;;
    *) die "prebuilt binaries are Linux x86_64 only for now; build from source instead:
  cargo install --git https://github.com/$REPO" ;;
esac

if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -qO "$2" "$1"; }
else
    die "need curl or wget"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

say "Downloading Carl…"
fetch "$BASE/$ASSET" "$tmp/$ASSET" || die "download failed"
fetch "$BASE/$ASSET.sha256" "$tmp/$ASSET.sha256" || die "checksum download failed"
(cd "$tmp" && sha256sum -c "$ASSET.sha256" >/dev/null) || die "checksum mismatch"

mkdir -p "$DIR"
chmod 755 "$tmp/$ASSET"
mv -f "$tmp/$ASSET" "$DIR/carl"
say "Installed $("$DIR/carl" --version) to $DIR/carl"

case ":$PATH:" in
    *":$DIR:"*) ;;
    *) say "note: $DIR is not in your PATH (not needed for MCP clients)" ;;
esac

say ""
say "Next, add it to your agent:"
if command -v claude >/dev/null 2>&1; then
    say "  claude mcp add --scope user carl -- $DIR/carl"
fi
if command -v codex >/dev/null 2>&1; then
    say "  codex mcp add carl -- $DIR/carl"
fi
if ! command -v claude >/dev/null 2>&1 && ! command -v codex >/dev/null 2>&1; then
    say "  Claude Code: claude mcp add --scope user carl -- $DIR/carl"
    say "  Codex:       codex mcp add carl -- $DIR/carl"
fi
say "More: https://github.com/$REPO#getting-started"
