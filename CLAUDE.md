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
```

## Project Structure

- `kernel/` - カーネル本体（Rust, no_std, UEFI target）
- `docs/adr/` - Architecture Decision Records
- `devbox.json` - 開発環境定義（rustup, qemu, OVMF）

## Git Workflow

- main ブランチにどんどんコミットしてプッシュする。PRやブランチ運用はしない
- コミットはこまめに行い、動く状態を保つ

## Development Principles

- ADRを開発日記代わりに積極的に書く。設計判断は必ずADRに記録する
- 習作プロジェクトなので楽しさと学びを重視
- 一度作った機能は壊さない。GitHub Actions でビルドの成功を常に保証する
- コメントはマシマシで書く。学習用プロジェクトなので「なぜそうしているか」「何をしているか」を後から読んで理解できるよう丁寧にコメントを残す

## CI/CD

- GitHub Actions で `cargo build --target x86_64-unknown-uefi` が通ることを保証
- 新機能を追加したら対応するCIチェックも追加する

## Architecture Decisions

ADR は `docs/adr/` に連番で管理する。フォーマット:

```
# ADR-NNNN: タイトル
## Status (Proposed / Accepted / Deprecated / Superseded)
## Context
## Decision
## Consequences
```
