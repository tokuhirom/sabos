# SABOS

x86_64向け自作OS。Rust (no_std) + UEFIで構築する。

## Goals

- ネットワーク接続
- GUIアプリケーションの動作

## Dev Diary

- [2026-02-01](docs/diary/2026-02-01.md) — Day 1: プロジェクト開始、Hello SABOS!
- [2026-02-02](docs/diary/2026-02-02.md) — Day 2: FAT16と基本シェル
- [2026-02-03](docs/diary/2026-02-03.md) — Day 3: IPCとselftestの整備
- [2026-02-04](docs/diary/2026-02-04.md) — Day 4: 型安全IPC・Capability・init

## Build

```bash
make build
```

## Run

```bash
make run
```

## Run (GUI)

```bash
make run-gui
```

## Test

```bash
make test
```
