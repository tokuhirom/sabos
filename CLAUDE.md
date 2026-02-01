# CLAUDE.md - SABOS Development Guide

## Project Overview

SABOS は x86_64 向け自作OS。Rust (no_std) + UEFI で構築する。

## Build & Run

```bash
# devbox shell に入る（Rust nightly, QEMU, OVMF が使える）
devbox shell

# ビルド
cd kernel && cargo build

# QEMU で実行（シリアル出力のみ）
make run

# QEMU で実行（GUI付き）
make run-gui

# スクリーンショットを撮る
make screenshot SCREENSHOT_OUT=docs/images/foo.png

# 待ち時間を変えたい場合（デフォルト6秒）
make screenshot SCREENSHOT_OUT=docs/images/foo.png SCREENSHOT_WAIT=10
```

## Project Structure

- `kernel/` - カーネル本体（Rust, no_std, UEFI target）
- `docs/YYYY-MM-DD.md` - 開発日記（スクショも貼る）
- `devbox.json` - 開発環境定義（rustup, qemu, OVMF）

## Git Workflow

- main ブランチにどんどんコミットしてプッシュする。PRやブランチ運用はしない
- コミットはこまめに行い、動く状態を保つ

## Development Principles

- 開発日記を `docs/YYYY-MM-DD.md` に書く。その日やったこと、学んだこと、スクショを残す
- 習作プロジェクトなので楽しさと学びを重視
- 一度作った機能は壊さない。GitHub Actions でビルドの成功を常に保証する
- コメントはマシマシで書く。学習用プロジェクトなので「なぜそうしているか」「何をしているか」を後から読んで理解できるよう丁寧にコメントを残す

## CI/CD

- GitHub Actions で `cargo build --target x86_64-unknown-uefi` が通ることを保証
- 新機能を追加したら対応するCIチェックも追加する

