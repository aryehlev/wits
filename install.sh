#!/bin/sh
# Install wits: one binary, no dependencies.
#
#   curl -fsSL https://raw.githubusercontent.com/aryehlev/wits/main/install.sh | sh
#
# Environment:
#   WITS_INSTALL_DIR   where to put the binary (default: ~/.local/bin)
#   WITS_VERSION       release tag to install (default: latest)
set -eu

REPO=aryehlev/wits
DIR=${WITS_INSTALL_DIR:-$HOME/.local/bin}

case "$(uname -s)" in
  Linux) OS=unknown-linux-musl ;;
  Darwin) OS=apple-darwin ;;
  *) echo "wits runs on Linux and macOS." >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) TARGET=x86_64-$OS ;;
  aarch64 | arm64) TARGET=aarch64-$OS ;;
  *) echo "no prebuilt wits for $(uname -m); try: cargo install wits" >&2; exit 1 ;;
esac

if [ -n "${WITS_VERSION:-}" ]; then
  BASE=https://github.com/$REPO/releases/download/$WITS_VERSION
else
  BASE=https://github.com/$REPO/releases/latest/download
fi
FILE=wits-$TARGET.tar.gz

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q "$1" -O "$2"; }
else
  echo "need curl or wget" >&2; exit 1
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
fetch "$BASE/$FILE" "$TMP/$FILE"
fetch "$BASE/$FILE.sha256" "$TMP/$FILE.sha256"
if command -v sha256sum >/dev/null 2>&1; then
  (cd "$TMP" && sha256sum -c "$FILE.sha256" >/dev/null) || { echo "checksum mismatch for $FILE" >&2; exit 1; }
elif command -v shasum >/dev/null 2>&1; then
  (cd "$TMP" && shasum -a 256 -c "$FILE.sha256" >/dev/null) || { echo "checksum mismatch for $FILE" >&2; exit 1; }
fi
tar -xzf "$TMP/$FILE" -C "$TMP"
mkdir -p "$DIR"
install -m 755 "$TMP/wits" "$DIR/wits"

echo "installed $("$DIR/wits" --version) to $DIR/wits"
case ":$PATH:" in
  *":$DIR:"*) ;;
  *) echo "note: $DIR is not on your PATH" ;;
esac
