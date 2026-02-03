# CLAUDE.md - SABOS Development Guide

## Project Overview

SABOS は x86_64 向け自作OS。Rust (no_std) + UEFI で構築する。

## Design Philosophy

SABOS はセキュリティと型安全性を重視した設計を採用する。

### 基本原則

1. **POSIX 互換は目指さない**
   - レガシーな制約に縛られず、より良い API を設計する
   - 必要に応じて独自のシステムコール体系を構築する

2. **null 終端文字列をカーネル API から排除する**
   - すべての文字列・バッファは長さ付き（スライス形式）で渡す
   - バッファオーバーフローを構造的に防止する
   - 例: `(ptr, len)` 形式を使い、C の `char*` 形式は使わない

3. **システムコール境界で型安全性を保証する**
   - ユーザー空間からのポインタは `UserPtr<T>` / `UserSlice<T>` でラップする
   - カーネル側で必ず検証してからアクセスする
   - 不正なアドレスはコンパイル時または実行時に弾く

4. **Rust のメモリ安全性をカーネルレベルで活かす**
   - `unsafe` ブロックは最小限に閉じ込める
   - 所有権システムでリソースリークを防止する
   - 生ポインタ操作は明示的な型でラップする

### システムコール設計指針

```rust
// NG: POSIX 風（危険）
ssize_t write(int fd, const void *buf, size_t count);

// OK: SABOS 風（型安全）
struct WriteArgs {
    handle: Handle,       // fd の代わりに型付きハンドル
    buf: UserSlice<u8>,   // 検証済みユーザー空間スライス
}
fn sys_write(args: &WriteArgs) -> Result<usize, SyscallError>;
```

### アーキテクチャ目標: マイクロカーネル

将来的にはマイクロカーネルアーキテクチャを目指す。

```
┌─────────────────────────────────────────┐
│           User Space                     │
│  ┌─────┐ ┌─────┐ ┌─────┐ ┌─────────┐   │
│  │Shell│ │FS   │ │Net  │ │Drivers  │   │
│  └──┬──┘ └──┬──┘ └──┬──┘ └────┬────┘   │
│     └───────┴───────┴─────────┘         │
│                  ↓ IPC                   │
├─────────────────────────────────────────┤
│           Microkernel                    │
│  • スケジューラ                          │
│  • メモリ管理                            │
│  • IPC (メッセージパッシング)             │
│  • 割り込みハンドリング                   │
└─────────────────────────────────────────┘
```

**メリット:**
- ドライバやファイルシステムのバグがカーネルをクラッシュさせない
- コンポーネントを独立して再起動可能
- カーネルが小さいので形式検証しやすい
- IPC メッセージに型を付けることで Rust の型安全性を活かせる

**移行ロードマップ:**
1. 現状: モノリシックカーネルで機能開発を進める
2. IPC 基盤を設計・実装（型安全なメッセージパッシング）
3. シェルをユーザー空間に移行
4. ファイルシステム → ネットワーク → ドライバの順でユーザー空間へ

## Build & Run

```bash
# 初回セットアップ（Ubuntu）
bash setup-ubuntu.sh

# ビルド
make build

# QEMU で実行（シリアル出力のみ）
make run

# QEMU で実行（GUI付き）
make run-gui

# スクリーンショットを撮る
make screenshot SCREENSHOT_OUT=docs/images/foo.png

# 待ち時間を変えたい場合（デフォルト6秒）
make screenshot SCREENSHOT_OUT=docs/images/foo.png SCREENSHOT_WAIT=10

# 自動テストを実行
make test
```

## Project Structure

- `kernel/` - カーネル本体（Rust, no_std, UEFI target）
- `user/` - ユーザープログラム（ELF バイナリ、x86_64-unknown-none target）
- `scripts/` - テストスクリプト等
- `docs/YYYY-MM-DD.md` - 開発日記（スクショも貼る）
- `setup-ubuntu.sh` - Ubuntu 向け開発環境セットアップスクリプト

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

## Testing

- `make test` で自動テストを実行できる
- QEMU を起動して `selftest` コマンドを自動実行し、結果を検証する
- テスト対象: メモリアロケータ、ページング、スケジューラ、virtio-blk、FAT16、DNS
- **新機能を追加したら `selftest` にもテストを追加する**

### selftest コマンド

シェルで `selftest` を実行すると各サブシステムをテストする:

```
sabos> selftest
=== SELFTEST START ===
[PASS] memory_allocator
[PASS] paging
...
=== SELFTEST END: 6/6 PASSED ===
```

## CI/CD

- GitHub Actions で build と test の 2 ジョブを実行
- build: カーネルとユーザープログラムのビルド確認
- test: QEMU で実際に起動して selftest を実行
- 新機能を追加したら対応するテストも追加する

