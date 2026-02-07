# TODO: Rust std ライブラリ対応ロードマップ

SABOS のユーザープログラムで `std` クレートを使えるようにするための TODO リスト。
`std` 環境が使えれば `alloc::format!` の代わりに `println!`、`alloc::string::String` の代わりに `std::string::String`、
そして外部の std 対応クレートが利用可能になり、開発効率が大幅に上がる。

## 背景: Rust std が OS に要求するもの

Rust の std ライブラリは Platform Abstraction Layer (PAL) を通じて OS 機能にアクセスする。
PAL は `library/std/src/sys/pal/<os>/` に配置され、以下のモジュールを実装する必要がある：

| PAL モジュール | 必要な OS 機能 | SABOS の対応状況 |
|---------------|---------------|-----------------|
| **alloc** | ヒープメモリ確保 (mmap/brk 相当) | △ 静的バンプアロケータのみ |
| **stdio** | stdout/stderr/stdin | ✅ SYS_READ/SYS_WRITE |
| **args** | コマンドライン引数 | ❌ 未実装 |
| **env** | 環境変数 | ❌ 未実装 |
| **fs** | ファイル操作 (open/read/write/close/stat) | △ read は実装済み、write/stat 未実装 |
| **io** | I/O エラー型、ヘルパー | △ 基盤はある |
| **net** | ソケット (TCP/UDP) | △ TCP のみ（netd 経由） |
| **os** | OS 固有の型・定数 | ❌ 未実装 |
| **path** | パス操作 | ❌ 未実装（OS 依存のセパレータ等） |
| **pipe** | パイプ | ❌ 未実装 |
| **process** | プロセス生成・終了 | △ spawn/exec/exit あり |
| **thread** | スレッド生成・join | ❌ 未実装（プロセスベースのみ） |
| **time** | 時刻取得 (clock_gettime 相当) | ❌ 未実装 |
| **rand** | 乱数ソース | ❌ 未実装 |
| **mutex/condvar/rwlock** | 同期プリミティブ | ❌ 未実装 |

## 実装アプローチ

### 方法 A: rustc をフォークして `target_os = "sabos"` の PAL を追加

Redox OS が採用している方法。rustc のソースに SABOS 用の PAL を追加し、`-Zbuild-std` でビルドする。

**メリット:** 正攻法、std の全機能が使える
**デメリット:** rustc フォークのメンテナンスコストが高い

### 方法 B: `x86_64-unknown-none` のまま、std 互換レイヤーを自前で提供

`no_std` のまま、必要な std の型・トレイトだけを re-export するクレートを作る。

**メリット:** フォーク不要、段階的に進められる
**デメリット:** 外部クレートの `use std::*` がそのままでは動かない

### 方法 C: カスタムターゲット JSON + `-Zbuild-std`

`x86_64-sabos.json` というカスタムターゲットを定義し、`-Zbuild-std` で std を再ビルド。
PAL の `unsupported` モジュールをベースに、対応可能なものだけ実装する。

**メリット:** フォーク不要、nightly の `-Zbuild-std` を使える
**デメリット:** unstable 機能に依存、PAL の組み込みが難しい場合がある

### 推奨: 方法 B → 方法 C の段階的移行

1. まずは方法 B で `sabos-std` クレートを作り、`println!` や `String` を使えるようにする
2. カーネル側のシステムコールを充実させる（下の TODO リスト）
3. 十分なシステムコールが揃ったら方法 C でフル std 対応を目指す

## TODO リスト（簡単な順）

### Phase 1: 即座に対応できるもの（既存 syscall の整理）

- [ ] **`println!` マクロの提供**
  - 難易度: ★☆☆☆☆
  - `SYS_WRITE` をバックエンドにした `core::fmt::Write` 実装は簡単
  - `sabos-std` クレートに `println!` / `eprintln!` マクロを定義
  - これだけで `format!` + `write_str` の手書きが不要になる

- [ ] **コマンドライン引数の受け渡し**
  - 難易度: ★★☆☆☆
  - `SYS_EXEC` / `SYS_SPAWN` に引数文字列を渡せるようにする
  - カーネル側: 新しいタスクのスタックに argc/argv 相当を積む
  - ユーザー側: `_start` で引数を受け取る仕組み

- [ ] **環境変数（簡易版）**
  - 難易度: ★★☆☆☆
  - プロセスごとの環境変数テーブルをカーネルまたはユーザー空間で管理
  - `SYS_GETENV(key_ptr, key_len, val_ptr, val_len) -> n`
  - `SYS_SETENV(key_ptr, key_len, val_ptr, val_len) -> 0`

### Phase 2: ファイルシステムの完成

- [ ] **`SYS_HANDLE_WRITE` の実装（ファイル書き込み）**
  - 難易度: ★★☆☆☆
  - syscall 番号 72 は定義済み、カーネル側の FAT32 書き込みロジックを接続するだけ
  - `std::fs::write()` / `std::io::Write` の基盤

- [ ] **ファイル stat（メタデータ取得）**
  - 難易度: ★★☆☆☆
  - ファイルサイズ、作成日時、属性を返す syscall
  - `SYS_HANDLE_STAT(handle_ptr, stat_ptr) -> 0`
  - `std::fs::metadata()` の基盤

- [ ] **ファイル seek（ポジション変更）**
  - 難易度: ★★☆☆☆
  - `SYS_HANDLE_SEEK(handle_ptr, offset, whence) -> new_pos`
  - `std::io::Seek` トレイトの基盤

- [ ] **ディレクトリ作成の handle ベース化**
  - 難易度: ★★☆☆☆
  - 現在 `mkdir` は handle ベースだが、`SYS_HANDLE_CREATE` 等の整理

### Phase 3: 時刻・乱数

- [ ] **時刻取得 syscall**
  - 難易度: ★★★☆☆
  - HPET / TSC / PIT からの時刻取得をカーネルで実装
  - `SYS_CLOCK_GETTIME(clock_id, timespec_ptr) -> 0`
  - `std::time::Instant` / `std::time::SystemTime` の基盤

- [ ] **乱数ソース**
  - 難易度: ★★☆☆☆
  - RDRAND 命令があれば簡単（x86_64 なら大抵ある）
  - `SYS_GETRANDOM(buf_ptr, len) -> n`
  - `std::collections::HashMap` のハッシュシードにも必要

### Phase 4: 動的メモリ管理の改善

- [ ] **mmap 相当の syscall**
  - 難易度: ★★★☆☆
  - ユーザー空間に新しいページをマップする
  - `SYS_MMAP(addr_hint, len, prot, flags) -> addr`
  - `SYS_MUNMAP(addr, len) -> 0`
  - 現在の 256 KiB 固定バンプアロケータを置き換え
  - `std` の GlobalAlloc の基盤

- [ ] **brk / sbrk 相当**
  - 難易度: ★★☆☆☆
  - mmap より簡単な代替案。ヒープの終端を伸ばす
  - 既存のバンプアロケータを拡張可能にするだけでも良い

### Phase 5: スレッドと同期プリミティブ

- [ ] **スレッド生成 syscall**
  - 難易度: ★★★★☆
  - 同じアドレス空間内で新しいスレッドを作る
  - `SYS_THREAD_CREATE(entry_ptr, stack_ptr, arg_ptr) -> thread_id`
  - `SYS_THREAD_JOIN(thread_id, timeout_ms) -> exit_code`
  - ページテーブル共有の仕組みが必要

- [ ] **Mutex / Condvar**
  - 難易度: ★★★☆☆
  - futex 相当の syscall を提供
  - `SYS_FUTEX_WAIT(addr, expected_val, timeout_ms) -> 0`
  - `SYS_FUTEX_WAKE(addr, count) -> n`
  - ユーザー空間で spin + futex のハイブリッド mutex を実装

- [ ] **Thread Local Storage (TLS)**
  - 難易度: ★★★★☆
  - ELF の TLS セクション対応、または手動 TLS
  - `std` の初期化に必須

### Phase 6: ネットワークの std 対応

- [x] **ソケット抽象化**
  - 難易度: ★★★☆☆
  - `user/src/net.rs` に TcpStream / TcpListener / DNS API を実装
  - shell.rs / httpd.rs / telnetd.rs から重複コード ~380 行を削除
  - Drop で自動クローズ（RAII パターン）、低レベル raw API も提供
  - UDP 対応は未実装（将来の課題）

- [x] **DNS リゾルバの std 対応**
  - 難易度: ★★☆☆☆
  - `net::dns_lookup()` で名前解決。`std::net::ToSocketAddrs` は将来課題

### Phase 7: カスタムターゲットと `-Zbuild-std`

- [ ] **`x86_64-sabos.json` カスタムターゲット定義**
  - 難易度: ★★★☆☆
  - `x86_64-unknown-none` をベースに `os = "sabos"` を設定
  - リンカスクリプト、ABI の設定

- [ ] **PAL 実装 (`sys/pal/sabos/`)**
  - 難易度: ★★★★★
  - Phase 1〜6 で実装した syscall を PAL のインターフェースに接続
  - `unsupported` モジュールをベースに、対応可能なものだけ実装

- [ ] **`-Zbuild-std` でのビルド確認**
  - 難易度: ★★★☆☆
  - `cargo build -Zbuild-std=std,core,alloc` でビルドが通ることを確認

## 先行事例

### Redox OS
- Rust で書かれた Unix 風マイクロカーネル OS
- rustc をフォークして `target_os = "redox"` を追加
- `relibc` (C ライブラリ) を経由して std を実装
- 参考: https://gitlab.redox-os.org/redox-os/relibc

### Theseus OS
- Rust で書かれた研究用 OS
- `no_std` のまま独自の安全な抽象化を構築
- std は使わずに Rust の型システムを活用

### OSDev Wiki
- Porting Rust standard library の手順がまとまっている
- 参考: https://wiki.osdev.org/Porting_Rust_standard_library

## まず最初にやること

**Phase 1 の `println!` マクロ提供** が最もインパクト大で簡単。
`SYS_WRITE` は既に動いているので、以下だけで実現できる：

```rust
// sabos-std/src/lib.rs (新クレート)
#![no_std]

use core::fmt;

struct StdoutWriter;

impl fmt::Write for StdoutWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        syscall::write(s.as_bytes());
        Ok(())
    }
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let mut w = $crate::StdoutWriter;
        let _ = writeln!(w, $($arg)*);
    }};
}
```

これだけで `println!("Hello, {} frames free!", free_count)` が書ける。
