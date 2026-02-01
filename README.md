# SABOS

x86_64向け自作OS。Rust + UEFIで構築する。

## Goals

- ネットワーク接続
- GUIアプリケーションの動作

## Architecture Decision Records

- [ADR-0001: Foundation](docs/adr/0001-foundation.md) — アーキテクチャ・言語・ブート方式

## Build

```bash
cargo build --target x86_64-unknown-uefi
```

## Run

```bash
make run
```
