#!/bin/bash
# SABOS 開発環境セットアップスクリプト（Ubuntu 向け）
# 使い方: bash setup-ubuntu.sh

set -euo pipefail

echo "=== SABOS 開発環境セットアップ ==="

# QEMU, OVMF, imagemagick のインストール
echo "--- apt パッケージのインストール ---"
sudo apt-get update
sudo apt-get install -y qemu-system-x86 ovmf imagemagick curl build-essential

# rustup のインストール（未インストールの場合）
if ! command -v rustup &>/dev/null; then
    echo "--- rustup のインストール ---"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    source "$HOME/.cargo/env"
fi

# nightly ツールチェインとターゲットのセットアップ
echo "--- Rust nightly のセットアップ ---"
rustup install nightly
rustup default nightly
rustup target add x86_64-unknown-uefi --toolchain nightly
rustup target add x86_64-unknown-none --toolchain nightly
rustup component add rust-src --toolchain nightly

echo ""
echo "=== セットアップ完了 ==="
echo "  make run      : シリアル出力で実行"
echo "  make run-gui  : GUI 付きで実行"
