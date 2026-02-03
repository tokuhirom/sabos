# SABOS

x86_64向け自作OS。Rust + UEFIで構築する。

## Goals

- ネットワーク接続
- GUIアプリケーションの動作

## Dev Diary

- [2026-02-01](docs/diary/2026-02-01.md) — Day 1: プロジェクト開始、Hello SABOS!

## Build

```bash
cargo build --target x86_64-unknown-uefi
```

## Run

```bash
make run
```
