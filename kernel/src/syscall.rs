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
use alloc::vec::Vec;
use crate::user_ptr::{UserPtr, UserSlice, SyscallError};
use x86_64::registers::control::Cr3;

/// システムコール番号の定義
///
/// sabos-syscall クレートで一元管理している。
/// 番号の追加・変更は libs/sabos-syscall/src/lib.rs で行うこと。
pub use sabos_syscall::*;

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
        SYS_PIPE => sys_pipe(arg1, arg2),
        SYS_SPAWN_REDIRECTED => sys_spawn_redirected(arg1),
        // テスト/デバッグ
        SYS_SELFTEST => sys_selftest(arg1),
        // ファイルシステム
        SYS_FILE_DELETE => sys_file_delete(arg1, arg2),
        SYS_DIR_LIST => sys_dir_list(arg1, arg2, arg3, arg4),
        SYS_FILE_WRITE => sys_file_write(arg1, arg2, arg3, arg4),
        SYS_DIR_CREATE => sys_dir_create(arg1, arg2),
        SYS_DIR_REMOVE => sys_dir_remove(arg1, arg2),
        SYS_FS_STAT => sys_fs_stat(arg1, arg2),
        // システム情報
        SYS_GET_MEM_INFO => sys_get_mem_info(arg1, arg2),
        SYS_GET_TASK_LIST => sys_get_task_list(arg1, arg2),
        SYS_GET_NET_INFO => sys_get_net_info(arg1, arg2),
        SYS_PCI_CONFIG_READ => sys_pci_config_read(arg1, arg2, arg3, arg4),
        SYS_GET_FB_INFO => sys_get_fb_info(arg1, arg2),
        SYS_MOUSE_READ => sys_mouse_read(arg1, arg2),
        SYS_CLOCK_MONOTONIC => sys_clock_monotonic(),
        SYS_GETRANDOM => sys_getrandom(arg1, arg2),
        SYS_MMAP => sys_mmap(arg1, arg2, arg3, arg4),
        SYS_MUNMAP => sys_munmap(arg1, arg2),
        // プロセス管理
        SYS_EXEC => sys_exec(arg1, arg2, arg3, arg4),
        SYS_SPAWN => sys_spawn(arg1, arg2, arg3, arg4),
        SYS_YIELD => sys_yield(),
        SYS_SLEEP => sys_sleep(arg1),
        SYS_WAIT => sys_wait(arg1, arg2),
        SYS_WAITPID => sys_waitpid(arg1, arg2, arg3),
        SYS_GETPID => sys_getpid(),
        SYS_KILL => sys_kill(arg1),
        SYS_GETENV => sys_getenv(arg1, arg2, arg3, arg4),
        SYS_SETENV => sys_setenv(arg1, arg2, arg3, arg4),
        SYS_LISTENV => sys_listenv(arg1, arg2),
        // ネットワーク
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
        SYS_HANDLE_STAT => sys_handle_stat(arg1, arg2),
        SYS_HANDLE_SEEK => sys_handle_seek(arg1, arg2, arg3),
        // ハンドル操作拡張
        SYS_HANDLE_CREATE_FILE => sys_handle_create_file(arg1, arg2, arg3, arg4),
        SYS_HANDLE_UNLINK => sys_handle_unlink(arg1, arg2, arg3),
        SYS_HANDLE_MKDIR => sys_handle_mkdir(arg1, arg2, arg3),
        // ブロックデバイス
        SYS_BLOCK_READ => sys_block_read(arg1, arg2, arg3, arg4),
        SYS_BLOCK_WRITE => sys_block_write(arg1, arg2, arg3, arg4),
        // IPC
        SYS_IPC_SEND => sys_ipc_send(arg1, arg2, arg3),
        SYS_IPC_RECV => sys_ipc_recv(arg1, arg2, arg3, arg4),
        SYS_IPC_CANCEL => sys_ipc_cancel(arg1),
        SYS_IPC_SEND_HANDLE => sys_ipc_send_handle(arg1, arg2, arg3, arg4),
        SYS_IPC_RECV_HANDLE => sys_ipc_recv_handle(arg1, arg2, arg3, arg4),
        // サウンド
        SYS_SOUND_PLAY => sys_sound_play(arg1, arg2),
        // スレッド
        SYS_THREAD_CREATE => sys_thread_create(arg1, arg2, arg3),
        SYS_THREAD_EXIT => sys_thread_exit(arg1),
        SYS_THREAD_JOIN => sys_thread_join(arg1, arg2),
        // Futex
        SYS_FUTEX => sys_futex(arg1, arg2, arg3, arg4),
        // 時刻
        SYS_CLOCK_REALTIME => sys_clock_realtime(),
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

    // stdin がパイプにリダイレクトされている場合はパイプから読み取り
    if let Some(stdin_handle) = crate::scheduler::current_stdin_handle() {
        // パイプからの読み取り: WouldBlock の場合は yield + retry でブロッキング
        // 割り込みを有効化してから yield する必要がある
        x86_64::instructions::interrupts::enable();
        loop {
            match crate::handle::read(&stdin_handle, buf) {
                Ok(n) => return Ok(n as u64),
                Err(SyscallError::WouldBlock) => {
                    // データがまだない、writer がまだ生きている → yield して再試行
                    crate::scheduler::yield_now();
                }
                Err(e) => return Err(e),
            }
        }
    }

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

    // stdout がパイプにリダイレクトされている場合はパイプに書き込み
    if let Some(stdout_handle) = crate::scheduler::current_stdout_handle() {
        let data = user_slice.as_slice();
        return match crate::handle::write(&stdout_handle, data) {
            Ok(n) => Ok(n as u64),
            Err(e) => Err(e),
        };
    }

    // UTF-8 として解釈してカーネルコンソールに出力
    // as_str_lossy() は不正な UTF-8 を "<invalid utf-8>" に置換
    let s = user_slice.as_str_lossy();
    crate::kprint!("{}", s);

    // 書き込んだバイト数を返す
    Ok(len as u64)
}

/// SYS_PIPE: パイプを作成する
///
/// 引数:
///   arg1 — 読み取りハンドルの書き込み先ポインタ（ユーザー空間）
///   arg2 — 書き込みハンドルの書き込み先ポインタ（ユーザー空間）
///
/// 戻り値:
///   0（成功）
///
/// 読み取り用と書き込み用の Handle ペアを作成し、ユーザー空間に書き込む。
fn sys_pipe(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    // ユーザー空間のポインタを検証
    let read_handle_ptr = user_ptr_from_arg::<crate::handle::Handle>(arg1)?;
    let write_handle_ptr = user_ptr_from_arg::<crate::handle::Handle>(arg2)?;

    // パイプハンドルペアを作成
    let (read_handle, write_handle) = crate::handle::create_pipe_handles();

    // ユーザー空間に書き込み
    read_handle_ptr.write(read_handle);
    write_handle_ptr.write(write_handle);

    Ok(0)
}

/// SYS_SPAWN_REDIRECTED: stdin/stdout リダイレクト付きプロセス起動
///
/// 引数:
///   arg1 — SpawnRedirectArgs 構造体のポインタ（ユーザー空間）
///
/// 戻り値:
///   子プロセスのタスク ID
///
/// 構造体ベースのシステムコール。引数が 4 つを超えるため、
/// ユーザー空間の構造体ポインタで渡す。
fn sys_spawn_redirected(arg1: u64) -> Result<u64, SyscallError> {
    /// stdin/stdout リダイレクト付きプロセス起動の引数構造体
    ///
    /// ユーザー空間で構築してポインタを渡す。
    /// handle_id が u64::MAX の場合はリダイレクトなし（コンソール直結）。
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct SpawnRedirectArgs {
        path_ptr: u64,
        path_len: u64,
        args_ptr: u64,
        args_len: u64,
        stdin_handle_id: u64,     // u64::MAX = リダイレクトなし
        stdin_handle_token: u64,
        stdout_handle_id: u64,    // u64::MAX = リダイレクトなし
        stdout_handle_token: u64,
    }

    // ユーザー空間から構造体を読み取り
    let args_ptr = user_ptr_from_arg::<SpawnRedirectArgs>(arg1)?;
    let args = args_ptr.read();

    // パスを取得
    let path_slice = user_slice_from_args(args.path_ptr, args.path_len)?;
    let path = path_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;
    let process_name = String::from(
        path.rsplit('/').next().unwrap_or(path)
    );
    let path_owned = String::from(path);

    // 追加引数をパース
    let extra_args = parse_args_buffer(args.args_ptr, args.args_len)?;

    // VFS 経由でファイルを読み込む
    let elf_data = crate::vfs::read_file(path).map_err(crate::vfs::vfs_error_to_syscall)?;

    // argv を構築: [path] + extra_args
    let mut args_vec: Vec<&str> = Vec::with_capacity(1 + extra_args.len());
    args_vec.push(&path_owned);
    for arg in &extra_args {
        args_vec.push(arg.as_str());
    }

    // stdin/stdout ハンドルを構築（u64::MAX = リダイレクトなし）
    let stdin_handle = if args.stdin_handle_id != u64::MAX {
        Some(crate::handle::Handle {
            id: args.stdin_handle_id,
            token: args.stdin_handle_token,
        })
    } else {
        None
    };
    let stdout_handle = if args.stdout_handle_id != u64::MAX {
        Some(crate::handle::Handle {
            id: args.stdout_handle_id,
            token: args.stdout_handle_token,
        })
    } else {
        None
    };

    // ハンドルが指定されている場合は権限を検証
    if let Some(ref h) = stdin_handle {
        crate::handle::check_rights(h, crate::handle::HANDLE_RIGHT_READ)?;
    }
    if let Some(ref h) = stdout_handle {
        crate::handle::check_rights(h, crate::handle::HANDLE_RIGHT_WRITE)?;
    }

    // スケジューラにユーザープロセスとして登録（カーネルのページテーブルで実行）
    let (current_cr3, current_flags) = Cr3::read();
    unsafe {
        crate::paging::switch_to_kernel_page_table();
    }
    let task_id = match crate::scheduler::spawn_user_redirected(
        &process_name, &elf_data, &args_vec, stdin_handle, stdout_handle,
    ) {
        Ok(id) => id,
        Err(_) => {
            unsafe { Cr3::write(current_cr3, current_flags); }
            return Err(SyscallError::Other);
        }
    };
    unsafe { Cr3::write(current_cr3, current_flags); }
    Ok(task_id)
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
/// 引数:
///   arg1: auto_exit フラグ（0 = 通常実行、1 = 完了後に ISA debug exit で QEMU を終了）
/// 戻り値: 0（成功）
fn sys_selftest(auto_exit: u64) -> Result<u64, SyscallError> {
    // selftest 中にタイマー割り込みやタスク切り替えが動くように有効化
    x86_64::instructions::interrupts::enable();
    crate::shell::run_selftest(auto_exit != 0);
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
    use crate::handle::Handle;

    let rights = arg4 as u32;

    // パスを取得
    let path_slice = user_slice_from_args(arg1, arg2)?;
    let path = path_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // Handle の書き込み先
    let handle_ptr = user_ptr_from_arg::<Handle>(arg3)?;

    // VFS 経由で open（/proc への書き込みは VFS が ReadOnly を返す）
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

/// SYS_HANDLE_STAT: Handle のメタデータを取得する
///
/// 引数:
///   arg1 — Handle のポインタ（ユーザー空間）
///   arg2 — HandleStat の書き込み先ポインタ（ユーザー空間）
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
fn sys_handle_stat(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    use crate::handle::{Handle, HandleStat};

    let handle_ptr = user_ptr_from_arg::<Handle>(arg1)?;
    let handle = handle_ptr.read();

    let stat_ptr = user_ptr_from_arg::<HandleStat>(arg2)?;

    let stat = crate::handle::stat(&handle)?;
    stat_ptr.write(stat);
    Ok(0)
}

/// SYS_HANDLE_SEEK: Handle のファイルポジションを変更する
///
/// 引数:
///   arg1 — Handle のポインタ（ユーザー空間）
///   arg2 — offset（i64 として解釈、SEEK_CUR/SEEK_END で負の値あり）
///   arg3 — whence（0=SEEK_SET, 1=SEEK_CUR, 2=SEEK_END）
///
/// 戻り値:
///   新しいポジション（成功時）
///   負の値（エラー時）
fn sys_handle_seek(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    use crate::handle::Handle;

    let handle_ptr = user_ptr_from_arg::<Handle>(arg1)?;
    let handle = handle_ptr.read();

    let offset = arg2 as i64;
    let whence = arg3;

    let new_pos = crate::handle::seek(&handle, offset, whence)?;
    Ok(new_pos)
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

    // 親ディレクトリハンドルの権限を引き継ぐ（Capability-based security の原則）
    let parent_rights = crate::handle::get_rights(&dir_handle)?;

    // ファイル/ディレクトリを開く（親の権限を引き継ぐ）
    let handle = open_path_to_handle(&full_path, parent_rights)?;
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

/// SYS_HANDLE_CREATE_FILE: ディレクトリハンドル内にファイルを作成し、書き込み可能なハンドルを返す
///
/// 引数:
///   arg1 — ディレクトリハンドルのポインタ（ユーザー空間）
///   arg2 — ファイル名のポインタ（ユーザー空間）
///   arg3 — ファイル名の長さ
///   arg4 — 出力ハンドルの書き込み先ポインタ（ユーザー空間）
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
///
/// セキュリティ:
///   - ディレクトリハンドルに CREATE 権限が必要
///   - ファイル名に ".." や "/" は禁止
fn sys_handle_create_file(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    use crate::handle::{Handle, HandleKind, HANDLE_RIGHT_CREATE, HANDLE_RIGHTS_FILE_RW,
                        create_handle_with_path};

    // ディレクトリハンドルを取得
    let dir_handle_ptr = user_ptr_from_arg::<Handle>(arg1)?;
    let dir_handle = dir_handle_ptr.read();

    // ファイル名を取得
    let name_slice = user_slice_from_args(arg2, arg3)?;
    let name = name_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // 出力ハンドルの書き込み先
    let out_handle_ptr = user_ptr_from_arg::<Handle>(arg4)?;

    // 権限チェック（CREATE 権限が必要）
    crate::handle::check_rights(&dir_handle, HANDLE_RIGHT_CREATE)?;

    // ディレクトリハンドルの種類チェック
    if crate::handle::get_kind(&dir_handle)? != HandleKind::Directory {
        return Err(SyscallError::InvalidArgument);
    }

    // ファイル名の検証（".." や "/" を禁止）
    validate_entry_name(name)?;

    // ディレクトリのパスを取得してフルパスを構築
    let dir_path = crate::handle::get_path(&dir_handle)?;
    let full_path = build_child_path(&dir_path, name);

    // VFS 経由でファイルを作成（/proc は VFS が ReadOnly を返す）
    let _ = crate::vfs::delete_file(&full_path); // 既存ファイルの削除（なくてもOK）
    crate::vfs::create_file(&full_path, &[]).map_err(crate::vfs::vfs_error_to_syscall)?;

    // RW 権限付きハンドルを作成して返す
    let handle = create_handle_with_path(Vec::new(), HANDLE_RIGHTS_FILE_RW, String::from(&*full_path));
    out_handle_ptr.write(handle);

    Ok(0)
}

/// SYS_HANDLE_UNLINK: ディレクトリハンドル内のファイルまたは空ディレクトリを削除
///
/// 引数:
///   arg1 — ディレクトリハンドルのポインタ（ユーザー空間）
///   arg2 — ファイル名のポインタ（ユーザー空間）
///   arg3 — ファイル名の長さ
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
///
/// セキュリティ:
///   - ディレクトリハンドルに DELETE 権限が必要
///   - ファイル名に ".." や "/" は禁止
fn sys_handle_unlink(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    use crate::handle::{Handle, HandleKind, HANDLE_RIGHT_DELETE};

    // ディレクトリハンドルを取得
    let dir_handle_ptr = user_ptr_from_arg::<Handle>(arg1)?;
    let dir_handle = dir_handle_ptr.read();

    // ファイル名を取得
    let name_slice = user_slice_from_args(arg2, arg3)?;
    let name = name_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // 権限チェック（DELETE 権限が必要）
    crate::handle::check_rights(&dir_handle, HANDLE_RIGHT_DELETE)?;

    // ディレクトリハンドルの種類チェック
    if crate::handle::get_kind(&dir_handle)? != HandleKind::Directory {
        return Err(SyscallError::InvalidArgument);
    }

    // ファイル名の検証
    validate_entry_name(name)?;

    // フルパスを構築
    let dir_path = crate::handle::get_path(&dir_handle)?;
    let full_path = build_child_path(&dir_path, name);

    // VFS 経由で削除（ファイルを先に試し、失敗したらディレクトリとして削除）
    // /proc は VFS が ReadOnly を返す
    if crate::vfs::delete_file(&full_path).is_err() {
        crate::vfs::delete_dir(&full_path).map_err(crate::vfs::vfs_error_to_syscall)?;
    }

    Ok(0)
}

/// SYS_HANDLE_MKDIR: ディレクトリハンドル内にサブディレクトリを作成
///
/// 引数:
///   arg1 — ディレクトリハンドルのポインタ（ユーザー空間）
///   arg2 — ディレクトリ名のポインタ（ユーザー空間）
///   arg3 — ディレクトリ名の長さ
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
///
/// セキュリティ:
///   - ディレクトリハンドルに CREATE 権限が必要
///   - ディレクトリ名に ".." や "/" は禁止
fn sys_handle_mkdir(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    use crate::handle::{Handle, HandleKind, HANDLE_RIGHT_CREATE};

    // ディレクトリハンドルを取得
    let dir_handle_ptr = user_ptr_from_arg::<Handle>(arg1)?;
    let dir_handle = dir_handle_ptr.read();

    // ディレクトリ名を取得
    let name_slice = user_slice_from_args(arg2, arg3)?;
    let name = name_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // 権限チェック（CREATE 権限が必要）
    crate::handle::check_rights(&dir_handle, HANDLE_RIGHT_CREATE)?;

    // ディレクトリハンドルの種類チェック
    if crate::handle::get_kind(&dir_handle)? != HandleKind::Directory {
        return Err(SyscallError::InvalidArgument);
    }

    // ディレクトリ名の検証
    validate_entry_name(name)?;

    // フルパスを構築
    let dir_path = crate::handle::get_path(&dir_handle)?;
    let full_path = build_child_path(&dir_path, name);

    // VFS 経由でディレクトリを作成（/proc は VFS が ReadOnly を返す）
    crate::vfs::create_dir(&full_path).map_err(crate::vfs::vfs_error_to_syscall)?;

    Ok(0)
}

/// ファイル/ディレクトリ名の検証ヘルパー
///
/// ".." や "/" を含む名前はパストラバーサル攻撃の原因になるため拒否する。
/// 空の名前も拒否する。
fn validate_entry_name(name: &str) -> Result<(), SyscallError> {
    if name.is_empty() {
        return Err(SyscallError::InvalidArgument);
    }
    if name.contains('/') || name.contains('\\') {
        return Err(SyscallError::InvalidArgument);
    }
    if name == ".." || name == "." {
        return Err(SyscallError::PathTraversal);
    }
    Ok(())
}

/// 親ディレクトリのパスと子の名前からフルパスを構築するヘルパー
fn build_child_path(dir_path: &str, name: &str) -> String {
    if dir_path.is_empty() || dir_path == "/" {
        format!("/{}", name)
    } else {
        format!("{}/{}", dir_path, name)
    }
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
///   arg4 — デバイスインデックス（0 = disk.img, 1 = hostfs.img, ...）
///
/// 戻り値:
///   読み取ったバイト数（成功時）
///   負の値（エラー時）
pub(crate) fn sys_block_read(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    let len = usize::try_from(arg3).map_err(|_| SyscallError::InvalidArgument)?;
    if len != 512 {
        return Err(SyscallError::InvalidArgument);
    }
    let dev_index = arg4 as usize;

    let buf_slice = user_slice_from_args(arg2, arg3)?;
    let buf = buf_slice.as_mut_slice();

    let mut devs = crate::virtio_blk::VIRTIO_BLKS.lock();
    let drv = devs.get_mut(dev_index).ok_or(SyscallError::Other)?;
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
///   arg4 — デバイスインデックス（0 = disk.img, 1 = hostfs.img, ...）
///
/// 戻り値:
///   書き込んだバイト数（成功時）
///   負の値（エラー時）
pub(crate) fn sys_block_write(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    let len = usize::try_from(arg3).map_err(|_| SyscallError::InvalidArgument)?;
    if len != 512 {
        return Err(SyscallError::InvalidArgument);
    }
    let dev_index = arg4 as usize;

    let buf_slice = user_slice_from_args(arg2, arg3)?;
    let buf = buf_slice.as_slice();

    let mut devs = crate::virtio_blk::VIRTIO_BLKS.lock();
    let drv = devs.get_mut(dev_index).ok_or(SyscallError::Other)?;
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

/// SYS_IPC_CANCEL: IPC recv 待ちをキャンセルする
///
/// 引数:
///   arg1 — キャンセル対象のタスクID
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
fn sys_ipc_cancel(arg1: u64) -> Result<u64, SyscallError> {
    crate::ipc::cancel_recv(arg1)?;
    Ok(0)
}

/// SYS_IPC_SEND_HANDLE: ハンドル付き IPC メッセージを送信する
///
/// 引数:
///   arg1 — 宛先タスクID
///   arg2 — バッファのポインタ（ユーザー空間）
///   arg3 — バッファの長さ
///   arg4 — ハンドルのポインタ（ユーザー空間、Handle 構造体）
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
fn sys_ipc_send_handle(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    let buf_slice = user_slice_from_args(arg2, arg3)?;
    let buf = buf_slice.as_slice();

    let handle_ptr = user_ptr_from_arg::<crate::handle::Handle>(arg4)?;
    let handle = handle_ptr.read();

    let sender = crate::scheduler::current_task_id();
    crate::ipc::send_with_handle(sender, arg1, buf.to_vec(), &handle)?;
    Ok(0)
}

/// SYS_IPC_RECV_HANDLE: ハンドル付き IPC メッセージを受信する
///
/// 引数:
///   arg1 — 送信元タスクIDの書き込み先（ユーザー空間）
///   arg2 — 受信バッファのポインタ（ユーザー空間）
///   arg3 — 受信バッファの長さ
///   arg4 — ハンドルの書き込み先（ユーザー空間、Handle 構造体）
///
/// 戻り値:
///   読み取ったバイト数（成功時）
///   負の値（エラー時）
fn sys_ipc_recv_handle(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    // IPC 受信は待ちに入る可能性があるため、割り込みを有効化してタイマ割り込みを許可する
    x86_64::instructions::interrupts::enable();

    let sender_ptr = user_ptr_from_arg::<u64>(arg1)?;
    let buf_slice = user_slice_from_args(arg2, arg3)?;
    let buf = buf_slice.as_mut_slice();
    let handle_out_ptr = user_ptr_from_arg::<crate::handle::Handle>(arg4)?;

    let task_id = crate::scheduler::current_task_id();
    let msg = crate::ipc::recv_with_handle(task_id)?;

    let copy_len = core::cmp::min(buf.len(), msg.data.len());
    buf[..copy_len].copy_from_slice(&msg.data[..copy_len]);
    sender_ptr.write(msg.sender);
    handle_out_ptr.write(msg.handle);

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

    // VFS 経由でファイルを削除（/proc は VFS が ReadOnly を返す）
    crate::vfs::delete_file(path).map_err(crate::vfs::vfs_error_to_syscall)?;

    Ok(0)
}

/// SYS_FILE_WRITE: ファイルを作成/上書き
///
/// 既にファイルが存在する場合は削除してから作成する。
///
/// 引数:
///   arg1 — パスのポインタ（ユーザー空間）
///   arg2 — パスの長さ
///   arg3 — データのポインタ（ユーザー空間）
///   arg4 — データの長さ
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
fn sys_file_write(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    // パスを取得
    let path_slice = user_slice_from_args(arg1, arg2)?;
    let path = path_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // データを取得
    let data_slice = user_slice_from_args(arg3, arg4)?;
    let data = data_slice.as_slice();

    // VFS 経由でファイルを作成/上書き（/proc は VFS が ReadOnly を返す）
    // create_file は既存ファイルがあるとエラーになるので、先に削除を試みる
    let _ = crate::vfs::delete_file(path); // 既存ファイルの削除（なくてもOK）
    crate::vfs::create_file(path, data).map_err(crate::vfs::vfs_error_to_syscall)?;

    Ok(0)
}

/// SYS_DIR_CREATE: ディレクトリを作成
///
/// 引数:
///   arg1 — パスのポインタ（ユーザー空間）
///   arg2 — パスの長さ
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
fn sys_dir_create(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    // パスを取得
    let path_slice = user_slice_from_args(arg1, arg2)?;
    let path = path_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // VFS 経由でディレクトリを作成（/proc は VFS が ReadOnly を返す）
    crate::vfs::create_dir(path).map_err(crate::vfs::vfs_error_to_syscall)?;

    Ok(0)
}

/// SYS_DIR_REMOVE: ディレクトリを削除
///
/// 空のディレクトリのみ削除可能。
///
/// 引数:
///   arg1 — パスのポインタ（ユーザー空間）
///   arg2 — パスの長さ
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
fn sys_dir_remove(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    // パスを取得
    let path_slice = user_slice_from_args(arg1, arg2)?;
    let path = path_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // VFS 経由でディレクトリを削除（/proc は VFS が ReadOnly を返す）
    crate::vfs::delete_dir(path).map_err(crate::vfs::vfs_error_to_syscall)?;

    Ok(0)
}

/// SYS_FS_STAT: ファイルシステム統計情報を取得
///
/// JSON 形式でファイルシステムの使用状況をバッファに書き込む。
///
/// 引数:
///   arg1 — バッファのポインタ（ユーザー空間）
///   arg2 — バッファの長さ
///
/// 戻り値:
///   書き込んだバイト数（成功時）
///   負の値（エラー時）
fn sys_fs_stat(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let buf_slice = user_slice_from_args(arg1, arg2)?;
    let buf = buf_slice.as_mut_slice();

    let mut fat32 = crate::fat32::Fat32::new().map_err(|_| SyscallError::Other)?;
    let total_clusters = fat32.total_clusters();
    let free_clusters = fat32.free_clusters().map_err(|_| SyscallError::Other)?;
    let cluster_bytes = fat32.cluster_bytes() as u64;
    let total_bytes = total_clusters as u64 * cluster_bytes;
    let free_bytes = free_clusters as u64 * cluster_bytes;
    let used_bytes = total_bytes.saturating_sub(free_bytes);

    // JSON 形式で書き込む
    let json = format!(
        "{{\"fs\":\"fat32\",\"total_bytes\":{},\"used_bytes\":{},\"free_bytes\":{},\"cluster_bytes\":{},\"total_clusters\":{},\"free_clusters\":{}}}",
        total_bytes, used_bytes, free_bytes, cluster_bytes, total_clusters, free_clusters
    );

    let json_bytes = json.as_bytes();
    if json_bytes.len() > buf.len() {
        return Err(SyscallError::BufferOverflow);
    }
    buf[..json_bytes.len()].copy_from_slice(json_bytes);

    Ok(json_bytes.len() as u64)
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
    // VFS 経由でディレクトリ一覧を取得
    // VFS が自動的に /proc へのルーティングやマウントポイントの追加を行う
    let entries = crate::vfs::list_dir(path).map_err(crate::vfs::vfs_error_to_syscall)?;

    let mut offset = 0;
    for entry in entries {
        let name = &entry.name;
        let is_dir = entry.kind == crate::vfs::VfsNodeKind::Directory;

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

/// パスから Handle を作成する
pub(crate) fn open_path_to_handle(path: &str, rights: u32) -> Result<crate::handle::Handle, SyscallError> {
    use crate::handle::{
        create_directory_handle, create_handle_with_path, HANDLE_RIGHT_ENUM, HANDLE_RIGHT_LOOKUP,
        HANDLE_RIGHT_READ, HANDLE_RIGHT_WRITE, HANDLE_RIGHTS_DIRECTORY_READ, HANDLE_RIGHTS_FILE_READ,
        HANDLE_RIGHTS_FILE_RW,
    };

    // ルートディレクトリは特別扱い
    if path == "/" || path.is_empty() {
        let dir_rights = if rights == 0 { HANDLE_RIGHTS_DIRECTORY_READ } else { rights };
        if (dir_rights & (HANDLE_RIGHT_ENUM | HANDLE_RIGHT_LOOKUP)) == 0 {
            return Err(SyscallError::InvalidArgument);
        }
        return Ok(create_directory_handle(String::from("/"), dir_rights));
    }

    // WRITE 権限付きの場合: 新規ファイル作成も許可する
    let has_write = (rights & HANDLE_RIGHT_WRITE) != 0;

    // パスを正規化してマウント先の FS を判定する
    let normalized = crate::vfs::normalize_path(path)
        .map_err(crate::vfs::vfs_error_to_syscall)?;
    let is_procfs = normalized.starts_with("/proc");

    // procfs 配下に WRITE 権限でオープンは不可
    if has_write && is_procfs {
        return Err(SyscallError::ReadOnly);
    }

    // VFS 経由でファイル/ディレクトリを開く
    match crate::vfs::open(path) {
        Ok(node) => {
            let kind = node.kind();
            match kind {
                crate::vfs::VfsNodeKind::Directory => {
                    // ディレクトリは書き込みオープン不可
                    if has_write {
                        return Err(SyscallError::NotSupported);
                    }
                    let dir_rights = if rights == 0 { HANDLE_RIGHTS_DIRECTORY_READ } else { rights };
                    if (dir_rights & (HANDLE_RIGHT_ENUM | HANDLE_RIGHT_LOOKUP)) == 0 {
                        return Err(SyscallError::InvalidArgument);
                    }
                    return Ok(create_directory_handle(String::from(path), dir_rights));
                }
                crate::vfs::VfsNodeKind::File => {
                    // ファイルデータを読み取る
                    let data = crate::vfs::read_file(path)
                        .map_err(crate::vfs::vfs_error_to_syscall)?;
                    let file_rights = if rights == 0 {
                        if has_write { HANDLE_RIGHTS_FILE_RW } else { HANDLE_RIGHTS_FILE_READ }
                    } else {
                        rights
                    };
                    if !has_write && (file_rights & HANDLE_RIGHT_READ) == 0 {
                        return Err(SyscallError::InvalidArgument);
                    }
                    return Ok(create_handle_with_path(data, file_rights, String::from(path)));
                }
            }
        }
        Err(crate::vfs::VfsError::NotAFile) => {
            // ディレクトリとして扱う（VFS がディレクトリに open() して NotAFile が返された場合）
            if has_write {
                return Err(SyscallError::NotSupported);
            }
            let dir_rights = if rights == 0 { HANDLE_RIGHTS_DIRECTORY_READ } else { rights };
            if (dir_rights & (HANDLE_RIGHT_ENUM | HANDLE_RIGHT_LOOKUP)) == 0 {
                return Err(SyscallError::InvalidArgument);
            }
            return Ok(create_directory_handle(String::from(path), dir_rights));
        }
        Err(crate::vfs::VfsError::NotFound) => {
            // ファイルが見つからない場合
            if has_write {
                // WRITE 権限付きなら新規ファイルとして空データでハンドル作成
                let file_rights = if rights == 0 { HANDLE_RIGHTS_FILE_RW } else { rights };
                Ok(create_handle_with_path(Vec::new(), file_rights, String::from(path)))
            } else {
                // WRITE なしならファイル未発見エラー
                Err(SyscallError::FileNotFound)
            }
        }
        Err(e) => Err(crate::vfs::vfs_error_to_syscall(e)),
    }
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
    let my_ip = crate::net_config::MY_IP;
    let gateway = crate::net_config::GATEWAY_IP;
    let dns = crate::net_config::DNS_SERVER_IP;

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
///   arg3 — 引数バッファのポインタ（0 なら引数なし）
///   arg4 — 引数バッファの長さ
///
/// 引数バッファのフォーマット:
///   [u16 len][bytes] の繰り返し。各引数は長さプレフィックス付きで連続配置。
///   arg3=0 なら後方互換でパスのみを argv[0] として渡す。
///
/// 戻り値:
///   0（成功時、プログラム終了後）
///   負の値（エラー時）
///
/// 指定した ELF ファイルを読み込んで同期実行する。
/// プログラムが終了するまでこのシステムコールはブロックする。
fn sys_exec(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    // パスを取得
    let path_slice = user_slice_from_args(arg1, arg2)?;
    let path = path_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // 追加引数をパース
    let extra_args = parse_args_buffer(arg3, arg4)?;

    exec_by_path_with_args(path, &extra_args)?;
    Ok(0)
}

/// ユーザー空間の引数バッファをパースする。
///
/// 引数バッファのフォーマット: [u16 len][bytes][u16 len][bytes]...
/// 各引数は「2バイトのリトルエンディアン長さ」+「その長さ分のバイト列」で連続配置。
/// args_ptr が 0 の場合は空の Vec を返す（後方互換）。
fn parse_args_buffer(args_ptr: u64, args_len: u64) -> Result<Vec<String>, SyscallError> {
    if args_ptr == 0 || args_len == 0 {
        return Ok(Vec::new());
    }

    let buf = user_slice_from_args(args_ptr, args_len)?;
    let data = buf.as_slice();
    let mut offset = 0;
    let mut args = Vec::new();

    while offset + 2 <= data.len() {
        // 長さプレフィックス（リトルエンディアン u16）を読む
        let len = u16::from_le_bytes([data[offset], data[offset + 1]]) as usize;
        offset += 2;

        if offset + len > data.len() {
            return Err(SyscallError::InvalidArgument);
        }

        let arg_str = core::str::from_utf8(&data[offset..offset + len])
            .map_err(|_| SyscallError::InvalidUtf8)?;
        args.push(String::from(arg_str));
        offset += len;
    }

    Ok(args)
}

/// exec の共通実装（カーネル内でパスが確定済みの場合に使用）
fn exec_by_path(path: &str) -> Result<(), SyscallError> {
    exec_by_path_with_args(path, &[])
}

/// exec の共通実装（追加引数付き）
///
/// argv は [path, extra_args[0], extra_args[1], ...] の形で構築される。
/// extra_args が空なら path のみが argv[0] になる（後方互換）。
fn exec_by_path_with_args(path: &str, extra_args: &[String]) -> Result<(), SyscallError> {
    // プロセス名を作成（パスからファイル名部分を抽出）
    let process_name = String::from(
        path.rsplit('/').next().unwrap_or(path)
    );

    // VFS 経由でファイルを読み込む
    let elf_data = crate::vfs::read_file(path).map_err(crate::vfs::vfs_error_to_syscall)?;

    // argv を構築: [path] + extra_args
    // path はユーザー空間メモリを指す &str なので、カーネルページテーブルに
    // 切り替える前にカーネルヒープにコピーしておく必要がある。
    // switch_to_kernel_page_table() 後はユーザー空間のアドレスは
    // カーネルのアイデンティティマッピング経由で別の物理アドレスを読んでしまう。
    let path_owned = String::from(path);
    let mut args_vec: Vec<&str> = Vec::with_capacity(1 + extra_args.len());
    args_vec.push(&path_owned);
    for arg in extra_args {
        args_vec.push(arg.as_str());
    }

    // スケジューラにユーザープロセスとして登録
    let (current_cr3, current_flags) = Cr3::read();
    unsafe {
        crate::paging::switch_to_kernel_page_table();
    }
    let task_id = match crate::scheduler::spawn_user(&process_name, &elf_data, &args_vec) {
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

/// selftest 用: ELF を spawn してタスク ID を返す（waitpid テスト用）
///
/// exec_by_path と同じロジックだが、wait_for_child を呼ばずに
/// spawn だけして task_id を返す。呼び出し元で waitpid を使って回収する。
pub fn exec_spawn_for_test(path: &str) -> Result<u64, SyscallError> {
    use alloc::string::String;
    use alloc::vec::Vec;
    use x86_64::registers::control::Cr3;

    let process_name = String::from(
        path.rsplit('/').next().unwrap_or(path)
    );

    let elf_data = crate::vfs::read_file(path).map_err(crate::vfs::vfs_error_to_syscall)?;

    let args_vec: Vec<&str> = alloc::vec![path];

    let (current_cr3, current_flags) = Cr3::read();
    unsafe {
        crate::paging::switch_to_kernel_page_table();
    }
    let task_id = match crate::scheduler::spawn_user(&process_name, &elf_data, &args_vec) {
        Ok(id) => id,
        Err(_) => {
            unsafe { Cr3::write(current_cr3, current_flags); }
            return Err(SyscallError::Other);
        }
    };
    unsafe { Cr3::write(current_cr3, current_flags); }

    Ok(task_id)
}

/// selftest 用: 引数・環境変数付きで exec を実行する
///
/// 指定したパスの ELF を args と env_vars 付きで spawn し、終了を待つ。
/// テスト用なので、環境変数は呼び出し元のタスクに一時的に設定してから
/// spawn する（spawn 時に子に継承される）。
pub fn exec_with_args_for_test(path: &str, args: &[&str], env_vars: &[(&str, &str)]) -> bool {
    use alloc::string::String;

    let process_name = String::from(
        path.rsplit('/').next().unwrap_or(path)
    );

    // 環境変数を現在のタスクに一時的に設定する（spawn で子に継承される）
    for &(key, value) in env_vars {
        crate::scheduler::set_env_var(key, value);
    }

    // VFS 経由でファイルを読み込む
    let elf_data = match crate::vfs::read_file(path) {
        Ok(data) => data,
        Err(_) => return false,
    };

    // カーネルのページテーブルで spawn する
    let (current_cr3, current_flags) = Cr3::read();
    unsafe {
        crate::paging::switch_to_kernel_page_table();
    }
    let task_id = match crate::scheduler::spawn_user(&process_name, &elf_data, args) {
        Ok(id) => id,
        Err(_) => {
            unsafe { Cr3::write(current_cr3, current_flags); }
            return false;
        }
    };
    unsafe { Cr3::write(current_cr3, current_flags); }

    // テスト用の環境変数を削除（クリーンアップ）
    // 注: 簡略化のため、キーを空文字列に設定（現在は削除 API がないため）
    // 子プロセスには既に継承済みなので問題ない

    // 子プロセスの終了を待つ
    match crate::scheduler::wait_for_child(task_id, 0) {
        Ok(exit_code) if exit_code >= 0 => true,
        _ => false,
    }
}

/// SYS_SPAWN: バックグラウンドでプロセスを起動
///
/// 引数:
///   arg1 — パスのポインタ（ユーザー空間）
///   arg2 — パスの長さ
///   arg3 — 引数バッファのポインタ（0 なら引数なし）
///   arg4 — 引数バッファの長さ
///
/// 引数バッファのフォーマット:
///   [u16 len][bytes] の繰り返し。各引数は長さプレフィックス付きで連続配置。
///   arg3=0 なら後方互換でパスのみを argv[0] として渡す。
///
/// 戻り値:
///   タスク ID（成功時）
///   負の値（エラー時）
///
/// 指定した ELF ファイルを読み込んでバックグラウンドで実行する。
/// 即座に戻り、プロセスはスケジューラで管理される。
fn sys_spawn(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    // パスを取得
    let path_slice = user_slice_from_args(arg1, arg2)?;
    let path = path_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;
    // プロセス名を作成（パスからファイル名部分を抽出）
    // ユーザー空間の文字列を参照し続けないように、ここでコピーしておく。
    let process_name = String::from(
        path.rsplit('/').next().unwrap_or(path)
    );

    // パスもコピーしておく（ユーザー空間の文字列は spawn 後に無効になる可能性がある）
    let path_owned = String::from(path);

    // 追加引数をパース
    let extra_args = parse_args_buffer(arg3, arg4)?;

    // VFS 経由でファイルを読み込む
    let elf_data = crate::vfs::read_file(path).map_err(crate::vfs::vfs_error_to_syscall)?;

    // argv を構築: [path] + extra_args
    let mut args_vec: Vec<&str> = Vec::with_capacity(1 + extra_args.len());
    args_vec.push(&path_owned);
    for arg in &extra_args {
        args_vec.push(arg.as_str());
    }

    // スケジューラにユーザープロセスとして登録（カーネルのページテーブルで実行）
    let (current_cr3, current_flags) = Cr3::read();
    unsafe {
        crate::paging::switch_to_kernel_page_table();
    }
    let task_id = match crate::scheduler::spawn_user(&process_name, &elf_data, &args_vec) {
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

/// SYS_WAITPID: 子プロセスの終了を待つ（拡張版）
///
/// 引数:
///   arg1 — 待つ子プロセスのタスク ID (0 なら任意の子)
///   arg2 — 終了コードの書き込み先ユーザー空間ポインタ (0 なら無視)
///   arg3 — フラグ（WNOHANG=1: 終了済みの子がいなければ即座に 0 を返す）
///
/// 戻り値:
///   終了した子プロセスのタスク ID（成功時）
///   0（WNOHANG で終了済みの子がいなかった場合）
///   負の値（エラー時）
///
/// 動作:
///   - SYS_WAIT との違い: どの子が終了したかの task_id を戻り値で返し、
///     exit_code はユーザー空間ポインタ経由で書き込む
///   - WNOHANG フラグでノンブロッキング待ちが可能
fn sys_waitpid(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    let target_task_id = arg1;
    let exit_code_ptr_raw = arg2;
    let flags = arg3;

    // 割り込みを有効化（ポーリング中にタイマー割り込みが必要）
    x86_64::instructions::interrupts::enable();

    let result = crate::scheduler::waitpid(target_task_id, flags);
    match result {
        Ok((child_id, exit_code)) => {
            // exit_code_ptr が 0 でなければ、ユーザー空間に終了コードを書き込む
            if exit_code_ptr_raw != 0 {
                let exit_code_ptr = user_ptr_from_arg::<i64>(exit_code_ptr_raw)?;
                exit_code_ptr.write(exit_code as i64);
            }
            Ok(child_id)
        }
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
// 環境変数関連システムコール
// =================================================================

/// SYS_GETENV: 環境変数を取得する
///
/// 引数:
///   arg1 — key のポインタ（ユーザー空間）
///   arg2 — key の長さ
///   arg3 — value を書き込むバッファのポインタ
///   arg4 — value バッファの長さ
///
/// 戻り値:
///   value の長さ（成功時、バッファに書き込み済み）
///   -20 (FILE_NOT_FOUND): 指定した key の環境変数が存在しない
///   -4 (BUFFER_OVERFLOW): バッファが小さすぎる
fn sys_getenv(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    // key を取得
    let key_slice = user_slice_from_args(arg1, arg2)?;
    let key = key_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // 現在のタスクの環境変数から key を検索
    let value = crate::scheduler::get_env_var(key)
        .ok_or(SyscallError::FileNotFound)?;

    // バッファに書き込む
    let val_buf = user_slice_from_args(arg3, arg4)?;
    if value.len() > val_buf.as_slice().len() {
        return Err(SyscallError::BufferOverflow);
    }

    val_buf.as_mut_slice()[..value.len()].copy_from_slice(value.as_bytes());
    Ok(value.len() as u64)
}

/// SYS_SETENV: 環境変数を設定する
///
/// 引数:
///   arg1 — key のポインタ（ユーザー空間）
///   arg2 — key の長さ
///   arg3 — value のポインタ
///   arg4 — value の長さ
///
/// 戻り値:
///   0（成功時）
fn sys_setenv(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    // key を取得
    let key_slice = user_slice_from_args(arg1, arg2)?;
    let key = key_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // value を取得
    let val_slice = user_slice_from_args(arg3, arg4)?;
    let value = val_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // 現在のタスクの環境変数に設定
    crate::scheduler::set_env_var(key, value);
    Ok(0)
}

/// SYS_LISTENV: 全環境変数を一覧取得する
///
/// 引数:
///   arg1 — バッファのポインタ（ユーザー空間）
///   arg2 — バッファの長さ
///
/// バッファに "KEY=VALUE\n" の繰り返しで書き込む。
///
/// 戻り値:
///   書き込んだバイト数（成功時）
///   -4 (BUFFER_OVERFLOW): バッファが小さすぎる
fn sys_listenv(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let buf = user_slice_from_args(arg1, arg2)?;

    // 全環境変数を "KEY=VALUE\n" 形式で取得
    let data = crate::scheduler::list_env_vars();

    if data.len() > buf.as_slice().len() {
        return Err(SyscallError::BufferOverflow);
    }

    buf.as_mut_slice()[..data.len()].copy_from_slice(data.as_bytes());
    Ok(data.len() as u64)
}

// =================================================================
// ネットワーク関連システムコール
// =================================================================
// DNS/TCP 系 syscall（SYS_DNS_LOOKUP, SYS_TCP_CONNECT/SEND/RECV/CLOSE）は
// netd デーモンに一元化したため削除済み。
// 残っているのは raw フレーム送受信（SYS_NET_SEND_FRAME/RECV_FRAME/GET_MAC）のみ。

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
        {
            let mut drv = crate::virtio_net::VIRTIO_NET.lock();
            if let Some(frame) = drv.as_mut().and_then(|d| d.receive_packet()) {
                let copy_len = core::cmp::min(frame.len(), buf_len);
                buf[..copy_len].copy_from_slice(&frame[..copy_len]);
                return Ok(copy_len as u64);
            }
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

        // QEMU TCG モードでは、ゲスト CPU がビジーループしていると
        // SLIRP のネットワーク I/O が処理されない。
        //
        // ISR ステータスの読み取り（port I/O）で QEMU のイベントループを
        // キックし、SLIRP が受信パケットを処理できるようにする。
        // その後 hlt で CPU を停止して、タイマー割り込みまで待機する。
        {
            let mut drv = crate::virtio_net::VIRTIO_NET.lock();
            if let Some(d) = drv.as_mut() {
                d.read_isr_status();
            }
        }
        x86_64::instructions::interrupts::enable_and_hlt();
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
// SYS_CLOCK_REALTIME: 壁時計時刻の取得
// =================================================================

/// SYS_CLOCK_REALTIME: CMOS RTC から現在時刻を読み取り、
/// UNIX エポック（1970-01-01 00:00:00 UTC）からの秒数を返す。
///
/// 戻り値: UNIX エポックからの秒数
fn sys_clock_realtime() -> Result<u64, SyscallError> {
    Ok(crate::rtc::read_unix_epoch_seconds())
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

// SYS_MMAP / SYS_MUNMAP: 匿名ページの動的マッピング/解除
//
// ユーザー空間から動的にメモリを確保するためのシステムコール。
// POSIX の mmap(MAP_ANONYMOUS) に相当するが、ファイルマッピングは未対応。
// std の GlobalAlloc や、ユーザー空間のヒープ拡張に使う。

/// MMAP_PROT_READ: 読み取り可能
const MMAP_PROT_READ: u64 = 0x1;
/// MMAP_PROT_WRITE: 書き込み可能
const MMAP_PROT_WRITE: u64 = 0x2;
/// MMAP_FLAG_ANONYMOUS: 匿名マッピング（ファイルに紐付かない）
const MMAP_FLAG_ANONYMOUS: u64 = 0x1;

/// mmap 用の仮想アドレスの下限。
/// ELF の LOAD セグメント、ユーザースタック (0x2000000)、
/// およびカーネルのアイデンティティマッピング（物理 RAM 範囲）と
/// 重ならないように、十分に高いアドレスから割り当てる。
///
/// UEFI は物理メモリを 1GiB ヒュージページで identity mapping するため、
/// L4[0] の範囲（0x0 ～ 0x7F_FFFF_FFFF = 512GiB）には identity mapping の
/// ページテーブルエントリが存在する可能性がある。
/// そのため mmap 領域は L4[2] の範囲（1TiB 以降）に配置して衝突を回避する。
const MMAP_VADDR_BASE: u64 = 0x100_0000_0000; // 1 TiB
/// mmap 領域の上限。
const MMAP_VADDR_LIMIT: u64 = 0x200_0000_0000; // 2 TiB

/// SYS_MMAP: ユーザー空間に匿名ページをマッピングする。
///
/// 引数:
/// - arg1 (addr_hint): マッピング先仮想アドレスのヒント（0 ならカーネルが決定）
/// - arg2 (len): マッピングサイズ（バイト、4KiB にアラインされる）
/// - arg3 (prot): プロテクションフラグ（PROT_READ | PROT_WRITE）
/// - arg4 (flags): マッピングフラグ（MAP_ANONYMOUS のみ対応）
///
/// 戻り値: マッピングされた仮想アドレス
fn sys_mmap(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    let addr_hint = arg1;
    let len = arg2;
    let prot = arg3;
    let flags = arg4;

    // len が 0 はエラー
    if len == 0 {
        return Err(SyscallError::InvalidArgument);
    }

    // 現状は匿名マッピングのみ対応
    if (flags & MMAP_FLAG_ANONYMOUS) == 0 {
        return Err(SyscallError::NotSupported);
    }

    // prot の検証（最低限 READ は必要）
    if (prot & MMAP_PROT_READ) == 0 {
        return Err(SyscallError::InvalidArgument);
    }

    let writable = (prot & MMAP_PROT_WRITE) != 0;

    // ページ数を計算（切り上げ）
    let num_pages = ((len + 4095) / 4096) as usize;

    // 現在のプロセスの L4 ページテーブルフレームを取得
    let l4_frame = crate::scheduler::current_task_page_table_frame()
        .ok_or(SyscallError::NotSupported)?; // カーネルタスクでは mmap 不可

    // マッピング先の仮想アドレスを決定する
    let virt_addr = if addr_hint != 0 {
        // ユーザーが指定したアドレスを使う（4KiB アラインに切り上げ）
        let aligned = (addr_hint + 4095) & !4095;
        if aligned < MMAP_VADDR_BASE || aligned + (num_pages as u64 * 4096) > MMAP_VADDR_LIMIT {
            return Err(SyscallError::InvalidAddress);
        }
        aligned
    } else {
        // カーネルが空き領域を探す
        find_free_mmap_region(l4_frame, num_pages)?
    };

    // ページをマッピング
    let allocated = crate::paging::map_anonymous_pages_in_process(
        l4_frame,
        x86_64::VirtAddr::new(virt_addr),
        num_pages,
        writable,
    );

    // 確保したフレームをプロセスの allocated_frames に追加
    // （プロセス終了時に自動で解放される）
    crate::scheduler::add_mmap_frames_to_current(&allocated);

    Ok(virt_addr)
}

/// SYS_MUNMAP: ユーザー空間のページマッピングを解除する。
///
/// 引数:
/// - arg1 (addr): マッピング解除する仮想アドレス（4KiB アライン必須）
/// - arg2 (len): 解除するサイズ（バイト）
///
/// 戻り値: 0（成功）
fn sys_munmap(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let addr = arg1;
    let len = arg2;

    // アドレスが 4KiB アラインされているか確認
    if (addr & 0xFFF) != 0 {
        return Err(SyscallError::MisalignedPointer);
    }

    if len == 0 {
        return Err(SyscallError::InvalidArgument);
    }

    // mmap 領域の範囲チェック
    if addr < MMAP_VADDR_BASE || addr + len > MMAP_VADDR_LIMIT {
        return Err(SyscallError::InvalidAddress);
    }

    let num_pages = ((len + 4095) / 4096) as usize;

    let l4_frame = crate::scheduler::current_task_page_table_frame()
        .ok_or(SyscallError::NotSupported)?;

    // ページのマッピングを解除し、物理フレームを解放
    let freed = crate::paging::unmap_pages_in_process(
        l4_frame,
        x86_64::VirtAddr::new(addr),
        num_pages,
    );

    // プロセスの allocated_frames から削除
    crate::scheduler::remove_mmap_frames_from_current(&freed);

    Ok(0)
}

/// mmap 領域から空き仮想アドレスを探す。
///
/// MMAP_VADDR_BASE から MMAP_VADDR_LIMIT の間で、num_pages 分の連続した
/// 未マッピング領域を探す。単純な線形探索（first-fit）。
fn find_free_mmap_region(
    process_l4_frame: x86_64::structures::paging::PhysFrame<x86_64::structures::paging::Size4KiB>,
    num_pages: usize,
) -> Result<u64, SyscallError> {
    let required_bytes = num_pages as u64 * 4096;

    let process_l4: &x86_64::structures::paging::page_table::PageTable = unsafe {
        &*(process_l4_frame.start_address().as_u64()
            as *const x86_64::structures::paging::page_table::PageTable)
    };

    // MMAP_VADDR_BASE からページ単位で空きを探す
    let mut candidate = MMAP_VADDR_BASE;

    while candidate + required_bytes <= MMAP_VADDR_LIMIT {
        let mut all_free = true;

        for page_idx in 0..num_pages {
            let addr = candidate + (page_idx as u64) * 4096;
            if is_page_mapped(process_l4, addr) {
                // この位置は使用中 → 次の候補に進む
                candidate = addr + 4096;
                all_free = false;
                break;
            }
        }

        if all_free {
            return Ok(candidate);
        }
    }

    // 空き領域が見つからなかった
    Err(SyscallError::Other)
}

/// 指定した仮想アドレスがプロセスのページテーブルでマッピング済みかチェックする。
fn is_page_mapped(
    l4_table: &x86_64::structures::paging::page_table::PageTable,
    virt_addr: u64,
) -> bool {
    use x86_64::structures::paging::PageTableFlags;

    let l4_idx = ((virt_addr >> 39) & 0x1FF) as usize;
    let l3_idx = ((virt_addr >> 30) & 0x1FF) as usize;
    let l2_idx = ((virt_addr >> 21) & 0x1FF) as usize;
    let l1_idx = ((virt_addr >> 12) & 0x1FF) as usize;

    let l4_entry = &l4_table[l4_idx];
    if l4_entry.is_unused() {
        return false;
    }

    let l3_table: &x86_64::structures::paging::page_table::PageTable = unsafe {
        &*(l4_entry.addr().as_u64()
            as *const x86_64::structures::paging::page_table::PageTable)
    };

    let l3_entry = &l3_table[l3_idx];
    if l3_entry.is_unused() {
        return false;
    }
    if l3_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        return true; // 1GiB ページ: マッピング済み
    }

    let l2_table: &x86_64::structures::paging::page_table::PageTable = unsafe {
        &*(l3_entry.addr().as_u64()
            as *const x86_64::structures::paging::page_table::PageTable)
    };

    let l2_entry = &l2_table[l2_idx];
    if l2_entry.is_unused() {
        return false;
    }
    if l2_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        return true; // 2MiB ページ: マッピング済み
    }

    let l1_table: &x86_64::structures::paging::page_table::PageTable = unsafe {
        &*(l2_entry.addr().as_u64()
            as *const x86_64::structures::paging::page_table::PageTable)
    };

    let l1_entry = &l1_table[l1_idx];
    !l1_entry.is_unused()
}

// =================================================================
// サウンド関連
// =================================================================

/// SYS_SOUND_PLAY: AC97 ドライバで正弦波ビープ音を再生する。
///
/// # 引数
/// - arg1 (freq_hz): 周波数 (Hz)。1〜20000 の範囲。
/// - arg2 (duration_ms): 持続時間 (ミリ秒)。1〜10000 の範囲。
///
/// # 戻り値
/// - 0: 成功
/// - エラー: InvalidArgument (範囲外), NotSupported (AC97 未検出)
fn sys_sound_play(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let freq_hz = arg1 as u32;
    let duration_ms = arg2 as u32;

    // 引数の範囲チェック
    if freq_hz == 0 || freq_hz > 20000 {
        return Err(SyscallError::InvalidArgument);
    }
    if duration_ms == 0 || duration_ms > 10000 {
        return Err(SyscallError::InvalidArgument);
    }

    // AC97 ドライバを取得して再生
    let mut ac97 = crate::ac97::AC97.lock();
    match ac97.as_mut() {
        Some(driver) => {
            driver.play_tone(freq_hz, duration_ms);
            Ok(0)
        }
        None => Err(SyscallError::NotSupported),
    }
}

/// SYS_THREAD_CREATE: 同一プロセス内で新しいスレッドを作成する
///
/// 引数:
///   arg1 — スレッドのエントリポイント（ユーザー空間アドレス）
///   arg2 — スレッド用ユーザースタックのトップ（mmap で確保済み）
///   arg3 — スレッドに渡す引数（rdi レジスタにセット）
///
/// 戻り値:
///   スレッドのタスク ID
fn sys_thread_create(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    let entry_point = arg1;
    let stack_top = arg2;
    let arg = arg3;

    match crate::scheduler::spawn_thread(entry_point, stack_top, arg) {
        Ok(thread_id) => Ok(thread_id),
        Err(_e) => Err(SyscallError::InvalidArgument),
    }
}

/// SYS_THREAD_EXIT: 現在のスレッドを終了する
///
/// 引数:
///   arg1 — 終了コード
///
/// スレッドの終了処理。プロセスリーダーの exit とは異なり、
/// アドレス空間（CR3）の破棄は行わない。
fn sys_thread_exit(arg1: u64) -> Result<u64, SyscallError> {
    let exit_code = arg1 as i32;
    crate::scheduler::set_exit_code(exit_code);
    // exit_usermode() でカーネルモードに戻り、
    // thread_exit_handler または user_task_exit_handler に流れる
    crate::usermode::exit_usermode();
}

/// SYS_THREAD_JOIN: スレッドの終了を待つ
///
/// 引数:
///   arg1 — 待つスレッドのタスク ID
///   arg2 — タイムアウト (ms)。0 なら無期限待ち。
///
/// 戻り値:
///   スレッドの終了コード
fn sys_thread_join(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let thread_id = arg1;
    let timeout_ms = arg2;

    match crate::scheduler::wait_for_thread(thread_id, timeout_ms) {
        Ok(exit_code) => Ok(exit_code as u64),
        Err(crate::scheduler::WaitError::NoChild) => Err(SyscallError::InvalidArgument),
        Err(crate::scheduler::WaitError::NotChild) => Err(SyscallError::PermissionDenied),
        Err(crate::scheduler::WaitError::Timeout) => Err(SyscallError::Timeout),
    }
}

/// SYS_FUTEX: Futex 操作（ユーザー空間同期プリミティブの基盤）
///
/// 引数:
///   arg1 — ユーザー空間の AtomicU32 のアドレス
///   arg2 — 操作コード（0: FUTEX_WAIT, 1: FUTEX_WAKE）
///   arg3 — WAIT 時: expected 値 / WAKE 時: 起床させる最大タスク数
///   arg4 — WAIT 時: タイムアウト (ms, 0 = 無期限) / WAKE 時: 未使用
///
/// 戻り値:
///   WAIT: 0（起床した）/ エラー（値が不一致で即リターン）
///   WAKE: 起床したタスクの数
fn sys_futex(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    let addr = arg1;
    let op = arg2;
    let val = arg3 as u32;

    match op {
        crate::futex::FUTEX_WAIT => {
            let timeout_ms = arg4;
            crate::futex::futex_wait(addr, val, timeout_ms)
        }
        crate::futex::FUTEX_WAKE => {
            crate::futex::futex_wake(addr, val)
        }
        _ => Err(SyscallError::InvalidArgument),
    }
}
