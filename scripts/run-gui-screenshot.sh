#!/bin/bash
# run-gui-screenshot.sh — GUI アプリを起動してスクリーンショットを撮る
#
# 使い方:
#   scripts/run-gui-screenshot.sh docs/images/gui-calc-pad.png

set -e

OUT="${1:-docs/images/gui-calc-pad.png}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR/.."

mkdir -p "$(dirname "$OUT")"

echo "$OUT" > scripts/gui-screenshot-path.txt
echo "Running make test with GUI screenshot..."
make test
