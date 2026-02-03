// syscall.rs — システムコールハンドラ
//
// Ring 3（ユーザーモード）から `int 0x80` で呼び出されるシステムコールの処理を行う。
//
// システムコール（system call）とは、ユーザープログラムがカーネルの機能を
// 利用するための仕組み。Ring 3 のコードは直接ハードウェアにアクセスできないため、
// ソフトウェア割り込み `int 0x80` を使って CPU の特権レベルを Ring 0 に上げ、
// カーネルのコードを実行する。
//
// レジスタ規約（Linux の int 0x80 規約に準拠）:
//   rax = システムコール番号
//   rdi = 第1引数
//   rsi = 第2引数
//   戻り値は rax に格納される
//
// アセンブリエントリポイント (syscall_handler_asm):
//   1. 汎用レジスタを保存
//   2. Microsoft x64 ABI に合わせて引数を rcx/rdx/r8 にセット
//   3. Rust の syscall_dispatch() を呼び出す
//   4. 汎用レジスタを復帰（rax は戻り値として上書き）
//   5. iretq でユーザーモードに復帰
//
// 注意: x86_64-unknown-uefi ターゲットでは extern "C" が Microsoft x64 ABI になる。
// System V ABI（Linux）とは引数の渡し方が異なるので注意。
//
// ## 設計原則（CLAUDE.md より）
//
// - null 終端文字列を使わない: すべてのバッファは (ptr, len) 形式
// - UserSlice<T> で型安全にラップ: ユーザー空間ポインタを検証してからアクセス
// - SyscallError で明確なエラー型: 生の数値ではなく型付きエラーを使用

use core::arch::global_asm;
use alloc::string::String;
use alloc::vec::Vec;
use crate::user_ptr::{UserPtr, UserSlice, SyscallError};

/// システムコール番号の定義
///
/// 番号体系は計画に従う:
/// - コンソール I/O: 0-9
/// - ファイルシステム: 10-19
/// - システム情報: 20-29
/// - プロセス管理: 30-39
/// - ネットワーク: 40-49
/// - システム制御: 50-59
/// - 終了: 60
/// - ファイルハンドル: 70-79
// コンソール I/O (0-9)
pub const SYS_READ: u64 = 0;         // read(buf_ptr, len) — コンソールから読み取り
pub const SYS_WRITE: u64 = 1;        // write(buf_ptr, len) — 文字列をカーネルコンソールに出力
pub const SYS_CLEAR_SCREEN: u64 = 2; // clear_screen() — 画面をクリア

// ファイルシステム (10-19)
pub const SYS_FILE_READ: u64 = 10;   // file_read(path_ptr, path_len, buf_ptr, buf_len) — ファイル読み取り
pub const SYS_FILE_WRITE: u64 = 11;  // file_write(path_ptr, path_len, data_ptr, data_len) — ファイル書き込み
pub const SYS_FILE_DELETE: u64 = 12; // file_delete(path_ptr, path_len) — ファイル削除
pub const SYS_DIR_LIST: u64 = 13;    // dir_list(path_ptr, path_len, buf_ptr, buf_len) — ディレクトリ一覧

// システム情報 (20-29)
pub const SYS_GET_MEM_INFO: u64 = 20;   // get_mem_info(buf_ptr, buf_len) — メモリ情報取得
pub const SYS_GET_TASK_LIST: u64 = 21;  // get_task_list(buf_ptr, buf_len) — タスク一覧取得
pub const SYS_GET_NET_INFO: u64 = 22;   // get_net_info(buf_ptr, buf_len) — ネットワーク情報取得
pub const SYS_PCI_CONFIG_READ: u64 = 23; // pci_config_read(bus, device, function, offset, size) — PCI Config 読み取り

// プロセス管理 (30-39)
pub const SYS_EXEC: u64 = 30;    // exec(path_ptr, path_len) — プログラムを同期実行
pub const SYS_SPAWN: u64 = 31;   // spawn(path_ptr, path_len) — バックグラウンドでプロセス起動
pub const SYS_YIELD: u64 = 32;   // yield() — CPU を譲る
pub const SYS_SLEEP: u64 = 33;   // sleep(ms) — 指定ミリ秒スリープ

// ネットワーク (40-49)
pub const SYS_DNS_LOOKUP: u64 = 40;  // dns_lookup(domain_ptr, domain_len, ip_ptr) — DNS 解決
pub const SYS_TCP_CONNECT: u64 = 41; // tcp_connect(ip_ptr, port) — TCP 接続
pub const SYS_TCP_SEND: u64 = 42;    // tcp_send(data_ptr, data_len) — TCP 送信
pub const SYS_TCP_RECV: u64 = 43;    // tcp_recv(buf_ptr, buf_len, timeout_ms) — TCP 受信
pub const SYS_TCP_CLOSE: u64 = 44;   // tcp_close() — TCP 切断

// システム制御 (50-59)
pub const SYS_HALT: u64 = 50;        // halt() — システム停止

// 終了 (60)
pub const SYS_EXIT: u64 = 60;        // exit() — ユーザープログラムを終了してカーネルに戻る

// ファイルハンドル (70-79)
pub const SYS_OPEN: u64 = 70;         // open(path_ptr, path_len, handle_ptr, rights)
pub const SYS_HANDLE_READ: u64 = 71;  // handle_read(handle_ptr, buf_ptr, len)
pub const SYS_HANDLE_WRITE: u64 = 72; // handle_write(handle_ptr, buf_ptr, len)
pub const SYS_HANDLE_CLOSE: u64 = 73; // handle_close(handle_ptr)

// =================================================================
// アセンブリエントリポイント
// =================================================================
//
// int 0x80 が発火すると CPU は自動的に以下を行う:
//   1. TSS の rsp0 からカーネルスタックに切り替え
//   2. SS, RSP, RFLAGS, CS, RIP をカーネルスタックに push
//   3. IDT 0x80 番のハンドラ（= syscall_handler_asm）にジャンプ
//
// ハンドラ側では汎用レジスタを保存し、Rust 関数を呼び、
// レジスタを復帰して iretq でユーザーモードに戻る。

global_asm!(
    ".global syscall_handler_asm",
    "syscall_handler_asm:",

    // --- 汎用レジスタの保存 ---
    // int 0x80 で CPU が自動保存するのは SS/RSP/RFLAGS/CS/RIP のみ。
    // 残りの汎用レジスタは手動で保存する必要がある。
    "push r11",
    "push r10",
    "push r9",
    "push r8",
    "push rdi",
    "push rsi",
    "push rdx",
    "push rcx",
    "push rbx",
    "push rbp",

    // --- Rust の syscall_dispatch(nr, arg1, arg2, arg3, arg4) を呼び出す ---
    // UEFI ターゲットは Microsoft x64 ABI を使用する。
    // Microsoft x64 ABI の引数渡し:
    //   第1引数: rcx, 第2引数: rdx, 第3引数: r8, 第4引数: r9
    //   第5引数以降はスタック経由
    //
    // int 0x80 のレジスタ規約（Linux 風）:
    //   rax = syscall番号, rdi = arg1, rsi = arg2, rdx = arg3, r10 = arg4
    //
    // レジスタの移動（順序が重要: 後で使うレジスタを先に移動）:
    // 注意: rdx は Linux ABI では arg3 だが、保存した値を使う必要がある
    // スタックに保存された rdx の位置: rbp(0) + rbx(8) + rcx(16) + rdx(24) からの位置
    // = [rsp + 24] が保存された rdx

    // スタックを 16 バイトアラインする（ABI 要件）
    // push を 10 回 + CPU が 5 個 push = 15 個 × 8 = 120 バイト
    // 120 % 16 = 8 なので、8 バイト追加して 16 の倍数にする。
    // さらに Microsoft x64 ABI ではシャドウスペース（32バイト）が必要。
    // 合計: 8 (アライン) + 32 (シャドウ) = 40 バイト確保
    "sub rsp, 40",

    // 第5引数 (arg4 = r10) をスタックに積む（Microsoft ABI では第5引数以降はスタック）
    "mov qword ptr [rsp+32], r10",  // arg4 → スタックの第5引数位置

    // 引数をセット（Microsoft ABI: rcx, rdx, r8, r9）
    "mov r9, rdx",    // arg3 (rdx) → 第4引数 (r9) ※先に移動
    "mov r8, rsi",    // arg2 (rsi) → 第3引数 (r8)
    "mov rdx, rdi",   // arg1 (rdi) → 第2引数 (rdx)
    "mov rcx, rax",   // syscall_nr (rax) → 第1引数 (rcx)

    // syscall_dispatch を呼び出す
    "call syscall_dispatch",

    // スタックの調整を元に戻す
    "add rsp, 40",

    // 戻り値は rax に入っている。このまま保持する。

    // --- 汎用レジスタの復帰 ---
    // rax は syscall_dispatch の戻り値なので復帰しない（ユーザーに返す値）
    "pop rbp",
    "pop rbx",
    "pop rcx",
    "pop rdx",
    "pop rsi",
    "pop rdi",
    "pop r8",
    "pop r9",
    "pop r10",
    "pop r11",

    // --- iretq でユーザーモードに復帰 ---
    // CPU が自動的に push した SS/RSP/RFLAGS/CS/RIP を pop して
    // Ring 3 の実行を再開する。
    "iretq",
);

// アセンブリで定義したシンボルを Rust から参照できるようにする
unsafe extern "C" {
    pub safe fn syscall_handler_asm();
}

// =================================================================
// Rust ディスパッチ関数
// =================================================================

/// システムコールのディスパッチ関数。
/// アセンブリエントリポイントから呼ばれる。
///
/// 引数:
///   nr   — システムコール番号（rax から渡される）
///   arg1 — 第1引数（rdi から渡される）
///   arg2 — 第2引数（rsi から渡される）
///   arg3 — 第3引数（rdx から渡される）
///   arg4 — 第4引数（r10 から渡される）
///
/// 戻り値:
///   rax に格納されてユーザープログラムに返される
///   エラーの場合は負の値（SyscallError::to_errno()）
#[unsafe(no_mangle)]
extern "C" fn syscall_dispatch(nr: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> u64 {
    // 各システムコールハンドラを呼び出し、Result を u64 に変換
    let result = dispatch_inner(nr, arg1, arg2, arg3, arg4);
    match result {
        Ok(value) => value,
        Err(err) => err.to_errno(),
    }
}

/// システムコールの内部ディスパッチ関数
///
/// Result 型を返すことで、エラーハンドリングを型安全に行う。
/// ? 演算子でエラーを早期リターンできる。
fn dispatch_inner(nr: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    match nr {
        SYS_READ => sys_read(arg1, arg2),
        SYS_WRITE => sys_write(arg1, arg2),
        SYS_CLEAR_SCREEN => sys_clear_screen(),
        // ファイルシステム
        SYS_FILE_READ => sys_file_read(arg1, arg2, arg3, arg4),
        SYS_FILE_WRITE => sys_file_write(arg1, arg2, arg3, arg4),
        SYS_FILE_DELETE => sys_file_delete(arg1, arg2),
        SYS_DIR_LIST => sys_dir_list(arg1, arg2, arg3, arg4),
        // システム情報
        SYS_GET_MEM_INFO => sys_get_mem_info(arg1, arg2),
        SYS_GET_TASK_LIST => sys_get_task_list(arg1, arg2),
        SYS_GET_NET_INFO => sys_get_net_info(arg1, arg2),
        SYS_PCI_CONFIG_READ => sys_pci_config_read(arg1, arg2, arg3, arg4),
        // プロセス管理
        SYS_EXEC => sys_exec(arg1, arg2),
        SYS_SPAWN => sys_spawn(arg1, arg2),
        SYS_YIELD => sys_yield(),
        SYS_SLEEP => sys_sleep(arg1),
        // ネットワーク
        SYS_DNS_LOOKUP => sys_dns_lookup(arg1, arg2, arg3),
        SYS_TCP_CONNECT => sys_tcp_connect(arg1, arg2),
        SYS_TCP_SEND => sys_tcp_send(arg1, arg2),
        SYS_TCP_RECV => sys_tcp_recv(arg1, arg2, arg3),
        SYS_TCP_CLOSE => sys_tcp_close(),
        // ハンドル
        SYS_OPEN => sys_open(arg1, arg2, arg3, arg4),
        SYS_HANDLE_READ => sys_handle_read(arg1, arg2, arg3),
        SYS_HANDLE_WRITE => sys_handle_write(arg1, arg2, arg3),
        SYS_HANDLE_CLOSE => sys_handle_close(arg1),
        // システム制御
        SYS_HALT => sys_halt(),
        SYS_EXIT => {
            // exit()
            // ユーザープログラムの終了を要求する。
            // 保存されたカーネルスタック（RSP/RBP）を復元して
            // run_in_usermode() の呼び出し元に return する。
            // この関数は戻らない
            crate::usermode::exit_usermode();
        }
        _ => {
            // 未知のシステムコール番号
            crate::kprintln!("Unknown syscall: {}", nr);
            Err(SyscallError::UnknownSyscall)
        }
    }
}

/// SYS_READ: コンソールから読み取り
///
/// 引数:
///   arg1 — バッファのポインタ（ユーザー空間、書き込み先）
///   arg2 — バッファの長さ（最大読み取りバイト数）
///
/// 戻り値:
///   読み取ったバイト数
///
/// 少なくとも1バイト読み取れるまでブロックする。
/// その後、利用可能なデータがあれば最大 len バイトまで読み取って返す。
fn sys_read(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let len = arg2 as usize;

    // 長さ 0 の場合は何もしない
    if len == 0 {
        return Ok(0);
    }

    // UserSlice で型安全にユーザー空間のバッファを取得
    let user_slice = UserSlice::<u8>::from_raw(arg1, len)?;

    // 可変スライスとしてアクセス（書き込み用）
    let buf = user_slice.as_mut_slice();

    // コンソール入力バッファから読み取り（ブロッキング）
    let bytes_read = crate::console::read_input(buf, len);

    Ok(bytes_read as u64)
}

/// SYS_WRITE: コンソールに文字列を出力
///
/// 引数:
///   arg1 — バッファのポインタ（ユーザー空間）
///   arg2 — バッファの長さ（バイト数）
///
/// 戻り値:
///   書き込んだバイト数
///
/// UserSlice を使って型安全にユーザー空間のバッファを検証してからアクセスする。
fn sys_write(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let len = arg2 as usize;

    // UserSlice で型安全にユーザー空間のバッファを取得
    // アドレス範囲、アラインメント、オーバーフローを検証
    let user_slice = UserSlice::<u8>::from_raw(arg1, len)?;

    // UTF-8 として解釈してカーネルコンソールに出力
    // as_str_lossy() は不正な UTF-8 を "<invalid utf-8>" に置換
    let s = user_slice.as_str_lossy();
    crate::kprint!("{}", s);

    // 書き込んだバイト数を返す
    Ok(len as u64)
}

/// SYS_CLEAR_SCREEN: 画面をクリア
///
/// 引数: なし
/// 戻り値: 0（成功）
fn sys_clear_screen() -> Result<u64, SyscallError> {
    crate::framebuffer::clear_global_screen();
    Ok(0)
}

// =================================================================
// ファイルシステム関連システムコール
// =================================================================

/// SYS_FILE_READ: ファイルの内容を読み取る
///
/// 引数:
///   arg1 — パスのポインタ（ユーザー空間）
///   arg2 — パスの長さ
///   arg3 — バッファのポインタ（ユーザー空間、書き込み先）
///   arg4 — バッファの長さ
///
/// 戻り値:
///   読み取ったバイト数（成功時）
///   負の値（エラー時）
fn sys_file_read(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    let path_len = arg2 as usize;
    let buf_len = arg4 as usize;

    // パスを取得
    let path_slice = UserSlice::<u8>::from_raw(arg1, path_len)?;
    let path = path_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // バッファを取得
    let buf_slice = UserSlice::<u8>::from_raw(arg3, buf_len)?;
    let buf = buf_slice.as_mut_slice();

    // /proc 配下は procfs で処理
    if path.starts_with("/proc") {
        let written = procfs_read(path, buf)?;
        return Ok(written as u64);
    }

    // FAT16 からファイルを読み取る
    let fat16 = crate::fat16::Fat16::new().map_err(|_| SyscallError::Other)?;
    let data = fat16.read_file(path).map_err(|_| SyscallError::FileNotFound)?;

    // バッファにコピー
    let copy_len = core::cmp::min(data.len(), buf_len);
    buf[..copy_len].copy_from_slice(&data[..copy_len]);

    Ok(copy_len as u64)
}

/// SYS_FILE_WRITE: ファイルを作成または上書き
///
/// 引数:
///   arg1 — パスのポインタ（ユーザー空間）
///   arg2 — パスの長さ
///   arg3 — データのポインタ（ユーザー空間）
///   arg4 — データの長さ
///
/// 戻り値:
///   書き込んだバイト数（成功時）
///   負の値（エラー時）
fn sys_file_write(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    let path_len = arg2 as usize;
    let data_len = arg4 as usize;

    // パスを取得
    let path_slice = UserSlice::<u8>::from_raw(arg1, path_len)?;
    let path = path_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // データを取得
    let data_slice = UserSlice::<u8>::from_raw(arg3, data_len)?;
    let data = data_slice.as_slice();

    // /proc 配下は読み取り専用
    if path.starts_with("/proc") {
        return Err(SyscallError::ReadOnly);
    }

    // FAT16 にファイルを書き込む
    let fat16 = crate::fat16::Fat16::new().map_err(|_| SyscallError::Other)?;
    fat16.create_file(path, data).map_err(|_| SyscallError::Other)?;

    Ok(data_len as u64)
}

// =================================================================
// ハンドル関連システムコール
// =================================================================

/// SYS_OPEN: ファイルを開いて Handle を返す
///
/// 引数:
///   arg1 — パスのポインタ（ユーザー空間）
///   arg2 — パスの長さ
///   arg3 — Handle の書き込み先ポインタ（ユーザー空間）
///   arg4 — rights（READ/WRITE 等のビット）
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
fn sys_open(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    use crate::handle::{self, Handle, HANDLE_RIGHT_READ, HANDLE_RIGHT_WRITE};

    let path_len = arg2 as usize;
    let rights = arg4 as u32;

    // パスを取得
    let path_slice = UserSlice::<u8>::from_raw(arg1, path_len)?;
    let path = path_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // Handle の書き込み先
    let handle_ptr = UserPtr::<Handle>::from_raw(arg3)?;

    // 現状は READ だけ許可
    if (rights & HANDLE_RIGHT_READ) == 0 {
        return Err(SyscallError::InvalidArgument);
    }
    if (rights & HANDLE_RIGHT_WRITE) != 0 {
        if path.starts_with("/proc") {
            return Err(SyscallError::ReadOnly);
        }
        return Err(SyscallError::NotSupported);
    }

    let handle = open_path_to_handle(path, rights)?;
    handle_ptr.write(handle);
    Ok(0)
}

/// SYS_HANDLE_READ: Handle から読み取る
///
/// 引数:
///   arg1 — Handle のポインタ（ユーザー空間）
///   arg2 — バッファのポインタ（ユーザー空間）
///   arg3 — バッファの長さ
///
/// 戻り値:
///   読み取ったバイト数（成功時）
///   負の値（エラー時）
fn sys_handle_read(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    use crate::handle::Handle;

    let buf_len = arg3 as usize;
    let handle_ptr = UserPtr::<Handle>::from_raw(arg1)?;
    let handle = handle_ptr.read();

    let buf_slice = UserSlice::<u8>::from_raw(arg2, buf_len)?;
    let buf = buf_slice.as_mut_slice();

    let n = crate::handle::read(&handle, buf)?;
    Ok(n as u64)
}

/// SYS_HANDLE_WRITE: Handle に書き込む
///
/// 引数:
///   arg1 — Handle のポインタ（ユーザー空間）
///   arg2 — バッファのポインタ（ユーザー空間）
///   arg3 — バッファの長さ
///
/// 戻り値:
///   書き込んだバイト数（成功時）
///   負の値（エラー時）
fn sys_handle_write(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    use crate::handle::Handle;

    let buf_len = arg3 as usize;
    let handle_ptr = UserPtr::<Handle>::from_raw(arg1)?;
    let handle = handle_ptr.read();

    let buf_slice = UserSlice::<u8>::from_raw(arg2, buf_len)?;
    let buf = buf_slice.as_slice();

    let n = crate::handle::write(&handle, buf)?;
    Ok(n as u64)
}

/// SYS_HANDLE_CLOSE: Handle を閉じる
///
/// 引数:
///   arg1 — Handle のポインタ（ユーザー空間）
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
fn sys_handle_close(arg1: u64) -> Result<u64, SyscallError> {
    use crate::handle::Handle;

    let handle_ptr = UserPtr::<Handle>::from_raw(arg1)?;
    let handle = handle_ptr.read();

    crate::handle::close(&handle)?;
    Ok(0)
}

/// SYS_FILE_DELETE: ファイルを削除
///
/// 引数:
///   arg1 — パスのポインタ（ユーザー空間）
///   arg2 — パスの長さ
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
fn sys_file_delete(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let path_len = arg2 as usize;

    // パスを取得
    let path_slice = UserSlice::<u8>::from_raw(arg1, path_len)?;
    let path = path_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // /proc 配下は読み取り専用
    if path.starts_with("/proc") {
        return Err(SyscallError::ReadOnly);
    }

    // FAT16 からファイルを削除
    let fat16 = crate::fat16::Fat16::new().map_err(|_| SyscallError::Other)?;
    fat16.delete_file(path).map_err(|_| SyscallError::FileNotFound)?;

    Ok(0)
}

/// SYS_DIR_LIST: ディレクトリの内容を一覧
///
/// 引数:
///   arg1 — パスのポインタ（ユーザー空間）"/" ならルート
///   arg2 — パスの長さ
///   arg3 — バッファのポインタ（ユーザー空間、エントリ名を改行区切りで書き込む）
///   arg4 — バッファの長さ
///
/// 戻り値:
///   書き込んだバイト数（成功時）
///   負の値（エラー時）
///
/// 出力形式:
///   ファイル名を改行区切りで出力。ディレクトリには末尾に "/" を付ける。
fn sys_dir_list(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    let path_len = arg2 as usize;
    let buf_len = arg4 as usize;

    // パスを取得
    let path_slice = UserSlice::<u8>::from_raw(arg1, path_len)?;
    let path = path_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // バッファを取得
    let buf_slice = UserSlice::<u8>::from_raw(arg3, buf_len)?;
    let buf = buf_slice.as_mut_slice();

    // /proc 配下は procfs で処理
    if path.starts_with("/proc") {
        let written = procfs_list_dir(path, buf)?;
        return Ok(written as u64);
    }

    // FAT16 からディレクトリ一覧を取得
    let fat16 = crate::fat16::Fat16::new().map_err(|_| SyscallError::Other)?;
    let entries = fat16.list_dir(path).map_err(|_| SyscallError::FileNotFound)?;

    // エントリ名を改行区切りでバッファに書き込む
    // ATTR_DIRECTORY = 0x10
    const ATTR_DIRECTORY: u8 = 0x10;

    let mut offset = 0;
    for entry in entries {
        let name = &entry.name;
        let is_dir = (entry.attr & ATTR_DIRECTORY) != 0;

        // 名前のバイト数 + 改行 (+ "/" for directories)
        let needed = name.len() + if is_dir { 2 } else { 1 };
        if offset + needed > buf_len {
            break;  // バッファがいっぱい
        }

        // 名前をコピー
        buf[offset..offset + name.len()].copy_from_slice(name.as_bytes());
        offset += name.len();

        // ディレクトリなら "/" を追加
        if is_dir {
            buf[offset] = b'/';
            offset += 1;
        }

        // 改行を追加
        buf[offset] = b'\n';
        offset += 1;
    }

    // ルートディレクトリには procfs を追加
    if path == "/" || path.is_empty() {
        let name = b"proc/";
        let needed = name.len() + 1;
        if offset + needed <= buf_len {
            buf[offset..offset + name.len()].copy_from_slice(name);
            offset += name.len();
            buf[offset] = b'\n';
            offset += 1;
        }
    }

    Ok(offset as u64)
}

// =================================================================
// システム情報関連システムコール
// =================================================================

// =================================================================
// procfs: 最小限の疑似ファイルシステム
// =================================================================

/// procfs のルートパス
const PROC_ROOT: &str = "/proc";
/// メモリ情報
const PROC_MEMINFO: &str = "/proc/meminfo";
/// タスク一覧
const PROC_TASKS: &str = "/proc/tasks";

/// procfs のファイルを読み取る
///
/// 対象ファイルが存在しない場合は FileNotFound を返す。
pub(crate) fn procfs_read(path: &str, buf: &mut [u8]) -> Result<usize, SyscallError> {
    match path {
        PROC_MEMINFO => Ok(write_mem_info(buf)),
        PROC_TASKS => Ok(write_task_list(buf)),
        _ => Err(SyscallError::FileNotFound),
    }
}

/// procfs の内容を Vec に読み取る（必要ならバッファを拡張）
fn procfs_read_to_vec(path: &str) -> Result<Vec<u8>, SyscallError> {
    let mut size = 256usize;
    loop {
        let mut buf = vec![0u8; size];
        let written = procfs_read(path, &mut buf)?;
        if written < size {
            buf.truncate(written);
            return Ok(buf);
        }
        size = size.saturating_mul(2);
        if size > 64 * 1024 {
            return Err(SyscallError::Other);
        }
    }
}

/// パスから Handle を作成する
pub(crate) fn open_path_to_handle(path: &str, rights: u32) -> Result<crate::handle::Handle, SyscallError> {
    if path == PROC_ROOT || path == "/proc/" {
        return Err(SyscallError::NotSupported);
    }

    if path.starts_with("/proc") {
        let data = procfs_read_to_vec(path)?;
        return Ok(crate::handle::create_handle(data, rights));
    }

    // FAT16 から読み取り
    let fat16 = crate::fat16::Fat16::new().map_err(|_| SyscallError::Other)?;
    let data = fat16.read_file(path).map_err(|_| SyscallError::FileNotFound)?;
    Ok(crate::handle::create_handle(data, rights))
}

/// procfs のディレクトリ一覧を取得する
///
/// /proc のみ対応。それ以外は FileNotFound。
pub(crate) fn procfs_list_dir(path: &str, buf: &mut [u8]) -> Result<usize, SyscallError> {
    if path != PROC_ROOT && path != "/proc/" {
        return Err(SyscallError::FileNotFound);
    }

    let mut offset = 0;
    let entries = [b"meminfo", b"tasks"];

    for name in entries {
        let needed = name.len() + 1;
        if offset + needed > buf.len() {
            break;
        }

        buf[offset..offset + name.len()].copy_from_slice(name);
        offset += name.len();
        buf[offset] = b'\n';
        offset += 1;
    }

    Ok(offset)
}

/// メモリ情報をテキスト形式で書き込む
fn write_mem_info(buf: &mut [u8]) -> usize {
    use crate::memory::FRAME_ALLOCATOR;
    use core::fmt::Write;

    // メモリ情報を取得
    let fa = FRAME_ALLOCATOR.lock();
    let total = fa.total_frames();
    let allocated = fa.allocated_count();
    let free = fa.free_frames();
    drop(fa);  // ロックを早めに解放

    // JSON 形式で書き込む
    let mut writer = SliceWriter::new(buf);
    let _ = write!(
        writer,
        "{{\"total_frames\":{},\"allocated_frames\":{},\"free_frames\":{},\"free_kib\":{}}}\n",
        total,
        allocated,
        free,
        free * 4
    );

    writer.written()
}

/// タスク一覧をテキスト形式で書き込む
fn write_task_list(buf: &mut [u8]) -> usize {
    use crate::scheduler::{self, TaskState};
    use core::fmt::Write;

    // タスク一覧を取得
    let tasks = scheduler::task_list();

    // JSON 形式で書き込む
    let mut writer = SliceWriter::new(buf);

    let _ = write!(writer, "{{\"tasks\":[");
    for (i, t) in tasks.iter().enumerate() {
        let state_str = match t.state {
            TaskState::Ready => "Ready",
            TaskState::Running => "Running",
            TaskState::Sleeping(_) => "Sleeping",
            TaskState::Finished => "Finished",
        };
        let type_str = if t.is_user_process { "user" } else { "kernel" };
        if i != 0 {
            let _ = write!(writer, ",");
        }
        let _ = write!(writer, "{{\"id\":{},\"state\":\"", t.id);
        let _ = writer.write_str(state_str);
        let _ = write!(writer, "\",\"type\":\"");
        let _ = writer.write_str(type_str);
        let _ = write!(writer, "\",\"name\":\"");
        let _ = write_json_string(&mut writer, t.name.as_str());
        let _ = write!(writer, "\"}}");
    }
    let _ = write!(writer, "]}}\n");

    writer.written()
}

/// SYS_GET_MEM_INFO: メモリ情報を取得
///
/// 引数:
///   arg1 — バッファのポインタ（ユーザー空間、書き込み先）
///   arg2 — バッファの長さ
///
/// 戻り値:
///   書き込んだバイト数（成功時）
///   負の値（エラー時）
///
/// 出力形式（テキスト）:
///   total_frames=XXXX
///   allocated_frames=XXXX
///   free_frames=XXXX
///   free_kib=XXXX
fn sys_get_mem_info(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let buf_len = arg2 as usize;
    let buf_slice = UserSlice::<u8>::from_raw(arg1, buf_len)?;
    let buf = buf_slice.as_mut_slice();
    Ok(write_mem_info(buf) as u64)
}

/// SYS_GET_TASK_LIST: タスク一覧を取得
///
/// 引数:
///   arg1 — バッファのポインタ（ユーザー空間、書き込み先）
///   arg2 — バッファの長さ
///
/// 戻り値:
///   書き込んだバイト数（成功時）
///   負の値（エラー時）
///
/// 出力形式（テキスト、1行目はヘッダ）:
///   id,state,type,name
///   1,Running,kernel,shell
///   2,Ready,user,HELLO.ELF
fn sys_get_task_list(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let buf_len = arg2 as usize;
    let buf_slice = UserSlice::<u8>::from_raw(arg1, buf_len)?;
    let buf = buf_slice.as_mut_slice();
    Ok(write_task_list(buf) as u64)
}

/// SYS_GET_NET_INFO: ネットワーク情報を取得
///
/// 引数:
///   arg1 — バッファのポインタ（ユーザー空間、書き込み先）
///   arg2 — バッファの長さ
///
/// 戻り値:
///   書き込んだバイト数（成功時）
///   負の値（エラー時）
///
/// 出力形式（テキスト）:
///   ip=X.X.X.X
///   gateway=X.X.X.X
///   dns=X.X.X.X
///   mac=XX:XX:XX:XX:XX:XX
fn sys_get_net_info(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    use core::fmt::Write;

    let buf_len = arg2 as usize;
    let buf_slice = UserSlice::<u8>::from_raw(arg1, buf_len)?;
    let buf = buf_slice.as_mut_slice();

    // ネットワーク情報を取得
    let my_ip = crate::net::MY_IP;
    let gateway = crate::net::GATEWAY_IP;
    let dns = crate::net::DNS_SERVER_IP;

    // テキスト形式で書き込む
    let mut writer = SliceWriter::new(buf);
    let _ = writeln!(writer, "ip={}.{}.{}.{}", my_ip[0], my_ip[1], my_ip[2], my_ip[3]);
    let _ = writeln!(writer, "gateway={}.{}.{}.{}", gateway[0], gateway[1], gateway[2], gateway[3]);
    let _ = writeln!(writer, "dns={}.{}.{}.{}", dns[0], dns[1], dns[2], dns[3]);

    // MAC アドレスを取得（virtio-net が初期化されていれば）
    let drv = crate::virtio_net::VIRTIO_NET.lock();
    if let Some(ref d) = *drv {
        let mac = d.mac_address;
        let _ = writeln!(writer, "mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);
    } else {
        let _ = writeln!(writer, "mac=none");
    }

    Ok(writer.written() as u64)
}

/// SYS_PCI_CONFIG_READ: PCI Configuration Space を読み取る
///
/// 引数:
///   arg1 — バス番号 (0-255)
///   arg2 — デバイス番号 (0-31)
///   arg3 — ファンクション番号 (0-7)
///   arg4 — offset と size を詰めた値
///          - 下位 8 ビット: offset
///          - 上位 8 ビット: size (1/2/4)
///
/// 戻り値:
///   読み取った値（下位 32 ビットに格納）
///   負の値（エラー時）
fn sys_pci_config_read(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    let bus = arg1 as u8;
    let device = arg2 as u8;
    let function = arg3 as u8;

    // arg4 の下位 16 ビットに offset/size を詰める
    let offset = (arg4 & 0xFF) as u8;
    let size = ((arg4 >> 8) & 0xFF) as u8;

    // 余分なビットが立っている場合は不正扱い
    if (arg4 >> 16) != 0 {
        return Err(SyscallError::InvalidArgument);
    }

    // 範囲チェック
    if arg1 > 0xFF || arg2 > 31 || arg3 > 7 || offset > 0xFF {
        return Err(SyscallError::InvalidArgument);
    }

    // サイズとアライメントのチェック
    match size {
        1 => {}
        2 => {
            if (offset & 1) != 0 || offset > 0xFE {
                return Err(SyscallError::InvalidArgument);
            }
        }
        4 => {
            if (offset & 3) != 0 || offset > 0xFC {
                return Err(SyscallError::InvalidArgument);
            }
        }
        _ => {
            return Err(SyscallError::InvalidArgument);
        }
    }

    let val32 = crate::pci::pci_config_read32(bus, device, function, offset & 0xFC);
    let value = match size {
        1 => {
            let shift = (offset & 3) * 8;
            (val32 >> shift) & 0xFF
        }
        2 => {
            let shift = (offset & 2) * 8;
            (val32 >> shift) & 0xFFFF
        }
        4 => val32,
        _ => 0,
    };

    Ok(value as u64)
}

// =================================================================
// プロセス管理関連システムコール
// =================================================================

/// SYS_EXEC: プログラムを同期実行（フォアグラウンド）
///
/// 引数:
///   arg1 — パスのポインタ（ユーザー空間）
///   arg2 — パスの長さ
///
/// 戻り値:
///   0（成功時、プログラム終了後）
///   負の値（エラー時）
///
/// 指定した ELF ファイルを読み込んで同期実行する。
/// プログラムが終了するまでこのシステムコールはブロックする。
fn sys_exec(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let path_len = arg2 as usize;

    // パスを取得
    let path_slice = UserSlice::<u8>::from_raw(arg1, path_len)?;
    let path = path_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // ファイル名を大文字に変換（FAT16 は大文字のみ）
    let path_upper: String = path.chars()
        .map(|c| c.to_ascii_uppercase())
        .collect();

    // FAT16 からファイルを読み込む
    let fs = crate::fat16::Fat16::new().map_err(|_| SyscallError::Other)?;
    let elf_data = fs.read_file(&path_upper).map_err(|_| SyscallError::FileNotFound)?;

    // ELF プロセスを作成
    let (process, entry_point, user_stack_top) =
        crate::usermode::create_elf_process(&elf_data).map_err(|_| SyscallError::Other)?;

    // Ring 3 で同期実行（完了するまでブロック）
    crate::usermode::run_elf_process(&process, entry_point, user_stack_top);

    // プロセスを破棄
    crate::usermode::destroy_user_process(process);

    Ok(0)
}

/// SYS_SPAWN: バックグラウンドでプロセスを起動
///
/// 引数:
///   arg1 — パスのポインタ（ユーザー空間）
///   arg2 — パスの長さ
///
/// 戻り値:
///   タスク ID（成功時）
///   負の値（エラー時）
///
/// 指定した ELF ファイルを読み込んでバックグラウンドで実行する。
/// 即座に戻り、プロセスはスケジューラで管理される。
fn sys_spawn(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let path_len = arg2 as usize;

    // パスを取得
    let path_slice = UserSlice::<u8>::from_raw(arg1, path_len)?;
    let path = path_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // ファイル名を大文字に変換（FAT16 は大文字のみ）
    let path_upper: String = path.chars()
        .map(|c| c.to_ascii_uppercase())
        .collect();

    // プロセス名を作成（パスからファイル名部分を抽出）
    let process_name = path_upper
        .rsplit('/')
        .next()
        .unwrap_or(&path_upper);

    // FAT16 からファイルを読み込む
    let fs = crate::fat16::Fat16::new().map_err(|_| SyscallError::Other)?;
    let elf_data = fs.read_file(&path_upper).map_err(|_| SyscallError::FileNotFound)?;

    // スケジューラにユーザープロセスとして登録
    let task_id = crate::scheduler::spawn_user(process_name, &elf_data)
        .map_err(|_| SyscallError::Other)?;

    Ok(task_id)
}

/// SYS_YIELD: CPU を譲る
///
/// 戻り値:
///   0（常に成功）
///
/// 現在のタスクの実行を中断し、他の ready なタスクに CPU を譲る。
fn sys_yield() -> Result<u64, SyscallError> {
    crate::scheduler::yield_now();
    Ok(0)
}

/// SYS_SLEEP: 指定ミリ秒スリープ
///
/// 引数:
///   arg1 — スリープ時間（ミリ秒）
///
/// 戻り値:
///   0（成功時）
///
/// 指定した時間だけ現在のタスクをスリープ状態にする。
fn sys_sleep(arg1: u64) -> Result<u64, SyscallError> {
    let ms = arg1;
    crate::scheduler::sleep_ms(ms);
    Ok(0)
}

// =================================================================
// ネットワーク関連システムコール
// =================================================================

/// SYS_DNS_LOOKUP: DNS 解決
///
/// 引数:
///   arg1 — ドメイン名のポインタ（ユーザー空間）
///   arg2 — ドメイン名の長さ
///   arg3 — 結果の IP アドレスを書き込むバッファ（4 バイト）
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
fn sys_dns_lookup(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    let domain_len = arg2 as usize;

    // ドメイン名を取得
    let domain_slice = UserSlice::<u8>::from_raw(arg1, domain_len)?;
    let domain = domain_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // IP バッファを取得（4 バイト）
    let ip_slice = UserSlice::<u8>::from_raw(arg3, 4)?;
    let ip_buf = ip_slice.as_mut_slice();

    // DNS 解決
    let ip = crate::net::dns_lookup(domain).map_err(|_| SyscallError::Other)?;

    // 結果をコピー
    ip_buf[0] = ip[0];
    ip_buf[1] = ip[1];
    ip_buf[2] = ip[2];
    ip_buf[3] = ip[3];

    Ok(0)
}

/// SYS_TCP_CONNECT: TCP 接続
///
/// 引数:
///   arg1 — IP アドレスのポインタ（4 バイト）
///   arg2 — ポート番号
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
fn sys_tcp_connect(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let port = arg2 as u16;

    // IP アドレスを取得
    let ip_slice = UserSlice::<u8>::from_raw(arg1, 4)?;
    let ip_buf = ip_slice.as_slice();
    let ip = [ip_buf[0], ip_buf[1], ip_buf[2], ip_buf[3]];

    // TCP 接続
    crate::net::tcp_connect(ip, port).map_err(|_| SyscallError::Other)?;

    Ok(0)
}

/// SYS_TCP_SEND: TCP 送信
///
/// 引数:
///   arg1 — データのポインタ（ユーザー空間）
///   arg2 — データの長さ
///
/// 戻り値:
///   送信したバイト数（成功時）
///   負の値（エラー時）
fn sys_tcp_send(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let data_len = arg2 as usize;

    // データを取得
    let data_slice = UserSlice::<u8>::from_raw(arg1, data_len)?;
    let data = data_slice.as_slice();

    // TCP 送信
    crate::net::tcp_send(data).map_err(|_| SyscallError::Other)?;

    Ok(data_len as u64)
}

/// SYS_TCP_RECV: TCP 受信
///
/// 引数:
///   arg1 — バッファのポインタ（ユーザー空間）
///   arg2 — バッファの長さ
///   arg3 — タイムアウト（ミリ秒）
///
/// 戻り値:
///   受信したバイト数（成功時）
///   0（タイムアウト時）
///   負の値（エラー時）
fn sys_tcp_recv(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    let buf_len = arg2 as usize;
    let timeout_ms = arg3;

    // バッファを取得
    let buf_slice = UserSlice::<u8>::from_raw(arg1, buf_len)?;
    let buf = buf_slice.as_mut_slice();

    // TCP 受信
    match crate::net::tcp_recv(timeout_ms) {
        Ok(data) => {
            let copy_len = core::cmp::min(data.len(), buf_len);
            buf[..copy_len].copy_from_slice(&data[..copy_len]);
            Ok(copy_len as u64)
        }
        Err(e) if e == "timeout" => Ok(0),  // タイムアウトは 0 を返す
        Err(e) if e == "connection closed" => Ok(0),  // 接続終了も 0 を返す
        Err(_) => Err(SyscallError::Other),
    }
}

/// SYS_TCP_CLOSE: TCP 切断
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
fn sys_tcp_close() -> Result<u64, SyscallError> {
    crate::net::tcp_close().map_err(|_| SyscallError::Other)?;
    Ok(0)
}

// =================================================================
// システム制御関連システムコール
// =================================================================

/// SYS_HALT: システム停止
///
/// システムを停止する。この関数は戻らない。
/// 割り込みを無効化し、HLT 命令で CPU を停止する。
fn sys_halt() -> Result<u64, SyscallError> {
    crate::kprintln!("System halted.");
    loop {
        x86_64::instructions::interrupts::disable();
        x86_64::instructions::hlt();
    }
}

// =================================================================
// ユーティリティ: スライスへの書き込み
// =================================================================

/// バイトスライスに書き込むための Write 実装
///
/// fmt::Write トレイトを実装して、write! / writeln! マクロを使えるようにする。
/// バッファがいっぱいになったら書き込みを止める（パニックしない）。
struct SliceWriter<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> SliceWriter<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn written(&self) -> usize {
        self.pos
    }
}

impl<'a> core::fmt::Write for SliceWriter<'a> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let remaining = self.buf.len() - self.pos;
        let to_write = core::cmp::min(bytes.len(), remaining);
        self.buf[self.pos..self.pos + to_write].copy_from_slice(&bytes[..to_write]);
        self.pos += to_write;
        Ok(())
    }
}

/// JSON 文字列用のエスケープ付き書き込み
fn write_json_string(writer: &mut SliceWriter<'_>, s: &str) -> core::fmt::Result {
    let mut buf = [0u8; 4];
    for ch in s.chars() {
        match ch {
            '\\' => {
                let _ = writer.write_str("\\\\");
            }
            '"' => {
                let _ = writer.write_str("\\\"");
            }
            '\n' => {
                let _ = writer.write_str("\\n");
            }
            '\r' => {
                let _ = writer.write_str("\\r");
            }
            '\t' => {
                let _ = writer.write_str("\\t");
            }
            _ => {
                let encoded = ch.encode_utf8(&mut buf);
                let _ = writer.write_str(encoded);
            }
        }
    }
    Ok(())
}
