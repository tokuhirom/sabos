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

5. **procfs は書き込み禁止**
   - /proc はカーネル内部情報を「表示するための疑似ファイルシステム」として扱う
   - 変更操作（write/delete）は将来も許可しない方針
   - procfs の出力は JSON 形式に統一する

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
- `docs/diary/YYYY-MM-DD.md` - 開発日記（スクショも貼る）
- `docs/images/` - スクリーンショット置き場

## ドキュメント一覧

- 開発日記: `docs/diary/YYYY-MM-DD.md`
- スクリーンショット: `docs/images/`
- `setup-ubuntu.sh` - Ubuntu 向け開発環境セットアップスクリプト

## Git Workflow

- main ブランチにどんどんコミットしてプッシュする。PRやブランチ運用はしない
- コミットはこまめに行い、動く状態を保つ
- **1つの作業（機能追加・修正）が終わるたびにコミットする**。まとめてコミットしない

## Development Principles

- 開発日記を `docs/diary/YYYY-MM-DD.md` に書く。その日やったこと、学んだこと、スクショを残す
- 習作プロジェクトなので楽しさと学びを重視
- 一度作った機能は壊さない。GitHub Actions でビルドの成功を常に保証する
- コメントはマシマシで書く。学習用プロジェクトなので「なぜそうしているか」「何をしているか」を後から読んで理解できるよう丁寧にコメントを残す
- **新しい技術用語が出てきたら、日記の文中で自然にその意味を説明する**。用語集として切り出すのではなく、文章の流れの中で「これはこういう意味」と書く。読み返したときに「これ何だっけ？」とならないように
- **コンパイル時の警告は全て解消する**。自分が出した警告でなくても、見つけたら対応する。警告ゼロの状態を維持する
- **デッドコード（未使用のコード）は削除する**。`dead_code` 警告が出たら、将来使うかもしれないと残さずに消す。必要になったら git から復元できる

## Daily Workflow

- 1日のはじめにその日の計画を `docs/diary/YYYY-MM-DD.md` に書き、計画に沿って進める
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

## 動作確認手順

### 自動テスト

```bash
make test
```

これで以下が自動的に実行される:
1. ユーザーシェル起動を待機 (`user>` プロンプト)
2. `exit` でユーザーシェルを終了
3. カーネルシェル起動を待機 (`sabos>` プロンプト)
4. `selftest` コマンドを実行
5. 全テスト PASS を確認

### 手動テスト（GUI モード）

```bash
make run-gui
```

#### 1. ユーザーシェルの確認

起動後、ユーザーシェル (`user>` プロンプト) が表示される。

**基本コマンド:**
```
user> help          # コマンド一覧
user> echo Hello    # エコー
user> clear         # 画面クリア
```

**ファイルシステム:**
```
user> ls            # ルートディレクトリ一覧
user> ls /SUBDIR    # サブディレクトリ一覧
user> cat HELLO.TXT # ファイル内容表示
user> write TEST.TXT Hello World  # ファイル作成
user> rm TEST.TXT   # ファイル削除
```

**システム情報:**
```
user> mem           # メモリ情報（JSON形式）
user> ps            # タスク一覧（JSON形式）
user> ip            # ネットワーク設定（JSON形式）
```

**プロセス実行:**
```
user> run /HELLO.ELF    # ELF バイナリを実行（フォアグラウンド）
user> spawn /HELLO.ELF  # ELF バイナリを実行（バックグラウンド）
user> sleep 1000        # 1000ms スリープ
```

**ネットワーク:**
```
user> dns google.com         # DNS 解決
user> http example.com /     # HTTP GET リクエスト
```

**システム制御:**
```
user> halt          # システム停止（復帰不可）
user> exit          # ユーザーシェル終了 → カーネルシェルへ
```

#### 2. カーネルシェルの確認

`exit` でユーザーシェルを終了するとカーネルシェル (`sabos>` プロンプト) に移行する。

**デバッグコマンド:**
```
sabos> selftest     # 自動テスト実行
sabos> lspci        # PCI デバイス一覧
sabos> blkread 0    # セクタ 0 の読み取り
sabos> panic        # カーネルパニックのテスト
```

**期待される selftest 結果:**
```
=== SELFTEST START ===
[PASS] memory_allocator
[PASS] paging
[PASS] scheduler
[PASS] virtio_blk
[PASS] fat16
[PASS] network_dns
=== SELFTEST END: 6/6 PASSED ===
```

### シェルの起動フロー

```
カーネル起動
    ↓
ユーザーシェル自動起動 (user>)
    ↓
exit コマンド
    ↓
カーネルシェルにフォールバック (sabos>)
```

### トラブルシューティング

**ユーザーシェルが起動しない場合:**
- `kernel/src/main.rs` の `Starting user shell...` 付近のログを確認
- ELF バイナリが正しくビルドされているか確認 (`make build-user`)

**selftest が失敗する場合:**
- 個別のテスト項目を確認して、どのサブシステムで失敗しているか特定
- `network_dns` 失敗時は QEMU のネットワーク設定を確認

**キーボード入力が効かない場合:**
- GUI モード (`make run-gui`) で実行しているか確認
- シリアルモード (`make run`) ではキーボード入力は使えない
