# ADR-0001: Foundation — アーキテクチャ・言語・ブート方式

## Status

Accepted

## Context

自作OS「SABOS」の開発を開始するにあたり、以下の基本方針を決定する必要がある。

- ターゲットCPUアーキテクチャ
- 実装言語
- ブート方式

最終目標はネットワーク接続とGUIアプリケーションの動作。
習作プロジェクトであり、楽しさと学びを重視する。

## Decision

### ターゲットアーキテクチャ: x86_64

- QEMUでのデバッグが容易
- OS開発の資料が最も豊富
- ネットワーク・GUIの実績が多い（virtio-net, virtio-gpu等）

### 実装言語: Rust

- メモリ安全性をコンパイル時に保証
- `no_std` 環境でのベアメタル開発が充実
- `uefi` crateなどOS開発向けエコシステムが活発
- 所有権システムにより、カーネル開発でありがちなuse-after-freeやダブルフリーを防止

### ブート方式: UEFI

- モダンなファームウェアインターフェース
- 起動時からグラフィックスフレームバッファが利用可能（GOP）
- メモリマップの取得が容易
- Legacy BIOSのようなリアルモード→プロテクトモード→ロングモードの遷移が不要
- GUI目標との相性が良い

## Consequences

- nightly Rustツールチェインが必要（`x86_64-unknown-uefi` ターゲット）
- QEMUとOVMF（UEFI firmware for QEMU）が開発に必要
- ブートローダ自作の手間は省けるが、UEFIプロトコルの理解が必要
- Rustのno_std環境ではstdライブラリが使えないため、基本的なデータ構造は自前実装 or `alloc` crateを利用
