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
- **1つの作業（機能追加・修正）が終わるたびにコミットする**。まとめてコミットしない

## Development Principles

- 開発日記を `docs/YYYY-MM-DD.md` に書く。その日やったこと、学んだこと、スクショを残す
- 習作プロジェクトなので楽しさと学びを重視
- 一度作った機能は壊さない。GitHub Actions でビルドの成功を常に保証する
- コメントはマシマシで書く。学習用プロジェクトなので「なぜそうしているか」「何をしているか」を後から読んで理解できるよう丁寧にコメントを残す
- **新しい技術用語が出てきたら、日記の文中で自然にその意味を説明する**。用語集として切り出すのではなく、文章の流れの中で「これはこういう意味」と書く。読み返したときに「これ何だっけ？」とならないように

## Daily Workflow

- 1日のはじめにその日の計画を `docs/YYYY-MM-DD.md` に書き、計画に沿って進める
- 各タスクは実装計画を具体的に（変更ファイル・変更内容）書き出してから着手する
- タスクは小さく分割する。大きすぎると planning mode に入ってコンテキストが失われるため

## Language

- ユーザーへの応答は日本語で行うこと
- コミットメッセージも日本語で書く
- コード内のコメントも日本語で書く（学習用プロジェクトのため）

## CI/CD

- GitHub Actions で `cargo build --target x86_64-unknown-uefi` が通ることを保証
- 新機能を追加したら対応するCIチェックも追加する

