# TODO: Rust std ライブラリ対応ロードマップ

SABOS のユーザープログラムで `std` クレートを使えるようにするための TODO リスト。
Phase 9 まで完了し、`std::env::args()` + 外部クレート（`serde_json`）が動作する状態。

## 現在の状況

**方法 C（カスタムターゲット JSON + `-Zbuild-std`）を採用**し、以下が動作している:

- `x86_64-sabos.json` カスタムターゲット（`os = "sabos"`）
- sysroot パッチ方式で PAL を追加（`scripts/patch-rust-sysroot.sh`）
- `user-std/` クレートで `fn main()` + `println!` + `String` + `Vec` が動作
- release ビルド（`opt-level = "z"`, LTO, strip）で 93KB の ELF を生成（serde_json 含む）
- `RUSTC_BOOTSTRAP_SYNTHETIC_TARGET=1` で外部クレートの `restricted_std` 問題を回避
- `serde` + `serde_json` がビルド・動作確認済み

### PAL 実装状況

| PAL モジュール | 状態 | 実装内容 |
|---------------|------|---------|
| **pal/sabos** | ✅ 実装済み | `_start` → `main()` → `exit()` のエントリポイント |
| **alloc** | ✅ 実装済み | SYS_MMAP/SYS_MUNMAP ベースの GlobalAlloc |
| **stdio** | ✅ 実装済み | SYS_WRITE/SYS_READ ベースの Stdout/Stdin |
| **random** | ✅ 実装済み | SYS_GETRANDOM ベースの fill_bytes |
| **thread_local** | ✅ 設定済み | `no_threads` モード（Cell ベース） |
| **args** | ✅ 実装済み | カーネルの argc/argv を Atomic 変数で保存、`std::env::args()` 対応 |
| **env** | ✅ 実装済み | SYS_GETENV/SYS_SETENV ベースの var/set_var（一覧取得は未対応） |
| **fs** | ✅ 実装済み | SYS_OPEN/READ/WRITE/CLOSE/STAT/SEEK ベースの File + readdir/unlink/rmdir |
| **net** | ✅ 実装済み | IPC 経由で netd に接続、DNS/TcpStream/TcpListener 対応（UDP/IPv6 は未対応） |
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

- [x] **PAL net の実装**
  - `sys_net_connection_sabos.rs` を追加
  - IPC syscall (SYS_IPC_SEND/RECV) で netd と直接通信
  - `std::net::TcpStream::connect()` / `TcpListener::bind()` / DNS lookup が動作
  - UdpSocket は unsupported（netd が UDP 未対応のため）
  - IPv6 は unsupported（SABOS は IPv4 のみ）

### Phase 9: コマンドライン引数 + 外部クレート対応

- [x] **コマンドライン引数の受け渡し**
  - PAL の `sys_args_sabos.rs` を新規作成: Unix 実装に倣い Atomic 変数で argc/argv を保存
  - `_start_rust()` に argc/argv を渡すよう修正（System V ABI でレジスタ伝播）
  - SYS_EXEC / SYS_SPAWN を 4 引数に拡張（arg3=args_ptr, arg4=args_len）
  - 引数バッファフォーマット: `[u16 len][bytes]` の繰り返し（長さプレフィックス形式）
  - シェルの run / spawn コマンドも引数対応
  - `std::env::args()` が動作確認済み

- [x] **外部クレートのビルドテスト**
  - `serde` + `serde_json` が SABOS 上でビルド・動作することを確認
  - `RUSTC_BOOTSTRAP_SYNTHETIC_TARGET=1` で `restricted_std` 問題を解決
  - JSON のシリアライズ/デシリアライズが正常動作

### Phase 10 以降の候補: 未実装 PAL モジュール

以下は現在 unsupported だが、カーネル側に基盤がある or 実装可能なもの。

- [ ] **PAL thread の実装**
  - 難易度: ★★★☆☆
  - カーネル側に SYS_THREAD_CREATE(110) / SYS_THREAD_EXIT(111) / SYS_THREAD_JOIN(112) + SYS_FUTEX(120) が既に存在
  - PAL の `sys/thread/sabos.rs` を作成して `std::thread::spawn()` を動かす
  - `std::sync::Mutex` / `Condvar` も Futex ベースで動くはず
  - thread_local は `no_threads` → `thread_local_key` に切り替えが必要
  - 影響: `rayon`, `tokio`, `crossbeam` などの並行処理クレートが使えるようになる

- [ ] **PAL process の実装**
  - 難易度: ★★☆☆☆
  - SYS_EXEC / SYS_SPAWN / SYS_WAIT が既にあるので PAL に接続するだけ
  - `std::process::Command::new("/FOO.ELF").arg("bar").spawn()` が動くようになる
  - `std::process::exit()` は既に動作中

- [ ] **SystemTime の実装**
  - 難易度: ★★★☆☆
  - RTC（リアルタイムクロック）のドライバ実装が必要
  - CMOS RTC から年月日時分秒を読み取る SYS_CLOCK_REALTIME を追加
  - `std::time::SystemTime::now()` / `UNIX_EPOCH` が使えるようになる
  - 影響: `chrono`, `time` クレートが動く

- [ ] **env::vars() の実装（環境変数一覧）**
  - 難易度: ★☆☆☆☆
  - SYS_LISTENV を追加してタスクの全環境変数を返す
  - `std::env::vars()` イテレータが空でなくなる

- [ ] **net: UdpSocket / IPv6**
  - 難易度: ★★★★☆
  - netd に UDP プロトコル処理を追加、IPv6 スタック実装
  - 現状 TCP + IPv4 のみ

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
