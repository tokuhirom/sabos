// syscall/console.rs — コンソール入出力関連システムコール
//
// SYS_READ, SYS_WRITE, SYS_KEY_READ, SYS_CONSOLE_GRAB,
// SYS_CLEAR_SCREEN, SYS_PIPE, SYS_SPAWN_REDIRECTED

use alloc::string::String;
use alloc::vec::Vec;
use crate::user_ptr::SyscallError;
use x86_64::registers::control::Cr3;
use super::{user_slice_from_args, user_ptr_from_arg};
use super::process::parse_args_buffer;

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
pub(crate) fn sys_read(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
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
pub(crate) fn sys_key_read(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
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
pub(crate) fn sys_console_grab(arg1: u64) -> Result<u64, SyscallError> {
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
pub(crate) fn sys_write(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
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
pub(crate) fn sys_pipe(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
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
pub(crate) fn sys_spawn_redirected(arg1: u64) -> Result<u64, SyscallError> {
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
    for a in &extra_args {
        args_vec.push(a.as_str());
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

    // 子プロセス用にハンドルを複製する。
    // 親と子が同じハンドルテーブルエントリを共有すると、
    // 一方が close したときにもう一方も無効になってしまう。
    // パイプの場合は writer の参照カウントも正しくインクリメントされる。
    let child_stdin = match stdin_handle {
        Some(ref h) => Some(crate::handle::duplicate_handle(h)?),
        None => None,
    };
    let child_stdout = match stdout_handle {
        Some(ref h) => Some(crate::handle::duplicate_handle(h)?),
        None => None,
    };

    // スケジューラにユーザープロセスとして登録（カーネルのページテーブルで実行）
    let (current_cr3, current_flags) = Cr3::read();
    unsafe {
        crate::paging::switch_to_kernel_page_table();
    }
    let task_id = match crate::scheduler::spawn_user_redirected(
        &process_name, &elf_data, &args_vec, child_stdin, child_stdout,
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
pub(crate) fn sys_clear_screen() -> Result<u64, SyscallError> {
    crate::framebuffer::clear_global_screen();
    Ok(0)
}
