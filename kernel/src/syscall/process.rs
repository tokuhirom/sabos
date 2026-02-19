// syscall/process.rs — プロセス管理・環境変数関連システムコール
//
// SYS_EXEC/SPAWN, SYS_YIELD/SLEEP/WAIT/WAITPID/GETPID/KILL,
// SYS_GETENV/SETENV/LISTENV, exec_by_path*, exec_for_test*

use alloc::string::String;
use alloc::vec::Vec;
use crate::user_ptr::SyscallError;
use x86_64::registers::control::Cr3;
use super::{user_slice_from_args, user_ptr_from_arg};

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
pub(crate) fn sys_exec(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
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
pub(super) fn parse_args_buffer(args_ptr: u64, args_len: u64) -> Result<Vec<String>, SyscallError> {
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
pub(crate) fn sys_spawn(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
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
pub(crate) fn sys_yield() -> Result<u64, SyscallError> {
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
pub(crate) fn sys_sleep(arg1: u64) -> Result<u64, SyscallError> {
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
pub(crate) fn sys_wait(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
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
pub(crate) fn sys_waitpid(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
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
pub(crate) fn sys_getpid() -> Result<u64, SyscallError> {
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
pub(crate) fn sys_kill(arg1: u64) -> Result<u64, SyscallError> {
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
pub(crate) fn sys_getenv(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
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
pub(crate) fn sys_setenv(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
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
pub(crate) fn sys_listenv(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let buf = user_slice_from_args(arg1, arg2)?;

    // 全環境変数を "KEY=VALUE\n" 形式で取得
    let data = crate::scheduler::list_env_vars();

    if data.len() > buf.as_slice().len() {
        return Err(SyscallError::BufferOverflow);
    }

    buf.as_mut_slice()[..data.len()].copy_from_slice(data.as_bytes());
    Ok(data.len() as u64)
}
