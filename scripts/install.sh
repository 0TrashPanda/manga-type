#!/bin/sh
# Build manga-type and install it for the current user (no sudo needed):
#   ~/.local/bin/manga-type                                   symlink to target/release/manga-type
#   ~/.local/share/applications/manga-type.desktop            app launcher entry
#   ~/.local/share/icons/hicolor/scalable/apps/manga-type.svg icon
#   ~/.local/share/manga-type/models/PP-OCRv6_*_small.onnx      PaddleOCR models (~31 MB,
#       from RapidOCR's model host, SHA256-checked; skipped when already there)
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

# PaddleOCR models (PP-OCRv6 small, Apache-2.0), as published by RapidOCR 3.10.0.
models="$data/manga-type/models"
mkdir -p "$models"
fetch() {
    name=$1 url=$2 sha=$3
    if [ -f "$models/$name" ] && echo "$sha  $models/$name" | sha256sum -c --quiet 2>/dev/null; then
        return
    fi
    echo "Downloading $name ..."
    curl -fL --retry 3 -o "$models/$name.part" "$url"
    if ! echo "$sha  $models/$name.part" | sha256sum -c --quiet; then
        rm -f "$models/$name.part"
        echo "Checksum mismatch for $name, not installed." >&2
        exit 1
    fi
    mv "$models/$name.part" "$models/$name"
}
base=https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.10.0/onnx/PP-OCRv6
fetch PP-OCRv6_det_small.onnx "$base/det/PP-OCRv6_det_small.onnx" 090f04abcd9d9a7498bc4ebf677e4cb9bdce1fe4197ddb7e529f1ef44e1ff94f
fetch PP-OCRv6_rec_small.onnx "$base/rec/PP-OCRv6_rec_small.onnx" 6f327246b50388f3c176ae304bd95767ea6dc0c9ae92153ef8cbe210b3c14884

echo "Installed: $bin/manga-type and the manga-type app launcher entry."
if ! command -v tesseract >/dev/null 2>&1; then
    echo "Note: the word list (and the fallback OCR) need Tesseract:  sudo pacman -S tesseract tesseract-data-eng"
fi
