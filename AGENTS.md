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

5. **外部 crate の活用を推奨する**
   - 車輪の再発明は避け、品質の高い既存 crate を積極的に使う
   - ただしパフォーマンスや学習目的で自前実装する場合はその限りではない
   - 「自作 OS だから全部自前で書く」という方針は取らない

6. **procfs は書き込み禁止**
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

# ユーザーランドバイナリをホスト共有ディスクに更新（disk.img 再作成不要）
make hostfs-update
```

## Project Structure

- `kernel/` - カーネル本体（Rust, no_std, UEFI target）
- `user/` - ユーザープログラム（ELF バイナリ、x86_64-unknown-none target）
- `user-std/` - std 対応ユーザープログラム（カスタム target、release ビルド）
- `libs/` - 共有ライブラリ（sabos-syscall 等）
- `scripts/` - テストスクリプト等
- `rust-std-sabos/` - Rust std の SABOS 向けパッチ（PAL 層）
- `docs/diary/YYYY-MM-DD.md` - 開発日記（スクショも貼る）
- `docs/images/` - スクリーンショット置き場
- `docs/spec/` - 仕様ドキュメント置き場

### ディスクイメージ

- `disk.img` - システムディスク（FAT32、64MB）。`make disk-img` で全体再作成
- `hostfs.img` - ホスト共有ディスク（FAT32、64MB）。`make hostfs-update` でインクリメンタル更新
- いずれも `.gitignore` で管理対象外

### VFS アーキテクチャ

全ファイル操作は VFS マネージャ経由で行う。マウントテーブル（BTreeMap）でパスの最長一致ルーティングを行う。

| マウントポイント | ファイルシステム | デバイス |
|----------------|----------------|---------|
| `/` | fat32（カーネル内） | virtio-blk[0]（disk.img） |
| `/proc` | ProcFs | なし（疑似ファイルシステム） |
| `/host` | fat32（カーネル内） | virtio-blk[1]（hostfs.img） |
| `/9p` | 9P2000.L | virtio-9p（ホスト `./user/target` 共有） |

カーネル内 FAT32 で `/` と `/host` をマウント。すべてのファイル操作はカーネル内で完結する。

QEMU は 2 台の virtio-blk デバイスと virtio-9p デバイスを接続する（256MB RAM）。カーネルは PCI バスをスキャンして全 virtio デバイスを検出・初期化する。

## ドキュメント一覧

- 開発日記: `docs/diary/YYYY-MM-DD.md`
- スクリーンショット: `docs/images/`
- 仕様ドキュメント: `docs/spec/`
- `setup-ubuntu.sh` - Ubuntu 向け開発環境セットアップスクリプト

## Git Workflow

- main ブランチにどんどんコミットしてプッシュする。PRやブランチ運用はしない
- コミットはこまめに行い、動く状態を保つ
- **1つの作業（機能追加・修正）が終わるたびにコミットする**。まとめてコミットしない
- **push するのに確認は不要**

## Development Principles

- 開発日記を `docs/diary/YYYY-MM-DD.md` に書く。その日やったこと、学んだこと、スクショを残す
- 仕様ドキュメントは `docs/spec/` に置く。`docs/` 直下には置かない
- 習作プロジェクトなので楽しさと学びを重視
- 一度作った機能は壊さない。GitHub Actions でビルドの成功を常に保証する
- コメントはマシマシで書く。学習用プロジェクトなので「なぜそうしているか」「何をしているか」を後から読んで理解できるよう丁寧にコメントを残す
- **新しい技術用語が出てきたら、日記の文中で自然にその意味を説明する**。用語集として切り出すのではなく、文章の流れの中で「これはこういう意味」と書く。読み返したときに「これ何だっけ？」とならないように
- **コンパイル時の警告は全て解消する**。自分が出した警告でなくても、見つけたら対応する。警告ゼロの状態を維持する
- **デッドコード（未使用のコード）は削除する**。`dead_code` 警告が出たら、将来使うかもしれないと残さずに消す。必要になったら git から復元できる
- **システムコール一覧は常に最新に保つ**。追加・削除・引数変更・戻り値変更を行ったら `docs/spec/syscall-list.md` を更新する
- **可能な限り自動テストを実装する**。手動確認が必要な箇所は、CI で再現できる形に落とし込む
- **日記は初日のテイストで書く**。事実の箇条書きだけでなく、当日の目的、試行錯誤、つまずき、気づき、嬉しかった点などを物語として書く
- **手抜きした箇所・暫定対応は TODO.md / TODO_std.md に残作業として記録する**。「動くけど正しくない」実装やワークアラウンドを入れた場合、後で見落とさないよう TODO ファイルに具体的に何を直すべきかを書いておく

## Daily Workflow

- 日記を書く前に `date` コマンドを実行して日付を確認する
- 日記の一日の初めに「今日一日、こんなことが出来たらいいな」という意気込みを書く
- 1日のはじめにその日の計画を `docs/diary/YYYY-MM-DD.md` に書き、計画に沿って進める
- 各タスクは実装計画を具体的に（変更ファイル・変更内容）書き出してから着手する
- タスクは小さく分割する。大きすぎると planning mode に入ってコンテキストが失われるため
- **日記を書いた後に「開発サイクルの振り返り」を行う**。今回の作業で開発がスムーズにいかなかった点を特定し、改善策を考えて実行する。例: テストが手動だった → 自動化する、定数が重複していた → 共有する仕組みを作る、など。小さな改善を積み重ねて開発体験を良くしていく

## Language

- ユーザーへの応答は日本語で行うこと
- コミットメッセージも日本語で書く
- コード内のコメントも日本語で書く（学習用プロジェクトのため）

## Testing

- `make test` で自動テストを実行できる
- QEMU を起動して `selftest` コマンドを自動実行し、結果を検証する
- テスト対象: メモリアロケータ、ページング、スケジューラ、virtio-blk、FAT32、IPC、ハンドル操作、syscall、ネットワーク、GUI、サーバーデーモン、9P 等（49 項目）
- **新機能を追加したら `selftest` にもテストを追加する**
- **修正したら指示がなくても必ずテストを実行する**
- **日記の更新や AGENTS.md の更新だけの場合は `make test` を省略してよい**

### selftest コマンド

シェルで `selftest` を実行すると各サブシステムをテストする（49 項目）:

```
sabos> selftest
=== SELFTEST START ===
[PASS] memory_allocator
[PASS] slab_allocator
[PASS] memory_mapping
[PASS] paging
...
=== SELFTEST END: 49/49 PASSED ===
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

**期待される selftest 結果（49 項目全 PASS）:**
```
=== SELFTEST START ===
[PASS] memory_allocator
[PASS] slab_allocator
[PASS] memory_mapping
[PASS] paging
[PASS] pci_enum
...（省略）...
[PASS] httpd_dirlist
=== SELFTEST END: 49/49 PASSED ===
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

### ユーザーランド開発ワークフロー

ユーザーランドバイナリの変更を素早くテストするには `hostfs-update` を使う:

```bash
# 1. ユーザープログラムを変更・ビルド
make build-user

# 2. hostfs.img にインクリメンタルコピー（disk.img 再作成不要）
make hostfs-update

# 3. QEMU 再起動して /host/ 経由でアクセス
make run-gui
# ゲスト内: run /host/SHELL.ELF
```

`/host` はゲスト内で VFS を通じて 2 台目の virtio-blk デバイス（hostfs.img）にマウントされる。`mcopy -o` による上書きコピーなので `dd + mkfs.fat` の全体再作成より高速。

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

### ログとテンポラリファイル

- QEMU のログは `./logs/` に自動保存される（タイムスタンプ付き）。`/tmp/` は使わない
- `make run` / `make run-gui` は `scripts/run-qemu.sh` 経由で QEMU を起動する。既存の QEMU プロセスは自動で pkill される
- `make run` は `disk.img` が無ければ自動作成する（明示的に再作成したい場合は `make disk-img`）
- `make test` のログも `./logs/` に保存される。テスト失敗時はログパスが表示される
- `./tmp/` は .gitignore されているので、ここにテンポラリファイルを書いてもよい
- `./logs/` も .gitignore されている

