#!/bin/sh
# Build manga-type and install it for the current user (no sudo needed):
#   ~/.local/bin/manga-type                                   symlink to target/release/manga-type
#   ~/.local/share/applications/manga-type.desktop            app launcher entry
#   ~/.local/share/icons/hicolor/scalable/apps/manga-type.svg icon
# Re-run after `git pull` to rebuild; the symlink then points at the new binary.
set -eu
cd "$(dirname "$0")/.."

cargo build --release --locked

data="${XDG_DATA_HOME:-$HOME/.local/share}"
bin="$HOME/.local/bin"
mkdir -p "$bin" "$data/applications" "$data/icons/hicolor/scalable/apps"

ln -sfn "$PWD/target/release/manga-type" "$bin/manga-type"
sed "s|@BIN@|$bin/manga-type|" packaging/manga-type.desktop > "$data/applications/manga-type.desktop"
cp packaging/manga-type.svg "$data/icons/hicolor/scalable/apps/manga-type.svg"
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$data/applications" || true
fi

echo "Installed: $bin/manga-type and the manga-type app launcher entry."
if ! command -v tesseract >/dev/null 2>&1; then
    echo "Note: OCR needs Tesseract:  sudo pacman -S tesseract tesseract-data-eng"
fi
