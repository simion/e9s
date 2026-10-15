#!/bin/sh
# Installs a `e9s` launcher on PATH that runs this checkout's source.
# Each launch rebuilds first if the source changed (a no-op build when it did not).
#   ./install.sh               install into ~/.local/bin (or BIN_DIR=/some/dir ./install.sh)
#   ./install.sh --uninstall   remove the launcher
set -eu

SRC="$(cd "$(dirname "$0")" && pwd)"
BIN_DIR="${BIN_DIR:-$HOME/.local/bin}"
TARGET="$BIN_DIR/e9s"

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$TARGET"
    echo "removed $TARGET"
    exit 0
fi

# cargo from PATH, else the usual rustup and Homebrew locations.
CARGO="$(command -v cargo 2>/dev/null || true)"
for c in "$HOME/.cargo/bin/cargo" /opt/homebrew/opt/rustup/bin/cargo /usr/local/opt/rustup/bin/cargo; do
    [ -n "$CARGO" ] && break
    [ -x "$c" ] && CARGO="$c"
done
if [ -z "$CARGO" ]; then
    echo "cargo not found: install Rust (https://rustup.rs) and run this again" >&2
    exit 1
fi
command -v aws >/dev/null 2>&1 || echo "warning: the AWS CLI v2 (aws) is not on PATH; e9s needs it at runtime" >&2

# Rustup's proxies find rustc through PATH, so the launcher carries cargo's directory.
CARGO_DIR="$(dirname "$CARGO")"
export PATH="$CARGO_DIR:$PATH"

mkdir -p "$BIN_DIR"
cat > "$TARGET" <<EOF
#!/bin/sh
# e9s launcher written by $SRC/install.sh: rebuilds from source when it changed, then runs it.
BIN="$SRC/target/release/e9s"
# Rebuild only when a source file is newer than the binary.
if [ ! -x "\$BIN" ] || [ -n "\$(find "$SRC/src" "$SRC/Cargo.toml" "$SRC/Cargo.lock" -newer "\$BIN" 2>/dev/null | head -n 1)" ]; then
    echo "e9s: source changed, rebuilding..." >&2
    PATH="$CARGO_DIR:\$PATH" "$CARGO" build --release --manifest-path "$SRC/Cargo.toml" >&2 || exit \$?
    touch "\$BIN"
fi
exec "\$BIN" "\$@"
EOF
chmod +x "$TARGET"

echo "building (the first release build takes a minute; later ones only rebuild what changed)..."
"$CARGO" build --release --manifest-path "$SRC/Cargo.toml"
echo "installed $TARGET -> $SRC"
case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) echo "note: $BIN_DIR is not on PATH; add it to your shell profile" ;;
esac
