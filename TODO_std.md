# TODO: Rust std ライブラリ対応ロードマップ

SABOS のユーザープログラムで `std` クレートを使えるようにするための TODO リスト。
Phase 7 で基本的な std 対応（`println!` / `String` / `Vec`）が動くようになった。
今後は PAL の各モジュールを充実させ、外部クレートが使えるレベルを目指す。

## 現在の状況

**方法 C（カスタムターゲット JSON + `-Zbuild-std`）を採用**し、以下が動作している:

- `x86_64-sabos.json` カスタムターゲット（`os = "sabos"`）
- sysroot パッチ方式で PAL を追加（`scripts/patch-rust-sysroot.sh`）
- `user-std/` クレートで `fn main()` + `println!` + `String` + `Vec` が動作
- release ビルド（`opt-level = "z"`, LTO, strip）で 29KB の ELF を生成

### PAL 実装状況

| PAL モジュール | 状態 | 実装内容 |
|---------------|------|---------|
| **pal/sabos** | ✅ 実装済み | `_start` → `main()` → `exit()` のエントリポイント |
| **alloc** | ✅ 実装済み | SYS_MMAP/SYS_MUNMAP ベースの GlobalAlloc |
| **stdio** | ✅ 実装済み | SYS_WRITE/SYS_READ ベースの Stdout/Stdin |
| **random** | ✅ 実装済み | SYS_GETRANDOM ベースの fill_bytes |
| **thread_local** | ✅ 設定済み | `no_threads` モード（Cell ベース） |
| **args** | ❌ unsupported | `std::env::args()` は空を返す |
| **env** | ✅ 実装済み | SYS_GETENV/SYS_SETENV ベースの var/set_var（一覧取得は未対応） |
| **fs** | ✅ 実装済み | SYS_OPEN/READ/WRITE/CLOSE/STAT/SEEK ベースの File + readdir/unlink/rmdir |
| **net** | ❌ unsupported | `std::net::*` はエラーを返す |
| **os** | ✅ 実装済み | exit/getpid + getcwd/temp_dir/home_dir |
| **thread** | ❌ unsupported | `std::thread::spawn()` はエラーを返す |
| **time** | ✅ 実装済み | SYS_CLOCK_MONOTONIC ベースの Instant（SystemTime は未対応） |
| **process** | ❌ unsupported | `std::process::Command` はエラーを返す |
| **sync** | ✅ 設定済み | `no_threads` モード（シングルスレッド用） |

## TODO リスト

### 完了済み

- [x] **Phase 1: `println!` マクロの提供** — std の println! が PAL 経由で動作
- [x] **Phase 2: ファイルシステムの基盤** — SYS_HANDLE_WRITE / SYS_HANDLE_SEEK / SYS_FS_STAT 実装済み（no_std バイナリから利用可能）
- [x] **Phase 3: 時刻・乱数** — SYS_CLOCK_MONOTONIC / SYS_GETRANDOM 実装済み
- [x] **Phase 4: 動的メモリ管理** — SYS_MMAP / SYS_MUNMAP 実装済み
- [x] **Phase 5: 同期プリミティブ** — SYS_FUTEX_WAIT / SYS_FUTEX_WAKE 実装済み
- [x] **Phase 6: ネットワーク抽象化** — user/src/net.rs に TcpStream / TcpListener / DNS
- [x] **Phase 7: カスタムターゲットと `-Zbuild-std`** — 基本動作確認済み

### Phase 8: PAL の充実（次のステップ）

既にカーネル側に syscall が存在するが、PAL に接続されていないものを繋ぐ。

- [x] **PAL fs の実装**
  - `sys_fs_sabos.rs` + `os_sabos_mod.rs` + `os_sabos_ffi.rs` を追加
  - `std::fs::read_to_string()` / `std::fs::write()` / `std::fs::metadata()` が動作
  - `_start` のスタックアラインメント修正（GPF 対策）も含む

- [x] **PAL time の実装**
  - `sys_time_sabos.rs` を追加
  - SYS_CLOCK_MONOTONIC(26) を PAL の `time::Instant` に接続
  - `std::time::Instant::now()` / `elapsed()` が動作
  - `SystemTime` は RTC 未実装のため unsupported

- [x] **PAL os の充実 + env の実装**
  - `sys_pal_sabos_os.rs` を改善: getcwd → "/", temp_dir → "/", home_dir → "/"
  - `sys_env_sabos.rs` を新規作成: SYS_GETENV(37)/SYS_SETENV(38) に接続
  - `std::env::var()` / `std::env::set_var()` / `std::env::current_dir()` が動作
  - env 一覧取得（`std::env::vars()`）は空を返す（SYS_LISTENV 未実装のため）

- [ ] **PAL net の実装**
  - 難易度: ★★★★☆
  - netd 経由の TCP を PAL の `net::TcpStream` に接続
  - `std::net::TcpStream::connect()` が使えるようになる
  - IPC ベースの netd 通信を PAL 内部に隠蔽する必要がある

### Phase 9: 外部クレート対応

- [ ] **コマンドライン引数の受け渡し**
  - 難易度: ★★☆☆☆
  - SYS_EXEC / SYS_SPAWN に引数文字列を渡せるようにする
  - PAL の args に接続して `std::env::args()` が使えるようになる
  - 多くの CLI クレート（clap 等）の前提条件

- [ ] **環境変数**
  - 難易度: ★★☆☆☆
  - プロセスごとの環境変数テーブル
  - `std::env::var()` / `std::env::set_var()` が使えるようになる

- [ ] **外部クレートのビルドテスト**
  - 難易度: ★★★☆☆
  - `serde_json` や `regex` など代表的なクレートが `user-std/` でビルドできるか確認
  - 不足している PAL 機能を洗い出す

### 残課題

- [ ] **debug ビルドの OOM 問題の改善**
  - 現状: debug ビルドの ELF が 6.4MB で、カーネルヒープ (16MB) 上に Vec で読むと OOM
  - 原因: カーネルの ELF ローダーが `Vec<u8>` でファイル全体をヒープに読み込む
  - 対策案: ストリーミング読み込み、またはカーネルヒープサイズ増加

- [ ] **nightly 更新時の sysroot パッチ追従**
  - `scripts/patch-rust-sysroot.sh` は idempotent 設計だが、std のソース構造が変わるとパッチが壊れる
  - `rust-toolchain.toml` でバージョン固定することで緩和可能

## ビルド手順

```bash
# 1. sysroot にパッチを適用（初回 or nightly 更新時）
make patch-sysroot

# 2. std 対応バイナリをビルド（release）
make build-user-std

# 3. テスト
make test  # HELLOSTD.ELF テストを含む 40/40 + α
```

## 関連ファイル

- `x86_64-sabos.json` — カスタムターゲット定義
- `rust-std-sabos/` — sysroot パッチファイル（PAL + alloc + stdio + random）
- `scripts/patch-rust-sysroot.sh` — パッチ適用スクリプト
- `scripts/apply-sysroot-patches.py` — Python パッチエンジン
- `user-std/` — std 対応バイナリ用クレート
- `user-std/.cargo/config.toml` — `-Zbuild-std` の設定
