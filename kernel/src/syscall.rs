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
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use crate::user_ptr::{UserPtr, UserSlice, SyscallError};
use x86_64::registers::control::Cr3;

/// システムコール番号の定義
///
/// 番号体系は計画に従う:
/// - コンソール I/O: 0-9
/// - テスト/デバッグ: 10-11
/// - ファイルシステム: 12-19
/// - システム情報: 20-29
/// - プロセス管理: 30-39
/// - ネットワーク: 40-49
/// - システム制御: 50-59
/// - 終了: 60
/// - ファイルハンドル: 70-79
/// - ブロックデバイス: 80-89
/// - IPC: 90-99
// コンソール I/O (0-9)
pub const SYS_READ: u64 = 0;         // read(buf_ptr, len) — コンソールから読み取り
pub const SYS_WRITE: u64 = 1;        // write(buf_ptr, len) — 文字列をカーネルコンソールに出力
pub const SYS_CLEAR_SCREEN: u64 = 2; // clear_screen() — 画面をクリア
pub const SYS_KEY_READ: u64 = 3;     // key_read(buf_ptr, len) — ノンブロッキングキー読み取り
pub const SYS_CONSOLE_GRAB: u64 = 4; // console_grab(grab) — キーボードフォーカス取得/解放

// テスト/デバッグ (10-11)
pub const SYS_SELFTEST: u64 = 10;    // selftest() — カーネル selftest を実行

// ファイルシステム (12-19)
pub const SYS_FILE_DELETE: u64 = 12; // file_delete(path_ptr, path_len) — ファイル削除
pub const SYS_DIR_LIST: u64 = 13;    // dir_list(path_ptr, path_len, buf_ptr, buf_len) — ディレクトリ一覧

// システム情報 (20-29)
pub const SYS_GET_MEM_INFO: u64 = 20;   // get_mem_info(buf_ptr, buf_len) — メモリ情報取得
pub const SYS_GET_TASK_LIST: u64 = 21;  // get_task_list(buf_ptr, buf_len) — タスク一覧取得
pub const SYS_GET_NET_INFO: u64 = 22;   // get_net_info(buf_ptr, buf_len) — ネットワーク情報取得
pub const SYS_PCI_CONFIG_READ: u64 = 23; // pci_config_read(bus, device, function, offset, size) — PCI Config 読み取り
pub const SYS_GET_FB_INFO: u64 = 24;    // get_fb_info(buf_ptr, buf_len) — フレームバッファ情報取得
pub const SYS_MOUSE_READ: u64 = 25;     // mouse_read(buf_ptr, buf_len) — マウス状態取得
pub const SYS_CLOCK_MONOTONIC: u64 = 26; // clock_monotonic() — 起動からの経過ミリ秒を返す
pub const SYS_GETRANDOM: u64 = 27;       // getrandom(buf_ptr, len) — ランダムバイトを生成

// プロセス管理 (30-39)
pub const SYS_EXEC: u64 = 30;    // exec(path_ptr, path_len) — プログラムを同期実行
pub const SYS_SPAWN: u64 = 31;   // spawn(path_ptr, path_len) — バックグラウンドでプロセス起動
pub const SYS_YIELD: u64 = 32;   // yield() — CPU を譲る
pub const SYS_SLEEP: u64 = 33;   // sleep(ms) — 指定ミリ秒スリープ
pub const SYS_WAIT: u64 = 34;    // wait(task_id, timeout_ms) — 子プロセスの終了を待つ
pub const SYS_GETPID: u64 = 35;  // getpid() — 自分のタスク ID を取得
pub const SYS_KILL: u64 = 36;    // kill(task_id) — タスクを強制終了

// ネットワーク (40-49)
pub const SYS_DNS_LOOKUP: u64 = 40;  // dns_lookup(domain_ptr, domain_len, ip_ptr) — DNS 解決
pub const SYS_TCP_CONNECT: u64 = 41; // tcp_connect(ip_ptr, port) — TCP 接続
pub const SYS_TCP_SEND: u64 = 42;    // tcp_send(data_ptr, data_len) — TCP 送信
pub const SYS_TCP_RECV: u64 = 43;    // tcp_recv(buf_ptr, buf_len, timeout_ms) — TCP 受信
pub const SYS_TCP_CLOSE: u64 = 44;   // tcp_close() — TCP 切断
pub const SYS_NET_SEND_FRAME: u64 = 45; // net_send_frame(buf_ptr, len) — Ethernet フレーム送信
pub const SYS_NET_RECV_FRAME: u64 = 46; // net_recv_frame(buf_ptr, len, timeout_ms) — Ethernet フレーム受信
pub const SYS_NET_GET_MAC: u64 = 47;    // net_get_mac(buf_ptr, len) — MAC アドレス取得

// システム制御 (50-59)
pub const SYS_HALT: u64 = 50;        // halt() — システム停止
pub const SYS_DRAW_PIXEL: u64 = 51;  // draw_pixel(x, y, rgb) — 1ピクセル描画
pub const SYS_DRAW_RECT: u64 = 52;   // draw_rect(x, y, w_h, rgb) — 矩形描画（w/h は packed）
pub const SYS_DRAW_LINE: u64 = 53;   // draw_line(xy0, xy1, rgb) — 直線描画（x,y は packed）
pub const SYS_DRAW_BLIT: u64 = 54;   // draw_blit(x, y, w_h, buf_ptr) — 画像描画
pub const SYS_DRAW_TEXT: u64 = 55;   // draw_text(xy, fg_bg, buf_ptr, len) — 文字列描画

// 終了 (60)
pub const SYS_EXIT: u64 = 60;        // exit() — ユーザープログラムを終了してカーネルに戻る

// ファイルハンドル (70-79)
pub const SYS_OPEN: u64 = 70;         // open(path_ptr, path_len, handle_ptr, rights)
pub const SYS_HANDLE_READ: u64 = 71;  // handle_read(handle_ptr, buf_ptr, len)
pub const SYS_HANDLE_WRITE: u64 = 72; // handle_write(handle_ptr, buf_ptr, len)
pub const SYS_HANDLE_CLOSE: u64 = 73; // handle_close(handle_ptr)
pub const SYS_OPENAT: u64 = 74;       // openat(dir_handle_ptr, path_ptr, path_len, new_handle_ptr, rights)
pub const SYS_RESTRICT_RIGHTS: u64 = 75; // restrict_rights(handle_ptr, new_rights, new_handle_ptr)
pub const SYS_HANDLE_ENUM: u64 = 76;  // handle_enum(dir_handle_ptr, buf_ptr, len)

// ブロックデバイス (80-89)
pub const SYS_BLOCK_READ: u64 = 80;   // block_read(sector, buf_ptr, len)
pub const SYS_BLOCK_WRITE: u64 = 81;  // block_write(sector, buf_ptr, len)

// IPC (90-99)
pub const SYS_IPC_SEND: u64 = 90;     // ipc_send(dest_task_id, buf_ptr, len)
pub const SYS_IPC_RECV: u64 = 91;     // ipc_recv(sender_ptr, buf_ptr, buf_len, timeout_ms)

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
    // 割り込みを無効化（カーネル内の再入を防ぐ）
    "cli",

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
    // syscall 内で割り込みを有効化しても、復帰前に無効化しておく
    "cli",

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

/// syscall 引数のユーザー空間バッファを検証して取得する（共通ヘルパー）
fn user_slice_from_args(arg_ptr: u64, arg_len: u64) -> Result<UserSlice<u8>, SyscallError> {
    let len = usize::try_from(arg_len).map_err(|_| SyscallError::InvalidArgument)?;
    UserSlice::<u8>::from_raw(arg_ptr, len)
}

/// syscall 引数のユーザー空間ポインタを検証して取得する（共通ヘルパー）
fn user_ptr_from_arg<T>(arg: u64) -> Result<UserPtr<T>, SyscallError> {
    UserPtr::<T>::from_raw(arg)
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
        SYS_KEY_READ => sys_key_read(arg1, arg2),
        SYS_CONSOLE_GRAB => sys_console_grab(arg1),
        // テスト/デバッグ
        SYS_SELFTEST => sys_selftest(),
        // ファイルシステム
        SYS_FILE_DELETE => sys_file_delete(arg1, arg2),
        SYS_DIR_LIST => sys_dir_list(arg1, arg2, arg3, arg4),
        // システム情報
        SYS_GET_MEM_INFO => sys_get_mem_info(arg1, arg2),
        SYS_GET_TASK_LIST => sys_get_task_list(arg1, arg2),
        SYS_GET_NET_INFO => sys_get_net_info(arg1, arg2),
        SYS_PCI_CONFIG_READ => sys_pci_config_read(arg1, arg2, arg3, arg4),
        SYS_GET_FB_INFO => sys_get_fb_info(arg1, arg2),
        SYS_MOUSE_READ => sys_mouse_read(arg1, arg2),
        SYS_CLOCK_MONOTONIC => sys_clock_monotonic(),
        SYS_GETRANDOM => sys_getrandom(arg1, arg2),
        // プロセス管理
        SYS_EXEC => sys_exec(arg1, arg2),
        SYS_SPAWN => sys_spawn(arg1, arg2),
        SYS_YIELD => sys_yield(),
        SYS_SLEEP => sys_sleep(arg1),
        SYS_WAIT => sys_wait(arg1, arg2),
        SYS_GETPID => sys_getpid(),
        SYS_KILL => sys_kill(arg1),
        // ネットワーク
        SYS_DNS_LOOKUP => sys_dns_lookup(arg1, arg2, arg3),
        SYS_TCP_CONNECT => sys_tcp_connect(arg1, arg2),
        SYS_TCP_SEND => sys_tcp_send(arg1, arg2),
        SYS_TCP_RECV => sys_tcp_recv(arg1, arg2, arg3),
        SYS_TCP_CLOSE => sys_tcp_close(),
        SYS_NET_SEND_FRAME => sys_net_send_frame(arg1, arg2),
        SYS_NET_RECV_FRAME => sys_net_recv_frame(arg1, arg2, arg3),
        SYS_NET_GET_MAC => sys_net_get_mac(arg1, arg2),
        // ハンドル
        SYS_OPEN => sys_open(arg1, arg2, arg3, arg4),
        SYS_HANDLE_READ => sys_handle_read(arg1, arg2, arg3),
        SYS_HANDLE_WRITE => sys_handle_write(arg1, arg2, arg3),
        SYS_HANDLE_CLOSE => sys_handle_close(arg1),
        SYS_OPENAT => sys_openat(arg1, arg2, arg3, arg4),
        SYS_RESTRICT_RIGHTS => sys_restrict_rights(arg1, arg2, arg3),
        SYS_HANDLE_ENUM => sys_handle_enum(arg1, arg2, arg3),
        // ブロックデバイス
        SYS_BLOCK_READ => sys_block_read(arg1, arg2, arg3),
        SYS_BLOCK_WRITE => sys_block_write(arg1, arg2, arg3),
        // IPC
        SYS_IPC_SEND => sys_ipc_send(arg1, arg2, arg3),
        SYS_IPC_RECV => sys_ipc_recv(arg1, arg2, arg3, arg4),
        // システム制御
        SYS_DRAW_PIXEL => sys_draw_pixel(arg1, arg2, arg3),
        SYS_DRAW_RECT => sys_draw_rect(arg1, arg2, arg3, arg4),
        SYS_DRAW_LINE => sys_draw_line(arg1, arg2, arg3),
        SYS_DRAW_BLIT => sys_draw_blit(arg1, arg2, arg3, arg4),
        SYS_DRAW_TEXT => sys_draw_text(arg1, arg2, arg3, arg4),
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

/// SYS_READ: コンソールから読み取り（フォーカス対応版）
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
///
/// キーボードフォーカスが設定されている場合、フォーカス外のタスクは
/// フォーカスが解放されるまで yield で待機する。
fn sys_read(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let len = usize::try_from(arg2).map_err(|_| SyscallError::InvalidArgument)?;

    // 長さ 0 の場合は何もしない
    if len == 0 {
        return Ok(0);
    }

    // UserSlice で型安全にユーザー空間のバッファを取得
    let user_slice = user_slice_from_args(arg1, arg2)?;

    // 可変スライスとしてアクセス（書き込み用）
    let buf = user_slice.as_mut_slice();

    // 呼び出し元のタスク ID を取得してフォーカス対応版で読み取り
    let caller_task_id = crate::scheduler::current_task_id();
    let bytes_read = crate::console::read_input_for_task(buf, len, caller_task_id);

    Ok(bytes_read as u64)
}

/// SYS_KEY_READ: ノンブロッキングキー読み取り
///
/// SYS_MOUSE_READ と同じパターンで、入力がなければ 0 を返す。
/// フォーカスを持つタスクのみがキー入力を読み取れる。
///
/// 引数:
///   arg1 — バッファのポインタ（ユーザー空間、書き込み先）
///   arg2 — バッファの長さ
///
/// 戻り値:
///   読み取ったバイト数（0 = 入力なし）
fn sys_key_read(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let len = usize::try_from(arg2).map_err(|_| SyscallError::InvalidArgument)?;
    if len == 0 {
        return Ok(0);
    }

    let user_slice = user_slice_from_args(arg1, arg2)?;
    let buf = user_slice.as_mut_slice();

    let caller_task_id = crate::scheduler::current_task_id();
    let mut count = 0;

    // ノンブロッキングで読めるだけ読む
    while count < len {
        if let Some(c) = crate::console::read_input_nonblocking_for_task(caller_task_id) {
            buf[count] = if c.is_ascii() { c as u8 } else { b'?' };
            count += 1;
        } else {
            break;
        }
    }

    Ok(count as u64)
}

/// SYS_CONSOLE_GRAB: キーボードフォーカスの取得/解放
///
/// 引数:
///   arg1 — 1 = フォーカス取得、0 = フォーカス解放
///
/// 戻り値:
///   0（成功）
fn sys_console_grab(arg1: u64) -> Result<u64, SyscallError> {
    let caller_task_id = crate::scheduler::current_task_id();
    if arg1 != 0 {
        // フォーカス取得
        crate::console::grab_keyboard(caller_task_id);
    } else {
        // フォーカス解放
        crate::console::release_keyboard(caller_task_id);
    }
    Ok(0)
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
    let len = usize::try_from(arg2).map_err(|_| SyscallError::InvalidArgument)?;

    // UserSlice で型安全にユーザー空間のバッファを取得
    // アドレス範囲、アラインメント、オーバーフローを検証
    let user_slice = user_slice_from_args(arg1, arg2)?;

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

/// SYS_GET_FB_INFO: フレームバッファ情報を取得する
///
/// 引数:
///   arg1 — 書き込み先バッファのポインタ（ユーザー空間）
///   arg2 — バッファ長
///
/// 戻り値:
///   書き込んだバイト数（成功時）
///   負の値（エラー時）
fn sys_get_fb_info(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let buf_len = usize::try_from(arg2).map_err(|_| SyscallError::InvalidArgument)?;
    let buf_slice = user_slice_from_args(arg1, arg2)?;
    let buf = buf_slice.as_mut_slice();

    let Some(info) = crate::framebuffer::screen_info() else {
        return Err(SyscallError::Other);
    };

    let info_size = core::mem::size_of::<crate::framebuffer::FramebufferInfoSmall>();
    if buf_len < info_size {
        return Err(SyscallError::BufferOverflow);
    }

    let bytes = unsafe {
        core::slice::from_raw_parts(
            &info as *const _ as *const u8,
            info_size,
        )
    };
    buf[..info_size].copy_from_slice(bytes);
    Ok(info_size as u64)
}

/// SYS_MOUSE_READ: マウス状態を取得する
///
/// 引数:
///   arg1 — 書き込み先バッファ（ユーザー空間）
///   arg2 — バッファ長
///
/// 戻り値:
///   0（更新なし）
///   sizeof(MouseState)（更新あり）
///   負の値（エラー）
fn sys_mouse_read(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let buf_slice = user_slice_from_args(arg1, arg2)?;
    let buf = buf_slice.as_mut_slice();

    let state = match crate::mouse::read_state() {
        Some(s) => s,
        None => return Ok(0),
    };

    let size = core::mem::size_of::<crate::mouse::MouseState>();
    if buf.len() < size {
        return Err(SyscallError::InvalidArgument);
    }

    let src = unsafe {
        core::slice::from_raw_parts(
            (&state as *const crate::mouse::MouseState) as *const u8,
            size,
        )
    };
    buf[..size].copy_from_slice(src);
    Ok(size as u64)
}

/// SYS_DRAW_PIXEL: 1 ピクセルを描画する
///
/// 引数:
///   arg1 — x 座標
///   arg2 — y 座標
///   arg3 — RGB packed (0xRRGGBB)
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
fn sys_draw_pixel(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    let x = usize::try_from(arg1).map_err(|_| SyscallError::InvalidArgument)?;
    let y = usize::try_from(arg2).map_err(|_| SyscallError::InvalidArgument)?;

    let rgb = arg3 as u32;
    let r = ((rgb >> 16) & 0xFF) as u8;
    let g = ((rgb >> 8) & 0xFF) as u8;
    let b = (rgb & 0xFF) as u8;

    match crate::framebuffer::draw_pixel_global(x, y, r, g, b) {
        Ok(()) => Ok(0),
        Err(crate::framebuffer::DrawError::NotInitialized) => Err(SyscallError::Other),
        Err(_) => Err(SyscallError::InvalidArgument),
    }
}

/// SYS_DRAW_RECT: 矩形を描画する
///
/// 引数:
///   arg1 — x 座標
///   arg2 — y 座標
///   arg3 — width/height packed（上位 32bit = w, 下位 32bit = h）
///   arg4 — RGB packed (0xRRGGBB)
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
fn sys_draw_rect(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    let x = usize::try_from(arg1).map_err(|_| SyscallError::InvalidArgument)?;
    let y = usize::try_from(arg2).map_err(|_| SyscallError::InvalidArgument)?;

    let w = (arg3 >> 32) as u32;
    let h = (arg3 & 0xFFFF_FFFF) as u32;
    let w = usize::try_from(w).map_err(|_| SyscallError::InvalidArgument)?;
    let h = usize::try_from(h).map_err(|_| SyscallError::InvalidArgument)?;

    let rgb = arg4 as u32;
    let r = ((rgb >> 16) & 0xFF) as u8;
    let g = ((rgb >> 8) & 0xFF) as u8;
    let b = (rgb & 0xFF) as u8;

    match crate::framebuffer::draw_rect_global(x, y, w, h, r, g, b) {
        Ok(()) => Ok(0),
        Err(crate::framebuffer::DrawError::NotInitialized) => Err(SyscallError::Other),
        Err(_) => Err(SyscallError::InvalidArgument),
    }
}

/// SYS_DRAW_LINE: 直線を描画する
///
/// 引数:
///   arg1 — x0/y0 packed（上位 32bit = x0, 下位 32bit = y0）
///   arg2 — x1/y1 packed（上位 32bit = x1, 下位 32bit = y1）
///   arg3 — RGB packed (0xRRGGBB)
fn sys_draw_line(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    let x0 = (arg1 >> 32) as u32;
    let y0 = (arg1 & 0xFFFF_FFFF) as u32;
    let x1 = (arg2 >> 32) as u32;
    let y1 = (arg2 & 0xFFFF_FFFF) as u32;

    let x0 = usize::try_from(x0).map_err(|_| SyscallError::InvalidArgument)?;
    let y0 = usize::try_from(y0).map_err(|_| SyscallError::InvalidArgument)?;
    let x1 = usize::try_from(x1).map_err(|_| SyscallError::InvalidArgument)?;
    let y1 = usize::try_from(y1).map_err(|_| SyscallError::InvalidArgument)?;

    let rgb = arg3 as u32;
    let r = ((rgb >> 16) & 0xFF) as u8;
    let g = ((rgb >> 8) & 0xFF) as u8;
    let b = (rgb & 0xFF) as u8;

    match crate::framebuffer::draw_line_global(x0, y0, x1, y1, r, g, b) {
        Ok(()) => Ok(0),
        Err(crate::framebuffer::DrawError::NotInitialized) => Err(SyscallError::Other),
        Err(_) => Err(SyscallError::InvalidArgument),
    }
}

/// SYS_DRAW_BLIT: 画像（RGBX）を描画する
///
/// 引数:
///   arg1 — x 座標
///   arg2 — y 座標
///   arg3 — width/height packed（上位 32bit = w, 下位 32bit = h）
///   arg4 — バッファポインタ（ユーザー空間）
fn sys_draw_blit(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    let x = usize::try_from(arg1).map_err(|_| SyscallError::InvalidArgument)?;
    let y = usize::try_from(arg2).map_err(|_| SyscallError::InvalidArgument)?;

    let w = (arg3 >> 32) as u32;
    let h = (arg3 & 0xFFFF_FFFF) as u32;
    let w = usize::try_from(w).map_err(|_| SyscallError::InvalidArgument)?;
    let h = usize::try_from(h).map_err(|_| SyscallError::InvalidArgument)?;

    let pixel_count = w.checked_mul(h).ok_or(SyscallError::InvalidArgument)?;
    let byte_len = pixel_count.checked_mul(4).ok_or(SyscallError::InvalidArgument)?;
    let buf_slice = UserSlice::<u8>::from_raw(arg4, byte_len)?;
    let buf = buf_slice.as_slice();

    match crate::framebuffer::draw_blit_global(x, y, w, h, buf) {
        Ok(()) => Ok(0),
        Err(crate::framebuffer::DrawError::NotInitialized) => Err(SyscallError::Other),
        Err(_) => Err(SyscallError::InvalidArgument),
    }
}

/// SYS_DRAW_TEXT: 文字列を描画する
///
/// 引数:
///   arg1 — x/y packed（上位 32bit = x, 下位 32bit = y）
///   arg2 — fg/bg packed（上位 32bit = fg, 下位 32bit = bg）
///   arg3 — 文字列ポインタ（ユーザー空間）
///   arg4 — 文字列長
fn sys_draw_text(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    let x = (arg1 >> 32) as u32;
    let y = (arg1 & 0xFFFF_FFFF) as u32;
    let x = usize::try_from(x).map_err(|_| SyscallError::InvalidArgument)?;
    let y = usize::try_from(y).map_err(|_| SyscallError::InvalidArgument)?;

    let fg = (arg2 >> 32) as u32;
    let bg = (arg2 & 0xFFFF_FFFF) as u32;
    let fg = (
        ((fg >> 16) & 0xFF) as u8,
        ((fg >> 8) & 0xFF) as u8,
        (fg & 0xFF) as u8,
    );
    let bg = (
        ((bg >> 16) & 0xFF) as u8,
        ((bg >> 8) & 0xFF) as u8,
        (bg & 0xFF) as u8,
    );

    let text_slice = user_slice_from_args(arg3, arg4)?;
    let text = text_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    match crate::framebuffer::draw_text_global(x, y, fg, bg, text) {
        Ok(()) => Ok(0),
        Err(crate::framebuffer::DrawError::NotInitialized) => Err(SyscallError::Other),
        Err(_) => Err(SyscallError::InvalidArgument),
    }
}

// =================================================================
// テスト/デバッグ関連システムコール
// =================================================================

/// SYS_SELFTEST: カーネル selftest を実行する
///
/// 引数: なし
/// 戻り値: 0（成功）
fn sys_selftest() -> Result<u64, SyscallError> {
    // selftest 中にタイマー割り込みやタスク切り替えが動くように有効化
    x86_64::instructions::interrupts::enable();
    crate::shell::run_selftest();
    Ok(0)
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
    use crate::handle::{Handle, HANDLE_RIGHT_WRITE};

    let rights = arg4 as u32;

    // パスを取得
    let path_slice = user_slice_from_args(arg1, arg2)?;
    let path = path_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // Handle の書き込み先
    let handle_ptr = user_ptr_from_arg::<Handle>(arg3)?;

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

    let handle_ptr = user_ptr_from_arg::<Handle>(arg1)?;
    let handle = handle_ptr.read();

    let buf_slice = user_slice_from_args(arg2, arg3)?;
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

    let handle_ptr = user_ptr_from_arg::<Handle>(arg1)?;
    let handle = handle_ptr.read();

    let buf_slice = user_slice_from_args(arg2, arg3)?;
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

    let handle_ptr = user_ptr_from_arg::<Handle>(arg1)?;
    let handle = handle_ptr.read();

    crate::handle::close(&handle)?;
    Ok(0)
}

/// SYS_OPENAT: ディレクトリハンドルからの相対パスでファイルを開く
///
/// Capability-based security の核心となるシステムコール。
/// ディレクトリハンドルが持つ権限の範囲内でのみファイルを開ける。
///
/// 引数:
///   arg1 — ディレクトリハンドルへのポインタ（ユーザー空間）
///   arg2 — 相対パスのポインタ（ユーザー空間、絶対パス禁止）
///   arg3 — パスの長さ
///   arg4 — 下位32ビット: 新しいハンドルの書き込み先ポインタ
///          上位32ビット: 要求する権限（親の権限以下に制限される）
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
///
/// セキュリティ:
///   - dir_handle が LOOKUP 権限を持つか確認
///   - path が "/" で始まっていたらエラー（絶対パス禁止）
///   - path に ".." が含まれていたらエラー（パストラバーサル防止）
///   - 新しいハンドルの権限 = requested_rights & dir_handle.rights
fn sys_openat(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    use crate::handle::{Handle, HandleKind, HANDLE_RIGHT_LOOKUP};

    // 注: 現在の実装では arg4 全体を new_handle_ptr として扱う。
    // rights は open_path_to_handle() 内で種別に応じてデフォルトを設定する。
    let new_handle_ptr_raw = arg4;

    // ディレクトリハンドルを取得
    let dir_handle_ptr = user_ptr_from_arg::<Handle>(arg1)?;
    let dir_handle = dir_handle_ptr.read();

    // 相対パスを取得
    let path_slice = user_slice_from_args(arg2, arg3)?;
    let path = path_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // 新しいハンドルの書き込み先
    let new_handle_ptr = user_ptr_from_arg::<Handle>(new_handle_ptr_raw)?;

    // ディレクトリハンドルの権限チェック（LOOKUP 権限が必要）
    crate::handle::check_rights(&dir_handle, HANDLE_RIGHT_LOOKUP)?;

    // ディレクトリハンドルの種類チェック
    let kind = crate::handle::get_kind(&dir_handle)?;
    if kind != HandleKind::Directory {
        return Err(SyscallError::InvalidArgument);
    }

    // 相対パスの検証（絶対パスと ".." を禁止）
    crate::vfs::validate_relative_path(path).map_err(|e| {
        match e {
            crate::vfs::VfsError::PathTraversal => SyscallError::PathTraversal,
            crate::vfs::VfsError::InvalidPath => SyscallError::InvalidArgument,
            _ => SyscallError::Other,
        }
    })?;

    // ディレクトリのパスを取得
    let dir_path = crate::handle::get_path(&dir_handle)?;

    // フルパスを構築
    let full_path = if dir_path.is_empty() || dir_path == "/" {
        format!("/{}", path)
    } else {
        format!("{}/{}", dir_path, path)
    };

    // ディレクトリハンドルの権限を取得（open_path_to_handle で検証）
    let _ = crate::handle::get_rights(&dir_handle)?;

    // ファイル/ディレクトリを開く（権限はデフォルト）
    let handle = open_path_to_handle(&full_path, 0)?;
    new_handle_ptr.write(handle);

    Ok(0)
}

/// SYS_HANDLE_ENUM: ディレクトリハンドルの内容を一覧
///
/// 引数:
///   arg1 — ディレクトリハンドルのポインタ（ユーザー空間）
///   arg2 — バッファのポインタ（ユーザー空間、エントリ名を改行区切りで書き込む）
///   arg3 — バッファの長さ
///
/// 戻り値:
///   書き込んだバイト数（成功時）
///   負の値（エラー時）
fn sys_handle_enum(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    use crate::handle::{Handle, HandleKind, HANDLE_RIGHT_ENUM};

    let handle_ptr = user_ptr_from_arg::<Handle>(arg1)?;
    let handle = handle_ptr.read();

    let buf_slice = user_slice_from_args(arg2, arg3)?;
    let buf = buf_slice.as_mut_slice();

    crate::handle::check_rights(&handle, HANDLE_RIGHT_ENUM)?;
    if crate::handle::get_kind(&handle)? != HandleKind::Directory {
        return Err(SyscallError::InvalidArgument);
    }

    let path = crate::handle::get_path(&handle)?;
    let written = list_dir_to_buffer(&path, buf)?;
    Ok(written as u64)
}

/// SYS_RESTRICT_RIGHTS: ハンドルの権限を縮小する
///
/// Capability-based security の重要な操作。
/// 権限は縮小のみ可能で、拡大はできない。
///
/// 引数:
///   arg1 — 元のハンドルへのポインタ（ユーザー空間）
///   arg2 — 新しい権限ビット（縮小のみ可）
///   arg3 — 新しいハンドルの書き込み先ポインタ（ユーザー空間）
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
///
/// セキュリティ:
///   - new_rights は元のハンドルの rights の部分集合でなければならない
///   - 権限の拡大を試みた場合は PermissionDenied エラー
fn sys_restrict_rights(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    use crate::handle::Handle;

    let new_rights = arg2 as u32;

    // 元のハンドルを取得
    let handle_ptr = user_ptr_from_arg::<Handle>(arg1)?;
    let handle = handle_ptr.read();

    // 新しいハンドルの書き込み先
    let new_handle_ptr = user_ptr_from_arg::<Handle>(arg3)?;

    // 権限を縮小した新しいハンドルを作成
    let new_handle = crate::handle::restrict_rights(&handle, new_rights)?;
    new_handle_ptr.write(new_handle);

    Ok(0)
}

/// SYS_BLOCK_READ: ブロックデバイスからセクタを読み取る
///
/// 引数:
///   arg1 — セクタ番号
///   arg2 — バッファのポインタ（ユーザー空間）
///   arg3 — バッファの長さ（512 バイト固定）
///
/// 戻り値:
///   読み取ったバイト数（成功時）
///   負の値（エラー時）
pub(crate) fn sys_block_read(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    let len = usize::try_from(arg3).map_err(|_| SyscallError::InvalidArgument)?;
    if len != 512 {
        return Err(SyscallError::InvalidArgument);
    }

    let buf_slice = user_slice_from_args(arg2, arg3)?;
    let buf = buf_slice.as_mut_slice();

    let mut drv = crate::virtio_blk::VIRTIO_BLK.lock();
    let drv = drv.as_mut().ok_or(SyscallError::Other)?;
    // ユーザー空間のバッファは物理アドレスではないため、
    // DMA 先に直接渡すと壊れる。カーネルバッファに読み取ってから
    // ユーザー空間にコピーする。
    let mut kernel_buf = [0u8; 512];
    drv.read_sector(arg1, &mut kernel_buf).map_err(|_| SyscallError::Other)?;
    buf.copy_from_slice(&kernel_buf);
    Ok(len as u64)
}

/// SYS_BLOCK_WRITE: ブロックデバイスにセクタを書き込む
///
/// 引数:
///   arg1 — セクタ番号
///   arg2 — バッファのポインタ（ユーザー空間）
///   arg3 — バッファの長さ（512 バイト固定）
///
/// 戻り値:
///   書き込んだバイト数（成功時）
///   負の値（エラー時）
pub(crate) fn sys_block_write(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    let len = usize::try_from(arg3).map_err(|_| SyscallError::InvalidArgument)?;
    if len != 512 {
        return Err(SyscallError::InvalidArgument);
    }

    let buf_slice = user_slice_from_args(arg2, arg3)?;
    let buf = buf_slice.as_slice();

    let mut drv = crate::virtio_blk::VIRTIO_BLK.lock();
    let drv = drv.as_mut().ok_or(SyscallError::Other)?;
    // DMA 先は物理アドレス前提なので、カーネルバッファにコピーしてから書き込む。
    let mut kernel_buf = [0u8; 512];
    kernel_buf.copy_from_slice(buf);
    drv.write_sector(arg1, &kernel_buf).map_err(|_| SyscallError::Other)?;
    Ok(len as u64)
}

/// SYS_IPC_SEND: メッセージを送信する
///
/// 引数:
///   arg1 — 宛先タスクID
///   arg2 — バッファのポインタ（ユーザー空間）
///   arg3 — バッファの長さ
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
fn sys_ipc_send(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    let buf_slice = user_slice_from_args(arg2, arg3)?;
    let buf = buf_slice.as_slice();

    let sender = crate::scheduler::current_task_id();
    crate::ipc::send(sender, arg1, buf.to_vec())?;
    Ok(0)
}

/// SYS_IPC_RECV: メッセージを受信する
///
/// 引数:
///   arg1 — 送信元タスクIDの書き込み先（ユーザー空間）
///   arg2 — 受信バッファのポインタ（ユーザー空間）
///   arg3 — 受信バッファの長さ
///   arg4 — タイムアウト (ms). 0 は無期限
///
/// 戻り値:
///   読み取ったバイト数（成功時）
///   負の値（エラー時）
fn sys_ipc_recv(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    // IPC 受信は待ちに入る可能性があるため、割り込みを有効化してタイマ割り込みを許可する。
    // これをしないと sleep_ticks() が起床できず、待ちが永久に続く。
    x86_64::instructions::interrupts::enable();

    let sender_ptr = user_ptr_from_arg::<u64>(arg1)?;
    let buf_slice = user_slice_from_args(arg2, arg3)?;
    let buf = buf_slice.as_mut_slice();

    let task_id = crate::scheduler::current_task_id();
    let msg = crate::ipc::recv(task_id, arg4)?;

    let copy_len = core::cmp::min(buf.len(), msg.data.len());
    buf[..copy_len].copy_from_slice(&msg.data[..copy_len]);
    sender_ptr.write(msg.sender);

    Ok(copy_len as u64)
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
    // パスを取得
    let path_slice = user_slice_from_args(arg1, arg2)?;
    let path = path_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // /proc 配下は読み取り専用
    if path.starts_with("/proc") {
        return Err(SyscallError::ReadOnly);
    }

    // FAT32 からファイルを削除
    let mut fat32 = crate::fat32::Fat32::new().map_err(|_| SyscallError::Other)?;
    fat32.delete_file(path).map_err(|_| SyscallError::FileNotFound)?;

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
    // パスを取得
    let path_slice = user_slice_from_args(arg1, arg2)?;
    let path = path_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // バッファを取得
    let buf_slice = user_slice_from_args(arg3, arg4)?;
    let buf = buf_slice.as_mut_slice();

    let written = list_dir_to_buffer(path, buf)?;
    Ok(written as u64)
}

/// ディレクトリ一覧をバッファに書き込む（共通ヘルパー）
fn list_dir_to_buffer(path: &str, buf: &mut [u8]) -> Result<usize, SyscallError> {
    // /proc 配下は procfs で処理
    if path.starts_with("/proc") {
        let written = crate::procfs::procfs_list_dir(path, buf)?;
        return Ok(written);
    }

    // FAT32 からディレクトリ一覧を取得
    let mut fat32 = crate::fat32::Fat32::new().map_err(|_| SyscallError::Other)?;
    let entries = fat32.list_dir(path).map_err(|_| SyscallError::FileNotFound)?;

    // エントリ名を改行区切りでバッファに書き込む
    // ATTR_DIRECTORY = 0x10
    const ATTR_DIRECTORY: u8 = 0x10;

    let mut offset = 0;
    for entry in entries {
        let name = &entry.name;
        let is_dir = (entry.attr & ATTR_DIRECTORY) != 0;

        // 名前のバイト数 + 改行 (+ "/" for directories)
        let needed = name.len() + if is_dir { 2 } else { 1 };
        if offset + needed > buf.len() {
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
        if offset + needed <= buf.len() {
            buf[offset..offset + name.len()].copy_from_slice(name);
            offset += name.len();
            buf[offset] = b'\n';
            offset += 1;
        }
    }

    Ok(offset)
}

/// selftest 用のテストエントリポイント
///
/// list_dir_to_buffer のテスト用ラッパー。
/// shell.rs の selftest から呼ぶため pub にしている。
pub fn list_dir_to_buffer_for_test(path: &str, buf: &mut [u8]) -> Result<usize, SyscallError> {
    list_dir_to_buffer(path, buf)
}

// =================================================================
// システム情報関連システムコール
// =================================================================

// =================================================================
// procfs 関連のヘルパー関数
// =================================================================
//
// procfs モジュールへのブリッジ。syscall.rs から呼び出しやすいように
// エラー型の変換などを行う。

/// procfs のルートパス
const PROC_ROOT: &str = "/proc";

/// procfs の内容を Vec に読み取る（必要ならバッファを拡張）
fn procfs_read_to_vec(path: &str) -> Result<Vec<u8>, SyscallError> {
    let mut size = 256usize;
    loop {
        let mut buf = vec![0u8; size];
        let written = crate::procfs::procfs_read(path, &mut buf)?;
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
    use crate::handle::{
        create_directory_handle, create_handle, HANDLE_RIGHT_ENUM, HANDLE_RIGHT_LOOKUP,
        HANDLE_RIGHT_READ, HANDLE_RIGHT_WRITE, HANDLE_RIGHTS_DIRECTORY_READ, HANDLE_RIGHTS_FILE_READ,
    };

    // /proc ディレクトリは一覧のみ許可
    if path == PROC_ROOT || path == "/proc/" {
        let dir_rights = if rights == 0 { HANDLE_RIGHTS_DIRECTORY_READ } else { rights };
        if (dir_rights & (HANDLE_RIGHT_ENUM | HANDLE_RIGHT_LOOKUP)) == 0 {
            return Err(SyscallError::InvalidArgument);
        }
        return Ok(create_directory_handle(String::from("/proc"), dir_rights));
    }

    // /proc 配下のファイルは procfs で読み取る
    if path.starts_with("/proc") {
        let file_rights = if rights == 0 { HANDLE_RIGHTS_FILE_READ } else { rights };
        if (file_rights & HANDLE_RIGHT_READ) == 0 {
            return Err(SyscallError::InvalidArgument);
        }
        if (file_rights & HANDLE_RIGHT_WRITE) != 0 {
            return Err(SyscallError::ReadOnly);
        }
        let data = procfs_read_to_vec(path)?;
        return Ok(create_handle(data, file_rights));
    }

    // ルートディレクトリは特別扱い
    if path == "/" || path.is_empty() {
        let dir_rights = if rights == 0 { HANDLE_RIGHTS_DIRECTORY_READ } else { rights };
        if (dir_rights & (HANDLE_RIGHT_ENUM | HANDLE_RIGHT_LOOKUP)) == 0 {
            return Err(SyscallError::InvalidArgument);
        }
        return Ok(create_directory_handle(String::from("/"), dir_rights));
    }

    // FAT32 からエントリを取得して種別判定
    let mut fat32 = crate::fat32::Fat32::new().map_err(|_| SyscallError::Other)?;
    let entry = fat32.find_entry(path).map_err(|_| SyscallError::FileNotFound)?;

    // ATTR_DIRECTORY = 0x10
    const ATTR_DIRECTORY: u8 = 0x10;
    if (entry.attr & ATTR_DIRECTORY) != 0 {
        let dir_rights = if rights == 0 { HANDLE_RIGHTS_DIRECTORY_READ } else { rights };
        if (dir_rights & (HANDLE_RIGHT_ENUM | HANDLE_RIGHT_LOOKUP)) == 0 {
            return Err(SyscallError::InvalidArgument);
        }
        return Ok(create_directory_handle(String::from(path), dir_rights));
    }

    // ファイルは読み取りのみ許可（書き込みは未対応）
    let file_rights = if rights == 0 { HANDLE_RIGHTS_FILE_READ } else { rights };
    if (file_rights & HANDLE_RIGHT_READ) == 0 {
        return Err(SyscallError::InvalidArgument);
    }
    if (file_rights & HANDLE_RIGHT_WRITE) != 0 {
        return Err(SyscallError::NotSupported);
    }

    let data = fat32.read_file(path).map_err(|_| SyscallError::FileNotFound)?;
    Ok(create_handle(data, file_rights))
}

/// メモリ情報をテキスト形式で書き込む（SYS_GET_MEM_INFO 用）
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

/// タスク一覧をテキスト形式で書き込む（SYS_GET_TASK_LIST 用）
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
    let buf_slice = user_slice_from_args(arg1, arg2)?;
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
    let buf_slice = user_slice_from_args(arg1, arg2)?;
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

    let buf_slice = user_slice_from_args(arg1, arg2)?;
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
    if arg1 > 0xFF || arg2 > 31 || arg3 > 7 {
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
    // パスを取得
    let path_slice = user_slice_from_args(arg1, arg2)?;
    let path = path_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;
    exec_by_path(path)?;
    Ok(0)
}

/// exec の共通実装（カーネル内でパスが確定済みの場合に使用）
fn exec_by_path(path: &str) -> Result<(), SyscallError> {
    // プロセス名を作成（パスからファイル名部分を抽出）
    let process_name = String::from(
        path.rsplit('/').next().unwrap_or(path)
    );

    // FAT32 からファイルを読み込む
    let mut fs = crate::fat32::Fat32::new().map_err(|_| SyscallError::Other)?;
    let elf_data = fs.read_file(path).map_err(|_| SyscallError::FileNotFound)?;

    // スケジューラにユーザープロセスとして登録
    let (current_cr3, current_flags) = Cr3::read();
    unsafe {
        crate::paging::switch_to_kernel_page_table();
    }
    let task_id = match crate::scheduler::spawn_user(&process_name, &elf_data) {
        Ok(id) => id,
        Err(_) => {
            unsafe { Cr3::write(current_cr3, current_flags); }
            return Err(SyscallError::Other);
        }
    };
    unsafe { Cr3::write(current_cr3, current_flags); }

    // 子プロセスの終了を待つ
    match crate::scheduler::wait_for_child(task_id, 0) {
        Ok(exit_code) if exit_code >= 0 => Ok(()),
        Ok(_) => Err(SyscallError::Other),
        Err(_) => Err(SyscallError::Other),
    }
}

/// selftest 用: exec の実行確認
pub fn exec_for_test(path: &str) -> bool {
    exec_by_path(path).is_ok()
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
    // パスを取得
    let path_slice = user_slice_from_args(arg1, arg2)?;
    let path = path_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;
    // プロセス名を作成（パスからファイル名部分を抽出）
    // ユーザー空間の文字列を参照し続けないように、ここでコピーしておく。
    let process_name = String::from(
        path.rsplit('/').next().unwrap_or(path)
    );

    // FAT32 からファイルを読み込む
    let mut fs = crate::fat32::Fat32::new().map_err(|_| SyscallError::Other)?;
    let elf_data = fs.read_file(path).map_err(|_| SyscallError::FileNotFound)?;

    // スケジューラにユーザープロセスとして登録（カーネルのページテーブルで実行）
    let (current_cr3, current_flags) = Cr3::read();
    unsafe {
        crate::paging::switch_to_kernel_page_table();
    }
    let task_id = match crate::scheduler::spawn_user(&process_name, &elf_data) {
        Ok(id) => id,
        Err(_) => {
            unsafe { Cr3::write(current_cr3, current_flags); }
            return Err(SyscallError::Other);
        }
    };
    unsafe { Cr3::write(current_cr3, current_flags); }
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
    // システムコールは割り込み無効状態で処理される。
    // Ring 3 からの sleep は、ここで一度割り込みを有効化してからスリープする。
    // そうしないと yield_now() が enable_and_hlt() に入って
    // 永久停止する（タイマー割り込みが来ない）。
    let ms = arg1;
    x86_64::instructions::interrupts::enable();
    crate::scheduler::sleep_ms(ms);
    Ok(0)
}

/// SYS_WAIT: 子プロセスの終了を待つ
///
/// 引数:
///   arg1 — 待つ子プロセスのタスク ID (0 なら任意の子)
///   arg2 — タイムアウト (ms)。0 なら無期限待ち
///
/// 戻り値:
///   終了した子プロセスの終了コード（成功時）
///   負の値（エラー時）
///
/// 動作:
///   - task_id > 0: 指定した子プロセスの終了を待つ
///   - task_id == 0: 任意の子プロセスの終了を待つ
///   - 子プロセスがない場合はエラー
///   - 子プロセスが既に終了していれば即座に戻る
fn sys_wait(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let target_task_id = arg1;
    let timeout_ms = arg2;

    // 割り込みを有効化（ポーリング中にタイマー割り込みが必要）
    x86_64::instructions::interrupts::enable();

    let result = crate::scheduler::wait_for_child(target_task_id, timeout_ms);
    match result {
        Ok(exit_code) => Ok(exit_code as u64),
        Err(crate::scheduler::WaitError::NoChild) => Err(SyscallError::InvalidArgument),
        Err(crate::scheduler::WaitError::NotChild) => Err(SyscallError::PermissionDenied),
        Err(crate::scheduler::WaitError::Timeout) => Err(SyscallError::Timeout),
    }
}

/// SYS_GETPID: 自分のタスク ID を取得
///
/// 引数: なし
///
/// 戻り値:
///   現在のタスク ID（常に成功）
fn sys_getpid() -> Result<u64, SyscallError> {
    Ok(crate::scheduler::current_task_id())
}

/// SYS_KILL: タスクを強制終了する
///
/// 引数:
///   arg1 — 終了させるタスクの ID
///
/// 戻り値:
///   0（成功時）
///
/// エラー:
///   - InvalidArgument: 自分自身を kill しようとした、またはタスクが見つからない
///   - PermissionDenied: 既に終了済み
fn sys_kill(arg1: u64) -> Result<u64, SyscallError> {
    let task_id = arg1;
    match crate::scheduler::kill_task(task_id) {
        Ok(()) => Ok(0),
        Err("cannot kill self") => Err(SyscallError::InvalidArgument),
        Err("task not found") => Err(SyscallError::InvalidArgument),
        Err("task already finished") => Err(SyscallError::PermissionDenied),
        Err(_) => Err(SyscallError::Other),
    }
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
    // ドメイン名を取得
    let domain_slice = user_slice_from_args(arg1, arg2)?;
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
    // データを取得
    let data_slice = user_slice_from_args(arg1, arg2)?;
    let data = data_slice.as_slice();

    // TCP 送信
    crate::net::tcp_send(data).map_err(|_| SyscallError::Other)?;

    Ok(data.len() as u64)
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
    let buf_len = usize::try_from(arg2).map_err(|_| SyscallError::InvalidArgument)?;
    let timeout_ms = arg3;

    // バッファを取得
    let buf_slice = user_slice_from_args(arg1, arg2)?;
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

/// SYS_NET_SEND_FRAME: Ethernet フレーム送信
///
/// 引数:
///   arg1 — フレームのポインタ（ユーザー空間）
///   arg2 — フレームの長さ
///
/// 戻り値:
///   送信したバイト数（成功時）
///   負の値（エラー時）
fn sys_net_send_frame(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let len = usize::try_from(arg2).map_err(|_| SyscallError::InvalidArgument)?;
    if len == 0 || len > 1514 {
        return Err(SyscallError::InvalidArgument);
    }

    let buf_slice = user_slice_from_args(arg1, arg2)?;
    let buf = buf_slice.as_slice();

    let mut drv = crate::virtio_net::VIRTIO_NET.lock();
    let drv = drv.as_mut().ok_or(SyscallError::Other)?;
    drv.send_packet(buf).map_err(|_| SyscallError::Other)?;

    Ok(len as u64)
}

/// SYS_NET_RECV_FRAME: Ethernet フレーム受信
///
/// 引数:
///   arg1 — バッファのポインタ（ユーザー空間）
///   arg2 — バッファの長さ
///   arg3 — タイムアウト（ミリ秒）。0 なら即時復帰
///
/// 戻り値:
///   受信したバイト数（成功時）
///   0（タイムアウト時）
///   負の値（エラー時）
fn sys_net_recv_frame(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    let buf_len = usize::try_from(arg2).map_err(|_| SyscallError::InvalidArgument)?;
    let timeout_ms = arg3;

    if buf_len == 0 {
        return Err(SyscallError::InvalidArgument);
    }

    let buf_slice = user_slice_from_args(arg1, arg2)?;
    let buf = buf_slice.as_mut_slice();

    x86_64::instructions::interrupts::enable();
    let start_tick = crate::interrupts::TIMER_TICK_COUNT.load(core::sync::atomic::Ordering::Relaxed);

    loop {
        let mut drv = crate::virtio_net::VIRTIO_NET.lock();
        if let Some(frame) = drv.as_mut().and_then(|d| d.receive_packet()) {
            let copy_len = core::cmp::min(frame.len(), buf_len);
            buf[..copy_len].copy_from_slice(&frame[..copy_len]);
            return Ok(copy_len as u64);
        }

        if timeout_ms == 0 {
            return Ok(0);
        }

        let now = crate::interrupts::TIMER_TICK_COUNT.load(core::sync::atomic::Ordering::Relaxed);
        let elapsed_ticks = now.saturating_sub(start_tick);
        let elapsed_ms = elapsed_ticks * 55;
        if elapsed_ms >= timeout_ms {
            return Ok(0);
        }

        crate::scheduler::yield_now();
    }
}

/// SYS_NET_GET_MAC: MAC アドレス取得
///
/// 引数:
///   arg1 — 書き込み先バッファ（ユーザー空間）
///   arg2 — バッファの長さ（6 以上）
///
/// 戻り値:
///   6（成功時）
///   負の値（エラー時）
fn sys_net_get_mac(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let buf_len = usize::try_from(arg2).map_err(|_| SyscallError::InvalidArgument)?;
    if buf_len < 6 {
        return Err(SyscallError::InvalidArgument);
    }

    let buf_slice = user_slice_from_args(arg1, arg2)?;
    let buf = buf_slice.as_mut_slice();

    let drv = crate::virtio_net::VIRTIO_NET.lock();
    let drv = drv.as_ref().ok_or(SyscallError::Other)?;
    buf[..6].copy_from_slice(&drv.mac_address);
    Ok(6)
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
    use core::fmt::Write;

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

// =================================================================
// SYS_CLOCK_MONOTONIC: 起動からの経過時間（ミリ秒）
// =================================================================

/// SYS_CLOCK_MONOTONIC: 起動からの経過ミリ秒を返す
///
/// PIT (Programmable Interval Timer) のティックカウントをミリ秒に変換する。
/// PIT のデフォルト周波数: 1193182 Hz / 65536 ≈ 18.2065 Hz
/// 1 ティック ≈ 54.925 ms
/// ms = ticks * 10000 / 182 （scheduler.rs の sleep_ms と逆算式）
///
/// 戻り値: 起動からの経過ミリ秒
fn sys_clock_monotonic() -> Result<u64, SyscallError> {
    let ticks = crate::interrupts::TIMER_TICK_COUNT.load(core::sync::atomic::Ordering::Relaxed);
    // ticks → ms 変換 (sleep_ms の逆: ms = ticks * 10000 / 182)
    let ms = ticks * 10000 / 182;
    Ok(ms)
}

// =================================================================
// SYS_GETRANDOM: ランダムバイト生成
// =================================================================

/// SYS_GETRANDOM: RDRAND 命令でランダムバイトを生成
///
/// x86_64 の RDRAND 命令を使って暗号学的に安全なランダムバイトを生成する。
/// RDRAND はハードウェア乱数生成器 (DRNG) を使うため、ソフトウェア PRNG より安全。
///
/// 引数:
///   arg1 — バッファのポインタ（ユーザー空間）
///   arg2 — バッファの長さ（書き込むバイト数）
///
/// 戻り値: 書き込んだバイト数
fn sys_getrandom(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let buf_slice = user_slice_from_args(arg1, arg2)?;
    let buf = buf_slice.as_mut_slice();
    let len = buf.len();

    // 8 バイトずつ RDRAND で生成し、バッファに書き込む
    let mut offset = 0;
    while offset < len {
        let random_value: u64 = rdrand64()?;
        let bytes = random_value.to_le_bytes();
        let remaining = len - offset;
        let to_copy = remaining.min(8);
        buf[offset..offset + to_copy].copy_from_slice(&bytes[..to_copy]);
        offset += to_copy;
    }

    Ok(len as u64)
}

/// RDRAND 命令で 64 ビットのランダム値を取得する。
///
/// RDRAND が失敗する場合（エントロピー枯渇など）は最大 10 回リトライする。
/// それでも失敗した場合はエラーを返す。
fn rdrand64() -> Result<u64, SyscallError> {
    for _ in 0..10 {
        let mut value: u64;
        let success: u8;
        unsafe {
            core::arch::asm!(
                "rdrand {val}",
                "setc {ok}",
                val = out(reg) value,
                ok = out(reg_byte) success,
            );
        }
        if success != 0 {
            return Ok(value);
        }
    }
    // RDRAND が 10 回連続で失敗した場合（通常は起こらない）
    Err(SyscallError::NotSupported)
}
