#!/bin/sh
# Installs the latest cockpit release: curl -fsSL https://raw.githubusercontent.com/Xunrise/ticket-cockpit/main/install.sh | sh
set -eu
REPO="${COCKPIT_RELEASE_REPO:-Xunrise/ticket-cockpit}"
BASE="${COCKPIT_RELEASE_BASE:-https://github.com}"
DEST="${COCKPIT_INSTALL_DIR:-$HOME/.local/bin}"

[ "$(uname -s)" = Darwin ] || { echo "cockpit needs macOS" >&2; exit 1; }
case "$(uname -m)" in
  arm64) TARGET=aarch64-apple-darwin ;;
  x86_64) TARGET=x86_64-apple-darwin ;;
  *) echo "unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac
command -v tmux >/dev/null || echo "warning: tmux is required (brew install tmux)" >&2

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
curl -fsSL "$BASE/$REPO/releases/latest/download/manifest.json" -o "$TMP/manifest.json"
# the manifest is flat enough to read with plutil (part of macOS)
URL="$(plutil -extract "assets.$TARGET.url" raw -o - "$TMP/manifest.json")"
SHA="$(plutil -extract "assets.$TARGET.sha256" raw -o - "$TMP/manifest.json")"
VERSION="$(plutil -extract version raw -o - "$TMP/manifest.json")"

echo "downloading cockpit $VERSION ($TARGET)"
curl -fsSL "$URL" -o "$TMP/cockpit.tar.gz"
GOT="$(shasum -a 256 "$TMP/cockpit.tar.gz" | cut -d' ' -f1)"
[ "$GOT" = "$SHA" ] || { echo "sha256 mismatch: expected $SHA, got $GOT" >&2; exit 1; }
tar -xzf "$TMP/cockpit.tar.gz" -C "$TMP"

mkdir -p "$DEST"
cp "$TMP/cockpit" "$DEST/cockpit.new"
chmod 755 "$DEST/cockpit.new"
[ -f "$DEST/cockpit" ] && cp "$DEST/cockpit" "$DEST/cockpit.prev"
mv "$DEST/cockpit.new" "$DEST/cockpit"
echo "installed $DEST/cockpit"
case ":$PATH:" in *":$DEST:"*) ;; *) echo "warning: $DEST is not on your PATH; add it to your shell profile" >&2 ;; esac

"$DEST/cockpit" tool settings.set '{"key":"update_channel","value":"release"}' >/dev/null
[ -n "${COCKPIT_SKIP_INSTALL:-}" ] || "$DEST/cockpit" install
